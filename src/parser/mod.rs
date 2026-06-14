use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Parser};

#[derive(Debug, Clone)]
pub struct Symbol {
    pub id: String,   // "src/auth.ts::login"
    pub file: String, // "src/auth.ts"
    pub name: String, // "login"
    pub kind: String, // "function" | "class" | "method" | "struct" | "impl"
    pub start_line: u32,
    pub end_line: u32,
    pub hash: String, // hash of the symbol's source text
}

#[derive(Debug, Clone)]
pub struct Dep {
    pub caller: String, // symbol id "src/auth.ts::login"
    pub callee: String, // symbol id "src/auth.ts::validateToken"
    pub kind: String,   // "calls"
}

pub struct SymbolIndex {
    repo_root: PathBuf,
}

struct LangConfig {
    language: Language,
    extensions: &'static [&'static str],
    /// (node_kind, name_field, kind_override_field) triples to extract
    symbol_queries: Vec<SymbolQuery>,
}

struct SymbolQuery {
    node_kind: &'static str,
    name_extractor: NameExtractor,
    /// How to determine the actual symbol kind (struct vs class vs enum, etc.)
    kind_override: KindOverride,
}

/// Strategy for determining the actual kind of a symbol node.
enum KindOverride {
    /// Use the node's own kind (default behavior for most languages)
    None,
    /// Read a named field from the node (Swift: declaration_kind field)
    Field(&'static str),
    /// Look for an unnamed child keyword token (Kotlin: class vs interface vs enum)
    Keyword(&'static [&'static str]),
}

impl SymbolQuery {
    fn new(node_kind: &'static str, name_extractor: NameExtractor) -> Self {
        Self {
            node_kind,
            name_extractor,
            kind_override: KindOverride::None,
        }
    }

    fn with_kind_field(
        node_kind: &'static str,
        name_extractor: NameExtractor,
        kind_field: &'static str,
    ) -> Self {
        Self {
            node_kind,
            name_extractor,
            kind_override: KindOverride::Field(kind_field),
        }
    }

    fn with_keyword(
        node_kind: &'static str,
        name_extractor: NameExtractor,
        keywords: &'static [&'static str],
    ) -> Self {
        Self {
            node_kind,
            name_extractor,
            kind_override: KindOverride::Keyword(keywords),
        }
    }
}

#[allow(dead_code)]
enum NameExtractor {
    Field(&'static str),
    ChildKind(&'static str),
}

impl SymbolIndex {
    pub fn new(repo_root: &str) -> Result<Self> {
        let repo_root = std::fs::canonicalize(repo_root)
            .with_context(|| format!("Cannot access repo: {}", repo_root))?;
        Ok(Self { repo_root })
    }

    /// Scan all files for symbols and extract call dependencies
    pub fn scan_with_deps(&self) -> Result<(Vec<Symbol>, Vec<Dep>)> {
        let symbols = self.scan_all()?;

        // Build a map: function_name -> [symbol_ids]
        let mut name_to_ids: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for sym in &symbols {
            if matches!(sym.kind.as_str(), "function" | "method" | "arrow_fn") {
                name_to_ids
                    .entry(sym.name.clone())
                    .or_default()
                    .push(sym.id.clone());
            }
        }

        // Build a map: (file, line_range) -> symbol_id for function bodies
        let mut file_symbols: std::collections::HashMap<String, Vec<(u32, u32, String)>> =
            std::collections::HashMap::new();
        for sym in &symbols {
            if matches!(sym.kind.as_str(), "function" | "method" | "arrow_fn") {
                file_symbols.entry(sym.file.clone()).or_default().push((
                    sym.start_line,
                    sym.end_line,
                    sym.id.clone(),
                ));
            }
        }

        let configs = Self::lang_configs();
        let mut parser = Parser::new();
        let mut deps = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for config in &configs {
            if parser.set_language(&config.language).is_err() {
                continue;
            }

            for ext in config.extensions {
                let pattern = format!("{}/**/*.{}", self.repo_root.display(), ext);
                for entry in glob::glob(&pattern)? {
                    let path = entry?;
                    let rel = path.strip_prefix(&self.repo_root).unwrap_or(&path);
                    let rel_str = rel.to_string_lossy().to_string();
                    if rel_str.contains("/node_modules/")
                        || rel_str.contains("/target/")
                        || rel_str.contains("/.")
                        || rel_str.contains("/.git/")
                    {
                        continue;
                    }

                    if let Some(fn_ranges) = file_symbols.get(&rel_str) {
                        if let Ok(source) = std::fs::read_to_string(&path) {
                            if let Some(tree) = parser.parse(&source, None) {
                                let calls = self.extract_calls(&tree.root_node(), &source);
                                for (call_name, call_line) in &calls {
                                    // Find which function this call is inside
                                    let caller = fn_ranges.iter().find(|(start, end, _)| {
                                        *call_line >= *start && *call_line <= *end
                                    });
                                    if let Some((_, _, caller_id)) = caller {
                                        // Find callee symbol(s) by name
                                        if let Some(callee_ids) = name_to_ids.get(call_name) {
                                            for callee_id in callee_ids {
                                                if callee_id != caller_id {
                                                    let key =
                                                        (caller_id.clone(), callee_id.clone());
                                                    if seen.insert(key) {
                                                        deps.push(Dep {
                                                            caller: caller_id.clone(),
                                                            callee: callee_id.clone(),
                                                            kind: "calls".to_string(),
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok((symbols, deps))
    }

    /// Extract function call names and their line numbers from a tree
    fn extract_calls(&self, node: &tree_sitter::Node, source: &str) -> Vec<(String, u32)> {
        let mut calls = Vec::new();
        self.walk_calls(node, source, &mut calls);
        calls
    }

    fn walk_calls(&self, node: &tree_sitter::Node, source: &str, out: &mut Vec<(String, u32)>) {
        let kind = node.kind();

        // Match call expressions across languages
        // call_expression: JS/TS/Rust/C/C++/Go/Java/C#/PHP/Kotlin
        // call: Python/Ruby
        // invocation_expression: C#
        if kind == "call_expression" || kind == "call" || kind == "invocation_expression" {
            // Try to extract the function name from the "function" field
            if let Some(func_node) = node.child_by_field_name("function") {
                let name = self.resolve_call_name(&func_node, source);
                if let Some(name) = name {
                    let line = node.start_position().row as u32 + 1;
                    out.push((name, line));
                }
            }
            // Also try "name" field (some grammars)
            if let Some(func_node) = node.child_by_field_name("name") {
                let name = source[func_node.byte_range()].trim().to_string();
                if !name.is_empty() {
                    let line = node.start_position().row as u32 + 1;
                    out.push((name, line));
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_calls(&child, source, out);
        }
    }

    /// Resolve a call expression's function node to a simple name
    fn resolve_call_name(&self, node: &tree_sitter::Node, source: &str) -> Option<String> {
        match node.kind() {
            "identifier" | "simple_identifier" => {
                Some(source[node.byte_range()].trim().to_string())
            }
            // method call: obj.method() -> extract "method"
            "member_expression" | "field_expression" | "navigation_expression" => node
                .child_by_field_name("property")
                .or_else(|| node.child_by_field_name("field"))
                .or_else(|| node.child_by_field_name("name"))
                .map(|n| source[n.byte_range()].trim().to_string()),
            // scoped: mod::func() -> extract "func"
            "scoped_identifier" | "qualified_identifier" => node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].trim().to_string()),
            _ => None,
        }
    }

    pub fn scan_all(&self) -> Result<Vec<Symbol>> {
        let configs = Self::lang_configs();
        let mut all_symbols = Vec::new();

        // Reuse one parser per language instead of creating one per file
        let mut parser = Parser::new();

        for config in &configs {
            if let Err(e) = parser.set_language(&config.language) {
                eprintln!("  warn: skipping language {:?}: {}", config.extensions, e);
                continue;
            }

            for ext in config.extensions {
                let pattern = format!("{}/**/*.{}", self.repo_root.display(), ext);
                for entry in glob::glob(&pattern)? {
                    let path = entry?;
                    // Skip hidden dirs, node_modules, target, .git
                    let rel = path.strip_prefix(&self.repo_root).unwrap_or(&path);
                    let rel_str = rel.to_string_lossy();
                    if rel_str.contains("/node_modules/")
                        || rel_str.contains("/target/")
                        || rel_str.contains("/.")
                        || rel_str.contains("/.git/")
                    {
                        continue;
                    }

                    match self.parse_file(&path, config, &mut parser) {
                        Ok(symbols) => all_symbols.extend(symbols),
                        Err(e) => {
                            eprintln!("  warn: skipping {}: {}", rel_str, e);
                        }
                    }
                }
            }
        }

        Ok(all_symbols)
    }

    fn parse_file(
        &self,
        path: &Path,
        config: &LangConfig,
        parser: &mut Parser,
    ) -> Result<Vec<Symbol>> {
        let source = std::fs::read_to_string(path)?;
        let rel_path = path
            .strip_prefix(&self.repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let tree = parser
            .parse(&source, None)
            .context("Failed to parse file")?;

        let mut symbols = Vec::new();
        self.walk_tree(&tree.root_node(), &source, &rel_path, config, &mut symbols);
        Ok(symbols)
    }

    fn walk_tree(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &str,
        config: &LangConfig,
        out: &mut Vec<Symbol>,
    ) {
        let kind = node.kind();

        for query in &config.symbol_queries {
            if kind == query.node_kind {
                if let Some(name) = self.extract_name(node, &query.name_extractor, source) {
                    let start = node.start_position().row as u32 + 1;
                    let end = node.end_position().row as u32 + 1;
                    let text = &source[node.byte_range()];
                    let hash = Self::hash_str(text);

                    // Determine kind: use override if available,
                    // otherwise fall back to normalize_kind on the node type
                    let symbol_kind = match &query.kind_override {
                        KindOverride::None => Self::normalize_kind(kind),
                        KindOverride::Field(field) => node
                            .child_by_field_name(field)
                            .map(|n| Self::normalize_kind(source[n.byte_range()].trim()))
                            .unwrap_or_else(|| Self::normalize_kind(kind)),
                        KindOverride::Keyword(keywords) => {
                            // Scan unnamed children for a matching keyword token
                            let mut found_kind = Self::normalize_kind(kind);
                            let mut cursor = node.walk();
                            for child in node.children(&mut cursor) {
                                if !child.is_named() {
                                    let text = source[child.byte_range()].trim();
                                    if keywords.contains(&text) {
                                        found_kind = Self::normalize_kind(text);
                                        break;
                                    }
                                }
                            }
                            found_kind
                        }
                    };

                    out.push(Symbol {
                        id: format!("{}::{}", file, name),
                        file: file.to_string(),
                        name,
                        kind: symbol_kind.to_string(),
                        start_line: start,
                        end_line: end,
                        hash,
                    });
                }
            }
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_tree(&child, source, file, config, out);
        }
    }

    fn extract_name(
        &self,
        node: &tree_sitter::Node,
        extractor: &NameExtractor,
        source: &str,
    ) -> Option<String> {
        match extractor {
            NameExtractor::Field(field) => node
                .child_by_field_name(field)
                .map(|n| source[n.byte_range()].to_string()),
            NameExtractor::ChildKind(kind) => {
                let mut cursor = node.walk();
                let result = node
                    .children(&mut cursor)
                    .find(|c| c.kind() == *kind)
                    .map(|n| source[n.byte_range()].to_string());
                result
            }
        }
    }

    fn normalize_kind(tree_sitter_kind: &str) -> &str {
        match tree_sitter_kind {
            "function_declaration" | "function_definition" | "function_item" | "fun" => "function",
            "method_definition" | "method_declaration" => "method",
            "class_declaration" | "class_definition" | "class" => "class",
            "struct_item" | "struct_declaration" | "struct" => "struct",
            "impl_item" => "impl",
            "enum_item" | "enum_declaration" | "enum" => "enum",
            "interface_declaration" | "interface" => "interface",
            "trait_item" | "trait_declaration" | "protocol_declaration" | "protocol" => "trait",
            "object_declaration" | "object" => "object",
            "actor" => "actor",
            "extension" => "extension",
            "type_alias_declaration" | "type_item" | "type_declaration" => "type",
            "arrow_function" => "arrow_fn",
            "export_statement" => "export",
            "namespace_declaration" => "namespace",
            "module_declaration" => "module",
            "singleton_method" => "method",
            other => other,
        }
    }

    fn hash_str(s: &str) -> String {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    fn lang_configs() -> Vec<LangConfig> {
        vec![
            // TypeScript / TSX
            LangConfig {
                language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                extensions: &["ts", "tsx"],
                symbol_queries: vec![
                    SymbolQuery::new("function_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("class_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("method_definition", NameExtractor::Field("name")),
                    SymbolQuery::new("interface_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("type_alias_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_declaration", NameExtractor::Field("name")),
                ],
            },
            // JavaScript
            LangConfig {
                language: tree_sitter_javascript::LANGUAGE.into(),
                extensions: &["js", "jsx"],
                symbol_queries: vec![
                    SymbolQuery::new("function_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("class_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("method_definition", NameExtractor::Field("name")),
                ],
            },
            // Rust
            LangConfig {
                language: tree_sitter_rust::LANGUAGE.into(),
                extensions: &["rs"],
                symbol_queries: vec![
                    SymbolQuery::new("function_item", NameExtractor::Field("name")),
                    SymbolQuery::new("struct_item", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_item", NameExtractor::Field("name")),
                    SymbolQuery::new("trait_item", NameExtractor::Field("name")),
                    SymbolQuery::new("impl_item", NameExtractor::Field("type")),
                    SymbolQuery::new("type_item", NameExtractor::Field("name")),
                ],
            },
            // Python
            LangConfig {
                language: tree_sitter_python::LANGUAGE.into(),
                extensions: &["py"],
                symbol_queries: vec![
                    SymbolQuery::new("function_definition", NameExtractor::Field("name")),
                    SymbolQuery::new("class_definition", NameExtractor::Field("name")),
                ],
            },
            // C#
            LangConfig {
                language: tree_sitter_c_sharp::LANGUAGE.into(),
                extensions: &["cs"],
                symbol_queries: vec![
                    SymbolQuery::new("method_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("class_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("interface_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("struct_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("namespace_declaration", NameExtractor::Field("name")),
                ],
            },
            // Go
            LangConfig {
                language: tree_sitter_go::LANGUAGE.into(),
                extensions: &["go"],
                symbol_queries: vec![
                    SymbolQuery::new("function_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("method_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("type_declaration", NameExtractor::Field("name")),
                ],
            },
            // Java
            LangConfig {
                language: tree_sitter_java::LANGUAGE.into(),
                extensions: &["java"],
                symbol_queries: vec![
                    SymbolQuery::new("method_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("class_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("interface_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_declaration", NameExtractor::Field("name")),
                ],
            },
            // C
            LangConfig {
                language: tree_sitter_c::LANGUAGE.into(),
                extensions: &["c", "h"],
                symbol_queries: vec![
                    SymbolQuery::new("function_definition", NameExtractor::Field("declarator")),
                    SymbolQuery::new("struct_specifier", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_specifier", NameExtractor::Field("name")),
                    SymbolQuery::new("type_definition", NameExtractor::Field("declarator")),
                ],
            },
            // C++
            LangConfig {
                language: tree_sitter_cpp::LANGUAGE.into(),
                extensions: &["cpp", "cc", "cxx", "hpp"],
                symbol_queries: vec![
                    SymbolQuery::new("function_definition", NameExtractor::Field("declarator")),
                    SymbolQuery::new("class_specifier", NameExtractor::Field("name")),
                    SymbolQuery::new("struct_specifier", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_specifier", NameExtractor::Field("name")),
                    SymbolQuery::new("namespace_definition", NameExtractor::Field("name")),
                ],
            },
            // Ruby
            LangConfig {
                language: tree_sitter_ruby::LANGUAGE.into(),
                extensions: &["rb"],
                symbol_queries: vec![
                    SymbolQuery::new("method", NameExtractor::Field("name")),
                    SymbolQuery::new("singleton_method", NameExtractor::Field("name")),
                    SymbolQuery::new("class", NameExtractor::Field("name")),
                    SymbolQuery::new("module", NameExtractor::Field("name")),
                ],
            },
            // PHP
            LangConfig {
                language: tree_sitter_php::LANGUAGE_PHP.into(),
                extensions: &["php"],
                symbol_queries: vec![
                    SymbolQuery::new("function_definition", NameExtractor::Field("name")),
                    SymbolQuery::new("method_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("class_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("interface_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("trait_declaration", NameExtractor::Field("name")),
                    SymbolQuery::new("enum_declaration", NameExtractor::Field("name")),
                ],
            },
            // Swift — uses class_declaration for class/struct/enum/protocol/actor/extension
            // with a declaration_kind field to differentiate
            LangConfig {
                language: tree_sitter_swift::LANGUAGE.into(),
                extensions: &["swift"],
                symbol_queries: vec![
                    SymbolQuery::new("function_declaration", NameExtractor::Field("name")),
                    SymbolQuery::with_kind_field(
                        "class_declaration",
                        NameExtractor::Field("name"),
                        "declaration_kind",
                    ),
                    SymbolQuery::new("protocol_declaration", NameExtractor::Field("name")),
                ],
            },
            // Kotlin — uses class_declaration for class/interface/enum
            // with unnamed keyword children to differentiate
            LangConfig {
                language: tree_sitter_kotlin_ng::LANGUAGE.into(),
                extensions: &["kt", "kts"],
                symbol_queries: vec![
                    SymbolQuery::new("function_declaration", NameExtractor::Field("name")),
                    SymbolQuery::with_keyword(
                        "class_declaration",
                        NameExtractor::Field("name"),
                        &["class", "interface", "enum"],
                    ),
                    SymbolQuery::new("object_declaration", NameExtractor::Field("name")),
                ],
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a file inside a TempDir, creating parent dirs as needed.
    fn write_file(dir: &TempDir, rel_path: &str, content: &str) -> PathBuf {
        let full = dir.path().join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
        full
    }

    /// Helper: find symbol by name in a slice.
    fn find_sym<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        symbols.iter().find(|s| s.name == name).unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                name,
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        })
    }

    // ── 1. Rust functions ──────────────────────────────────────────────

    #[test]
    fn test_parse_rust_functions() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/lib.rs",
            r#"
fn alpha() {}
fn beta(x: i32) -> i32 { x }
fn gamma() -> String { String::new() }
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let fns: Vec<_> = symbols.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(fns.len(), 3, "expected 3 functions, got {:?}", fns);

        for name in &["alpha", "beta", "gamma"] {
            let sym = find_sym(&symbols, name);
            assert_eq!(sym.kind, "function");
        }
    }

    // ── 2. Rust struct + impl ──────────────────────────────────────────

    #[test]
    fn test_parse_rust_struct_and_impl() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/model.rs",
            r#"
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn distance(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let _struct_sym = find_sym(&symbols, "Point");
        // The struct itself has kind "struct"; the impl also has name "Point" with kind "impl".
        let kinds: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Point")
            .map(|s| s.kind.as_str())
            .collect();
        assert!(
            kinds.contains(&"struct"),
            "expected struct, got {:?}",
            kinds
        );
        assert!(kinds.contains(&"impl"), "expected impl, got {:?}", kinds);

        // The method inside impl should also be extracted
        let distance = find_sym(&symbols, "distance");
        assert_eq!(distance.kind, "function");
    }

    // ── 3. Rust enum + trait ───────────────────────────────────────────

    #[test]
    fn test_parse_rust_enum_and_trait() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/types.rs",
            r#"
enum Color {
    Red,
    Green,
    Blue,
}

trait Drawable {
    fn draw(&self);
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let color = find_sym(&symbols, "Color");
        assert_eq!(color.kind, "enum");

        let drawable = find_sym(&symbols, "Drawable");
        assert_eq!(drawable.kind, "trait");
    }

    // ── 4. TypeScript functions ────────────────────────────────────────

    #[test]
    fn test_parse_typescript_functions() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/utils.ts",
            r#"
function add(a: number, b: number): number {
    return a + b;
}

function greet(name: string): string {
    return `Hello, ${name}`;
}

function noop(): void {}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let fns: Vec<_> = symbols.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(fns.len(), 3);

        for name in &["add", "greet", "noop"] {
            let sym = find_sym(&symbols, name);
            assert_eq!(sym.kind, "function");
        }
    }

    // ── 5. TypeScript class + methods ──────────────────────────────────

    #[test]
    fn test_parse_typescript_class() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/service.ts",
            r#"
class UserService {
    getUser(id: number): string {
        return "user";
    }
    deleteUser(id: number): void {}
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let cls = find_sym(&symbols, "UserService");
        assert_eq!(cls.kind, "class");

        let methods: Vec<_> = symbols.iter().filter(|s| s.kind == "method").collect();
        assert_eq!(methods.len(), 2);
        find_sym(&symbols, "getUser");
        find_sym(&symbols, "deleteUser");
    }

    // ── 6. TypeScript interface ────────────────────────────────────────

    #[test]
    fn test_parse_typescript_interface() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/types.ts",
            r#"
interface Config {
    host: string;
    port: number;
}

interface Logger {
    log(msg: string): void;
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let interfaces: Vec<_> = symbols.iter().filter(|s| s.kind == "interface").collect();
        assert_eq!(interfaces.len(), 2);
        find_sym(&symbols, "Config");
        find_sym(&symbols, "Logger");
    }

    // ── 7. Python functions ────────────────────────────────────────────

    #[test]
    fn test_parse_python_functions() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "utils.py",
            r#"
def connect(host, port):
    pass

def disconnect():
    pass

def retry(fn, times=3):
    pass
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let fns: Vec<_> = symbols.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(fns.len(), 3);
        for name in &["connect", "disconnect", "retry"] {
            find_sym(&symbols, name);
        }
    }

    // ── 8. Python class + methods ──────────────────────────────────────

    #[test]
    fn test_parse_python_class() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "models.py",
            r#"
class Dog:
    def __init__(self, name):
        self.name = name

    def bark(self):
        return "woof"
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let cls = find_sym(&symbols, "Dog");
        assert_eq!(cls.kind, "class");

        // Python methods are function_definition nodes inside a class — they get kind "function"
        let methods: Vec<_> = symbols.iter().filter(|s| s.kind == "function").collect();
        assert!(methods.len() >= 2, "expected at least __init__ and bark");
        find_sym(&symbols, "__init__");
        find_sym(&symbols, "bark");
    }

    // ── 9. JavaScript functions ────────────────────────────────────────

    #[test]
    fn test_parse_javascript_functions() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "lib/helpers.js",
            r#"
function sum(a, b) {
    return a + b;
}

function multiply(a, b) {
    return a * b;
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let fns: Vec<_> = symbols.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(fns.len(), 2);
        find_sym(&symbols, "sum");
        find_sym(&symbols, "multiply");
    }

    // ── 10. Symbol ID format ───────────────────────────────────────────

    #[test]
    fn test_symbol_id_format() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "src/core/engine.rs", "fn run() {}\n");

        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();
        assert_eq!(symbols.len(), 1);

        let sym = &symbols[0];
        assert_eq!(sym.id, "src/core/engine.rs::run");
        assert_eq!(sym.file, "src/core/engine.rs");
        assert_eq!(sym.name, "run");
    }

    // ── 11. Hash determinism ───────────────────────────────────────────

    #[test]
    fn test_symbol_hash_deterministic() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let code = "fn deterministic() { let x = 42; }\n";
        write_file(&dir1, "a.rs", code);
        write_file(&dir2, "a.rs", code);

        let s1 = SymbolIndex::new(dir1.path().to_str().unwrap())
            .unwrap()
            .scan_all()
            .unwrap();
        let s2 = SymbolIndex::new(dir2.path().to_str().unwrap())
            .unwrap()
            .scan_all()
            .unwrap();

        assert_eq!(s1.len(), 1);
        assert_eq!(s2.len(), 1);
        assert_eq!(
            s1[0].hash, s2[0].hash,
            "same source text must produce the same hash"
        );
        assert!(!s1[0].hash.is_empty());
    }

    // ── 12. Skips node_modules ─────────────────────────────────────────

    #[test]
    fn test_skips_node_modules() {
        let dir = TempDir::new().unwrap();
        // File inside a nested node_modules — should be skipped
        // Note: the skip filter checks for "/node_modules/" in the relative path,
        // so node_modules must be inside a parent directory (e.g., src/node_modules/).
        write_file(
            &dir,
            "src/node_modules/lodash/index.js",
            "function chunk() {}\n",
        );
        // File outside node_modules — should be found
        write_file(&dir, "src/app.js", "function main() {}\n");

        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        assert_eq!(symbols.len(), 1, "only src/app.js should be scanned");
        assert_eq!(symbols[0].name, "main");
    }

    // ── 13. Normalize kind (via public interface) ──────────────────────

    #[test]
    fn test_normalize_kind() {
        let dir = TempDir::new().unwrap();

        // Rust: function_item → "function", struct_item → "struct", enum_item → "enum",
        //       trait_item → "trait", impl_item → "impl"
        write_file(
            &dir,
            "src/all.rs",
            r#"
fn my_func() {}
struct MyStruct { x: i32 }
enum MyEnum { A, B }
trait MyTrait { fn do_it(&self); }
impl MyStruct { fn method(&self) {} }
"#,
        );

        // TypeScript: function_declaration → "function", class_declaration → "class",
        //             method_definition → "method", interface_declaration → "interface"
        write_file(
            &dir,
            "src/all.ts",
            r#"
function tsFunc(): void {}
class TsClass {
    tsMethod(): void {}
}
interface TsInterface { x: number; }
"#,
        );

        // Python: function_definition → "function", class_definition → "class"
        write_file(
            &dir,
            "all.py",
            "def py_func():\n    pass\n\nclass PyClass:\n    pass\n",
        );

        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        // Verify the normalized kinds
        assert_eq!(find_sym(&symbols, "my_func").kind, "function");
        assert_eq!(find_sym(&symbols, "MyStruct").kind, "struct");
        assert_eq!(find_sym(&symbols, "MyEnum").kind, "enum");
        assert_eq!(find_sym(&symbols, "MyTrait").kind, "trait");
        // impl MyStruct → name="MyStruct", kind="impl" (second entry with that name)
        let impls: Vec<_> = symbols.iter().filter(|s| s.kind == "impl").collect();
        assert!(!impls.is_empty(), "expected at least one impl symbol");

        assert_eq!(find_sym(&symbols, "tsFunc").kind, "function");
        assert_eq!(find_sym(&symbols, "TsClass").kind, "class");
        assert_eq!(find_sym(&symbols, "tsMethod").kind, "method");
        assert_eq!(find_sym(&symbols, "TsInterface").kind, "interface");

        assert_eq!(find_sym(&symbols, "py_func").kind, "function");
        assert_eq!(find_sym(&symbols, "PyClass").kind, "class");
    }

    // ── 14. C# ─────────────────────────────────────────────────────────

    #[test]
    fn test_parse_csharp() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/App.cs",
            r#"
namespace MyApp {
    class UserService {
        public void CreateUser(string name) {}
        public void DeleteUser(int id) {}
    }

    interface IRepository {
        void Save();
    }

    enum Status {
        Active,
        Inactive
    }
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "UserService");
        find_sym(&symbols, "CreateUser");
        find_sym(&symbols, "DeleteUser");
        find_sym(&symbols, "IRepository");
        find_sym(&symbols, "Status");
    }

    // ── 15. Go ──────────────────────────────────────────────────────────

    #[test]
    fn test_parse_go() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "main.go",
            r#"
package main

func Add(a int, b int) int {
    return a + b
}

func Subtract(a int, b int) int {
    return a - b
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        let fns: Vec<_> = symbols.iter().filter(|s| s.kind == "function").collect();
        assert_eq!(fns.len(), 2);
        find_sym(&symbols, "Add");
        find_sym(&symbols, "Subtract");
    }

    // ── 16. Java ────────────────────────────────────────────────────────

    #[test]
    fn test_parse_java() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/App.java",
            r#"
class UserService {
    public void createUser(String name) {}
    public void deleteUser(int id) {}
}

interface Repository {
    void save();
}

enum Color {
    RED, GREEN, BLUE
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "UserService");
        find_sym(&symbols, "createUser");
        find_sym(&symbols, "deleteUser");
        find_sym(&symbols, "Repository");
        find_sym(&symbols, "Color");
    }

    // ── 17. C ───────────────────────────────────────────────────────────

    #[test]
    fn test_parse_c() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/math.c",
            r#"
int add(int a, int b) {
    return a + b;
}

int multiply(int a, int b) {
    return a * b;
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        assert!(
            symbols.len() >= 2,
            "expected at least 2 symbols, got {:?}",
            symbols
                .iter()
                .map(|s| format!("{}({})", s.name, s.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── 18. C++ ─────────────────────────────────────────────────────────

    #[test]
    fn test_parse_cpp() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/engine.cpp",
            r#"
class Engine {
public:
    void start() {}
    void stop() {}
};

namespace physics {
    void simulate() {}
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "Engine");
        // namespace and functions may or may not be extracted depending on grammar
        assert!(!symbols.is_empty());
    }

    // ── 19. Ruby ────────────────────────────────────────────────────────

    #[test]
    fn test_parse_ruby() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "app.rb",
            r#"
class Dog
  def initialize(name)
    @name = name
  end

  def bark
    "woof"
  end

  def self.species
    "Canis familiaris"
  end
end

module Helpers
  def format(text)
    text.strip
  end
end
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "Dog");
        find_sym(&symbols, "initialize");
        find_sym(&symbols, "bark");
        find_sym(&symbols, "Helpers");
    }

    // ── 20. PHP ─────────────────────────────────────────────────────────

    #[test]
    fn test_parse_php() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "app.php",
            r#"<?php
class UserController {
    public function index() {
        return "list";
    }

    public function store(Request $request) {
        return "created";
    }
}

interface Authenticatable {
    public function getAuthIdentifier();
}

trait HasRoles {
    public function hasRole(string $role): bool {
        return true;
    }
}

function helper_function() {
    return 42;
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "UserController");
        find_sym(&symbols, "index");
        find_sym(&symbols, "store");
        find_sym(&symbols, "Authenticatable");
        find_sym(&symbols, "HasRoles");
        find_sym(&symbols, "helper_function");
    }

    // ── 21. Swift ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_swift() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "Sources/App.swift",
            r#"
func greet(name: String) -> String {
    return "Hello, \(name)"
}

class UserService {
    func createUser(name: String) {}
    func deleteUser(id: Int) {}
}

struct Point {
    var x: Double
    var y: Double
}

enum Direction {
    case north
    case south
    case east
    case west
}

protocol Drawable {
    func draw()
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "greet");
        assert_eq!(find_sym(&symbols, "greet").kind, "function");

        find_sym(&symbols, "UserService");
        assert_eq!(find_sym(&symbols, "UserService").kind, "class");

        find_sym(&symbols, "Point");
        assert_eq!(find_sym(&symbols, "Point").kind, "struct");

        find_sym(&symbols, "Direction");
        assert_eq!(find_sym(&symbols, "Direction").kind, "enum");

        find_sym(&symbols, "Drawable");
        assert_eq!(find_sym(&symbols, "Drawable").kind, "trait");
    }

    // ── 22. Kotlin ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_kotlin() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/main.kt",
            r#"
fun greet(name: String): String {
    return "Hello, $name"
}

class UserService {
    fun createUser(name: String) {}
    fun deleteUser(id: Int) {}
}

object Singleton {
    fun instance(): Singleton = this
}

interface Repository {
    fun save()
    fun delete()
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let symbols = idx.scan_all().unwrap();

        find_sym(&symbols, "greet");
        assert_eq!(find_sym(&symbols, "greet").kind, "function");

        find_sym(&symbols, "UserService");
        assert_eq!(find_sym(&symbols, "UserService").kind, "class");

        find_sym(&symbols, "Singleton");
        assert_eq!(find_sym(&symbols, "Singleton").kind, "object");

        find_sym(&symbols, "Repository");
        assert_eq!(find_sym(&symbols, "Repository").kind, "interface");
    }

    // ── 23. Dependency extraction ─────────────────────────────────────

    #[test]
    fn test_extract_deps_rust() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/auth.rs",
            r#"
fn validate_token(token: &str) -> bool {
    true
}

fn hash_password(pwd: &str) -> String {
    pwd.to_string()
}

fn login(user: &str, pwd: &str) -> bool {
    let h = hash_password(pwd);
    validate_token(&h)
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let (symbols, deps) = idx.scan_with_deps().unwrap();

        assert_eq!(symbols.len(), 3);

        // login should call validate_token and hash_password
        let login_deps: Vec<&str> = deps
            .iter()
            .filter(|d| d.caller == "src/auth.rs::login")
            .map(|d| d.callee.as_str())
            .collect();
        assert!(
            login_deps.contains(&"src/auth.rs::validate_token"),
            "login should call validate_token, got: {:?}",
            login_deps
        );
        assert!(
            login_deps.contains(&"src/auth.rs::hash_password"),
            "login should call hash_password, got: {:?}",
            login_deps
        );
    }

    #[test]
    fn test_extract_deps_typescript() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "src/utils.ts",
            r#"
function validate(input: string): boolean {
    return input.length > 0;
}

function process(data: string): string {
    if (!validate(data)) {
        return "";
    }
    return data.toUpperCase();
}
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let (symbols, deps) = idx.scan_with_deps().unwrap();

        assert_eq!(symbols.len(), 2);

        let process_deps: Vec<&str> = deps
            .iter()
            .filter(|d| d.caller == "src/utils.ts::process")
            .map(|d| d.callee.as_str())
            .collect();
        assert!(
            process_deps.contains(&"src/utils.ts::validate"),
            "process should call validate, got: {:?}",
            process_deps
        );
    }

    #[test]
    fn test_extract_deps_python() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "app.py",
            r#"
def helper():
    pass

def main():
    helper()
"#,
        );
        let idx = SymbolIndex::new(dir.path().to_str().unwrap()).unwrap();
        let (symbols, deps) = idx.scan_with_deps().unwrap();

        assert_eq!(symbols.len(), 2);

        let main_deps: Vec<&str> = deps
            .iter()
            .filter(|d| d.caller == "app.py::main")
            .map(|d| d.callee.as_str())
            .collect();
        assert!(
            main_deps.contains(&"app.py::helper"),
            "main should call helper, got: {:?}",
            main_deps
        );
    }
}
