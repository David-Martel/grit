pub mod azure_store;
pub mod lock_store;
pub mod s3_store;
pub mod sqlite_store;

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::parser::{Dep, Symbol};

/// Apply standard PRAGMA settings to a new SQLite connection.
pub fn configure_connection(conn: &Connection) -> Result<()> {
    // foreign_keys is enforced at runtime (the bundled SQLite defaults it on),
    // but set it explicitly so the locks/deps -> symbols references stay
    // enforced regardless of how SQLite was built.
    match conn
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")
    {
        Ok(_) => Ok(()),
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("locked") || err_str.contains("busy") {
                anyhow::bail!(
                    "Database is locked by another process. \
                     If this persists, check for stale grit processes or remove the WAL files."
                );
            }
            anyhow::bail!("Database configuration failed: {}", e);
        }
    }
}

/// (id, file, name, kind, locked_by_agent)
pub type SymbolRow = (String, String, String, String, Option<String>);

/// (symbol_id, agent_id, intent, mode, queued_at)
pub type QueueRow = (String, String, String, String, String);

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| {
            anyhow::anyhow!(
                "Failed to open database {}: {}.\n\
                 If the file is corrupted, remove it and re-run `grit init`.",
                path.display(),
                e
            )
        })?;
        configure_connection(&conn)?;

        // Quick integrity check
        match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
            Ok(ref result) if result == "ok" => {}
            Ok(detail) => {
                anyhow::bail!(
                    "Database {} failed integrity check: {}.\n\
                     Remove it and re-run `grit init` to rebuild.",
                    path.display(),
                    detail
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "Database {} may be corrupted: {}.\n\
                     Remove it and re-run `grit init` to rebuild.",
                    path.display(),
                    e
                );
            }
        }

        Ok(Self { conn })
    }

    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS symbols (
                id          TEXT PRIMARY KEY,
                file        TEXT NOT NULL,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                start_line  INTEGER,
                end_line    INTEGER,
                hash        TEXT
            );

            CREATE TABLE IF NOT EXISTS locks (
                symbol_id   TEXT NOT NULL REFERENCES symbols(id),
                agent_id    TEXT NOT NULL,
                intent      TEXT,
                mode        TEXT DEFAULT 'write',
                locked_at   TEXT DEFAULT (datetime('now')),
                ttl_seconds INTEGER DEFAULT 600,
                PRIMARY KEY (symbol_id, agent_id)
            );

            CREATE TABLE IF NOT EXISTS deps (
                caller  TEXT NOT NULL REFERENCES symbols(id),
                callee  TEXT NOT NULL REFERENCES symbols(id),
                kind    TEXT NOT NULL,
                PRIMARY KEY (caller, callee)
            );

            CREATE TABLE IF NOT EXISTS sessions (
                name        TEXT PRIMARY KEY,
                branch      TEXT NOT NULL,
                base_branch TEXT NOT NULL,
                created_at  TEXT DEFAULT (datetime('now')),
                status      TEXT DEFAULT 'active'
            );

            CREATE TABLE IF NOT EXISTS lock_queue (
                symbol_id   TEXT NOT NULL,
                agent_id    TEXT NOT NULL,
                intent      TEXT,
                mode        TEXT DEFAULT 'write',
                queued_at   TEXT DEFAULT (datetime('now')),
                PRIMARY KEY (symbol_id, agent_id)
            );

            CREATE INDEX IF NOT EXISTS idx_locks_agent ON locks(agent_id);
            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
            CREATE INDEX IF NOT EXISTS idx_deps_callee ON deps(callee);
            CREATE INDEX IF NOT EXISTS idx_queue_symbol ON lock_queue(symbol_id);
            ",
        )?;

        // Migrate pre-ttl databases: add ttl_seconds only when it is actually
        // missing. Fresh DBs already have the column from CREATE TABLE, so the
        // old unconditional ALTER always failed and its error was swallowed —
        // detect the column and surface any real migration error instead.
        let has_ttl = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(locks)")?;
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            cols.iter().any(|c| c == "ttl_seconds")
        };
        if !has_ttl {
            self.conn
                .execute_batch("ALTER TABLE locks ADD COLUMN ttl_seconds INTEGER DEFAULT 600;")?;
        }

        Ok(())
    }

    pub fn upsert_symbols(&self, symbols: &[Symbol]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols (id, file, name, kind, start_line, end_line, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    start_line = excluded.start_line,
                    end_line = excluded.end_line,
                    hash = excluded.hash",
            )?;
            for s in symbols {
                stmt.execute(params![
                    s.id,
                    s.file,
                    s.name,
                    s.kind,
                    s.start_line,
                    s.end_line,
                    s.hash
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn available_symbols_in_files(&self, files: &[&str]) -> Result<Vec<String>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = files
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        // Join only NON-expired locks, so a symbol whose only lock has expired
        // is still reported available (an expired lock no longer occupies it).
        let sql = format!(
            "SELECT s.id FROM symbols s
             LEFT JOIN locks l ON s.id = l.symbol_id
               AND (julianday('now') - julianday(l.locked_at)) * 86400 <= COALESCE(l.ttl_seconds, 600)
             WHERE s.file IN ({}) AND l.symbol_id IS NULL
             ORDER BY s.id",
            placeholders.join(", ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = files
            .iter()
            .map(|f| f as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn count_symbols(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    pub fn list_symbols(&self, file_filter: Option<&str>) -> Result<Vec<SymbolRow>> {
        let sql = match file_filter {
            Some(_) => {
                "SELECT s.id, s.file, s.name, s.kind, l.agent_id
                        FROM symbols s LEFT JOIN locks l ON s.id = l.symbol_id
                        WHERE s.file LIKE ?1
                        ORDER BY s.file, s.start_line"
            }
            None => {
                "SELECT s.id, s.file, s.name, s.kind, l.agent_id
                        FROM symbols s LEFT JOIN locks l ON s.id = l.symbol_id
                        ORDER BY s.file, s.start_line"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut results: Vec<SymbolRow> = Vec::new();
        match file_filter {
            Some(f) => {
                let pattern = format!("%{}%", f);
                let mut rows = stmt.query(params![pattern])?;
                while let Some(row) = rows.next()? {
                    results.push((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ));
                }
            }
            None => {
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    results.push((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ));
                }
            }
        };
        Ok(results)
    }

    pub fn search_symbols(&self, keywords: &[&str]) -> Result<Vec<SymbolRow>> {
        let conditions: Vec<String> = keywords
            .iter()
            .enumerate()
            .map(|(i, _)| {
                format!(
                    "(s.name LIKE ?{0} OR s.file LIKE ?{0} OR s.id LIKE ?{0})",
                    i + 1
                )
            })
            .collect();
        let where_clause = if conditions.is_empty() {
            "1=1".to_string()
        } else {
            conditions.join(" OR ")
        };
        let sql = format!(
            "SELECT s.id, s.file, s.name, s.kind, l.agent_id
             FROM symbols s LEFT JOIN locks l ON s.id = l.symbol_id
             WHERE {}
             ORDER BY s.file, s.start_line",
            where_clause
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<String> = keywords.iter().map(|k| format!("%{}%", k)).collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let mut rows = stmt.query(param_refs.as_slice())?;
        let mut results: Vec<SymbolRow> = Vec::new();
        while let Some(row) = rows.next()? {
            results.push((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ));
        }
        Ok(results)
    }

    // ── Session management ──

    pub fn create_session(&self, name: &str, branch: &str, base_branch: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (name, branch, base_branch) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET status = 'active'",
            params![name, branch, base_branch],
        )?;
        Ok(())
    }

    pub fn get_active_session(&self) -> Result<Option<(String, String, String)>> {
        let result = self.conn.query_row(
            "SELECT name, branch, base_branch FROM sessions WHERE status = 'active' ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        match result {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn close_session(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = 'closed' WHERE name = ?1",
            params![name],
        )?;
        Ok(())
    }

    // ── Dependency management ──

    pub fn upsert_deps(&self, deps: &[Dep]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            // Clear old deps first
            tx.execute("DELETE FROM deps", [])?;
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO deps (caller, callee, kind) VALUES (?1, ?2, ?3)",
            )?;
            for d in deps {
                stmt.execute(params![d.caller, d.callee, d.kind])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Get direct callees of a symbol
    #[allow(dead_code)]
    pub fn get_deps(&self, symbol_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT callee, kind FROM deps WHERE caller = ?1")?;
        let rows = stmt.query_map(params![symbol_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get transitive callees (recursive) using WITH RECURSIVE CTE
    pub fn get_transitive_deps(&self, symbol_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE dep_tree(sym) AS (
                 SELECT callee FROM deps WHERE caller = ?1
                 UNION
                 SELECT d.callee FROM deps d JOIN dep_tree t ON d.caller = t.sym
             )
             SELECT DISTINCT sym FROM dep_tree",
        )?;
        let rows = stmt.query_map(params![symbol_id], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn count_deps(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM deps", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    // ── Queue management ──

    pub fn enqueue(&self, symbol_id: &str, agent_id: &str, intent: &str, mode: &str) -> Result<()> {
        // Use ON CONFLICT (not INSERT OR REPLACE): re-enqueuing the same agent
        // must keep its original queued_at and rowid so it does not lose its
        // FIFO position by going to the back of the line.
        self.conn.execute(
            "INSERT INTO lock_queue (symbol_id, agent_id, intent, mode) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(symbol_id, agent_id) DO UPDATE SET intent = excluded.intent, mode = excluded.mode",
            params![symbol_id, agent_id, intent, mode],
        )?;
        Ok(())
    }

    pub fn dequeue(&self, symbol_id: &str, agent_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM lock_queue WHERE symbol_id = ?1 AND agent_id = ?2",
            params![symbol_id, agent_id],
        )?;
        Ok(())
    }

    pub fn dequeue_all(&self, agent_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM lock_queue WHERE agent_id = ?1",
            params![agent_id],
        )?;
        Ok(count)
    }

    /// Next agent in queue for a symbol (FIFO by queued_at)
    pub fn next_in_queue(&self, symbol_id: &str) -> Result<Option<(String, String, String)>> {
        // rowid (monotonic insertion order) breaks queued_at ties, which are
        // common since queued_at has only second resolution.
        let result = self.conn.query_row(
            "SELECT agent_id, intent, COALESCE(mode, 'write') FROM lock_queue
             WHERE symbol_id = ?1 ORDER BY queued_at ASC, rowid ASC LIMIT 1",
            params![symbol_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Queue position for an agent on a symbol (1-based, None if not queued)
    pub fn queue_position(&self, symbol_id: &str, agent_id: &str) -> Result<Option<usize>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent_id FROM lock_queue WHERE symbol_id = ?1 ORDER BY queued_at ASC, rowid ASC"
        )?;
        let agents: Vec<String> = stmt
            .query_map(params![symbol_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(agents.iter().position(|a| a == agent_id).map(|p| p + 1))
    }

    /// List all queued entries
    pub fn list_queue(&self) -> Result<Vec<QueueRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT symbol_id, agent_id, intent, COALESCE(mode, 'write'), queued_at
             FROM lock_queue ORDER BY symbol_id, queued_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Symbol;
    use tempfile::TempDir;

    fn make_symbol(id: &str, file: &str, name: &str, kind: &str) -> Symbol {
        Symbol {
            id: id.to_string(),
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            start_line: 1,
            end_line: 10,
            hash: "abc123".to_string(),
        }
    }

    fn setup_db() -> (TempDir, Database) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        db.init_schema().unwrap();
        (tmp, db)
    }

    #[test]
    fn test_open_and_init_schema() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        assert!(db.init_schema().is_ok());
    }

    #[test]
    fn test_upsert_and_count_symbols() {
        let (_tmp, db) = setup_db();
        let symbols: Vec<Symbol> = (0..5)
            .map(|i| {
                make_symbol(
                    &format!("file.rs::fn{}", i),
                    "file.rs",
                    &format!("fn{}", i),
                    "function",
                )
            })
            .collect();
        db.upsert_symbols(&symbols).unwrap();
        assert_eq!(db.count_symbols().unwrap(), 5);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let (_tmp, db) = setup_db();
        let sym = make_symbol("file.rs::foo", "file.rs", "foo", "function");
        db.upsert_symbols(&[sym]).unwrap();
        assert_eq!(db.count_symbols().unwrap(), 1);

        // Update same symbol with different hash
        let updated = Symbol {
            id: "file.rs::foo".to_string(),
            file: "file.rs".to_string(),
            name: "foo".to_string(),
            kind: "function".to_string(),
            start_line: 5,
            end_line: 20,
            hash: "new_hash".to_string(),
        };
        db.upsert_symbols(&[updated]).unwrap();
        assert_eq!(db.count_symbols().unwrap(), 1);
    }

    #[test]
    fn test_list_symbols_no_filter() {
        let (_tmp, db) = setup_db();
        let symbols = vec![
            make_symbol("a.rs::fn1", "a.rs", "fn1", "function"),
            make_symbol("a.rs::fn2", "a.rs", "fn2", "function"),
            make_symbol("b.rs::fn3", "b.rs", "fn3", "function"),
        ];
        db.upsert_symbols(&symbols).unwrap();
        let all = db.list_symbols(None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_list_symbols_with_filter() {
        let (_tmp, db) = setup_db();
        let symbols = vec![
            make_symbol("src/a.rs::fn1", "src/a.rs", "fn1", "function"),
            make_symbol("src/a.rs::fn2", "src/a.rs", "fn2", "function"),
            make_symbol("src/b.rs::fn3", "src/b.rs", "fn3", "function"),
        ];
        db.upsert_symbols(&symbols).unwrap();
        let filtered = db.list_symbols(Some("a.rs")).unwrap();
        assert_eq!(filtered.len(), 2);
        for row in &filtered {
            assert!(row.1.contains("a.rs"));
        }
    }

    #[test]
    fn test_search_symbols() {
        let (_tmp, db) = setup_db();
        let symbols = vec![
            make_symbol("src/auth.rs::login", "src/auth.rs", "login", "function"),
            make_symbol("src/auth.rs::logout", "src/auth.rs", "logout", "function"),
            make_symbol("src/db.rs::connect", "src/db.rs", "connect", "function"),
        ];
        db.upsert_symbols(&symbols).unwrap();
        let results = db.search_symbols(&["login"]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "login");
    }

    #[test]
    fn test_available_symbols_in_files() {
        let (_tmp, db) = setup_db();
        let symbols = vec![
            make_symbol("f.rs::a", "f.rs", "a", "function"),
            make_symbol("f.rs::b", "f.rs", "b", "function"),
            make_symbol("f.rs::c", "f.rs", "c", "function"),
        ];
        db.upsert_symbols(&symbols).unwrap();

        // Lock symbol "f.rs::b"
        db.conn
            .execute(
                "INSERT INTO locks (symbol_id, agent_id, intent) VALUES (?1, ?2, ?3)",
                params!["f.rs::b", "agent-1", "editing"],
            )
            .unwrap();

        let available = db.available_symbols_in_files(&["f.rs"]).unwrap();
        assert_eq!(available.len(), 2);
        assert!(available.contains(&"f.rs::a".to_string()));
        assert!(available.contains(&"f.rs::c".to_string()));
        assert!(!available.contains(&"f.rs::b".to_string()));
    }

    #[test]
    fn test_session_lifecycle() {
        let (_tmp, db) = setup_db();
        db.create_session("sess1", "feature/x", "main").unwrap();

        let active = db.get_active_session().unwrap();
        assert!(active.is_some());
        let (name, branch, base) = active.unwrap();
        assert_eq!(name, "sess1");
        assert_eq!(branch, "feature/x");
        assert_eq!(base, "main");

        db.close_session("sess1").unwrap();
        let active = db.get_active_session().unwrap();
        assert!(active.is_none());
    }

    #[test]
    fn test_no_active_session() {
        let (_tmp, db) = setup_db();
        let active = db.get_active_session().unwrap();
        assert!(active.is_none());
    }

    #[test]
    fn test_integrity_check_on_open() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        // First open creates the file
        {
            let db = Database::open(&db_path).unwrap();
            db.init_schema().unwrap();
        }
        // Second open runs integrity check on existing DB
        let result = Database::open(&db_path);
        assert!(result.is_ok());
    }

    // ── Queue tests ──

    #[test]
    fn test_queue_enqueue_and_list() {
        let (_tmp, db) = setup_db();
        db.enqueue("sym::a", "agent-1", "want to edit", "write")
            .unwrap();
        db.enqueue("sym::a", "agent-2", "also want to edit", "write")
            .unwrap();

        let queue = db.list_queue().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].1, "agent-1");
        assert_eq!(queue[1].1, "agent-2");
    }

    #[test]
    fn test_queue_position() {
        let (_tmp, db) = setup_db();
        db.enqueue("sym::a", "agent-1", "first", "write").unwrap();
        db.enqueue("sym::a", "agent-2", "second", "write").unwrap();
        db.enqueue("sym::a", "agent-3", "third", "write").unwrap();

        assert_eq!(db.queue_position("sym::a", "agent-1").unwrap(), Some(1));
        assert_eq!(db.queue_position("sym::a", "agent-2").unwrap(), Some(2));
        assert_eq!(db.queue_position("sym::a", "agent-3").unwrap(), Some(3));
        assert_eq!(db.queue_position("sym::a", "agent-99").unwrap(), None);
    }

    #[test]
    fn test_queue_next_in_queue() {
        let (_tmp, db) = setup_db();
        db.enqueue("sym::a", "agent-1", "first", "write").unwrap();
        db.enqueue("sym::a", "agent-2", "second", "read").unwrap();

        let next = db.next_in_queue("sym::a").unwrap();
        assert!(next.is_some());
        let (agent, intent, mode) = next.unwrap();
        assert_eq!(agent, "agent-1");
        assert_eq!(intent, "first");
        assert_eq!(mode, "write");
    }

    #[test]
    fn test_queue_dequeue() {
        let (_tmp, db) = setup_db();
        db.enqueue("sym::a", "agent-1", "first", "write").unwrap();
        db.enqueue("sym::a", "agent-2", "second", "write").unwrap();

        db.dequeue("sym::a", "agent-1").unwrap();
        let queue = db.list_queue().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].1, "agent-2");
    }

    #[test]
    fn test_queue_dequeue_all() {
        let (_tmp, db) = setup_db();
        db.enqueue("sym::a", "agent-1", "first", "write").unwrap();
        db.enqueue("sym::b", "agent-1", "second", "write").unwrap();
        db.enqueue("sym::a", "agent-2", "third", "write").unwrap();

        let count = db.dequeue_all("agent-1").unwrap();
        assert_eq!(count, 2);
        let queue = db.list_queue().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].1, "agent-2");
    }

    // ── Deps tests ──

    #[test]
    fn test_upsert_and_get_deps() {
        let (_tmp, db) = setup_db();
        let symbols = vec![
            make_symbol("a.rs::login", "a.rs", "login", "function"),
            make_symbol("a.rs::validate", "a.rs", "validate", "function"),
            make_symbol("a.rs::hash", "a.rs", "hash", "function"),
        ];
        db.upsert_symbols(&symbols).unwrap();

        let deps = vec![
            Dep {
                caller: "a.rs::login".into(),
                callee: "a.rs::validate".into(),
                kind: "calls".into(),
            },
            Dep {
                caller: "a.rs::login".into(),
                callee: "a.rs::hash".into(),
                kind: "calls".into(),
            },
        ];
        db.upsert_deps(&deps).unwrap();

        let login_deps = db.get_deps("a.rs::login").unwrap();
        assert_eq!(login_deps.len(), 2);
    }

    #[test]
    fn test_transitive_deps() {
        let (_tmp, db) = setup_db();
        let symbols = vec![
            make_symbol("a.rs::a", "a.rs", "a", "function"),
            make_symbol("a.rs::b", "a.rs", "b", "function"),
            make_symbol("a.rs::c", "a.rs", "c", "function"),
        ];
        db.upsert_symbols(&symbols).unwrap();

        // a -> b -> c
        let deps = vec![
            Dep {
                caller: "a.rs::a".into(),
                callee: "a.rs::b".into(),
                kind: "calls".into(),
            },
            Dep {
                caller: "a.rs::b".into(),
                callee: "a.rs::c".into(),
                kind: "calls".into(),
            },
        ];
        db.upsert_deps(&deps).unwrap();

        let transitive = db.get_transitive_deps("a.rs::a").unwrap();
        assert!(transitive.contains(&"a.rs::b".to_string()));
        assert!(transitive.contains(&"a.rs::c".to_string()));
        assert_eq!(transitive.len(), 2);
    }

    #[test]
    fn test_enqueue_preserves_position_on_requeue() {
        let (_tmp, db) = setup_db();
        db.upsert_symbols(&[make_symbol("f.rs::x", "f.rs", "x", "function")])
            .unwrap();

        db.enqueue("f.rs::x", "agent-1", "first", "write").unwrap();
        db.enqueue("f.rs::x", "agent-2", "second", "write").unwrap();
        assert_eq!(db.queue_position("f.rs::x", "agent-1").unwrap(), Some(1));
        assert_eq!(db.queue_position("f.rs::x", "agent-2").unwrap(), Some(2));

        // Re-enqueuing agent-1 (e.g. retry) must NOT send it to the back.
        db.enqueue("f.rs::x", "agent-1", "first-again", "write")
            .unwrap();
        assert_eq!(db.queue_position("f.rs::x", "agent-1").unwrap(), Some(1));
        assert_eq!(db.queue_position("f.rs::x", "agent-2").unwrap(), Some(2));

        // The head is still agent-1, with its intent updated.
        let (head, intent, _mode) = db.next_in_queue("f.rs::x").unwrap().unwrap();
        assert_eq!(head, "agent-1");
        assert_eq!(intent, "first-again");
    }

    #[test]
    fn test_availability_ignores_expired_locks() {
        let (_tmp, db) = setup_db();
        db.upsert_symbols(&[make_symbol("f.rs::x", "f.rs", "x", "function")])
            .unwrap();

        // An expired lock (ttl 1s, locked_at well in the past) must not make the
        // symbol look unavailable.
        db.conn
            .execute(
                "INSERT INTO locks (symbol_id, agent_id, intent, mode, locked_at, ttl_seconds)
             VALUES ('f.rs::x', 'ghost', 'stale', 'write', datetime('now','-1 hour'), 1)",
                [],
            )
            .unwrap();

        let avail = db.available_symbols_in_files(&["f.rs"]).unwrap();
        assert!(
            avail.contains(&"f.rs::x".to_string()),
            "expired lock should free the symbol"
        );

        // A fresh lock keeps it unavailable.
        db.conn.execute(
            "UPDATE locks SET locked_at = datetime('now'), ttl_seconds = 600 WHERE symbol_id = 'f.rs::x'",
            [],
        ).unwrap();
        let avail2 = db.available_symbols_in_files(&["f.rs"]).unwrap();
        assert!(
            !avail2.contains(&"f.rs::x".to_string()),
            "fresh lock should occupy the symbol"
        );
    }
}
