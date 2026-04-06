#!/usr/bin/env bash
# Shared helpers for grit benchmark scripts

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GRIT="$REPO_ROOT/target/release/grit"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

log()  { echo -e "${BLUE}[bench]${NC} $1"; }
ok()   { echo -e "${GREEN}[+]${NC} $1"; }
err()  { echo -e "${RED}[x]${NC} $1"; }
warn() { echo -e "${YELLOW}[!]${NC} $1"; }

# Ensure grit is built
ensure_grit() {
    if [[ ! -x "$GRIT" ]]; then
        log "Building grit (release)..."
        cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" 2>/dev/null
    fi
    ok "grit: $GRIT"
}

# Create a bare git work repo from a test project (no grit init)
# Usage: setup_git_repo <project_name> <dest_dir>
setup_git_repo() {
    local project="$1" dest="$2"
    local src="$REPO_ROOT/test-projects/$project"
    [[ -d "$src" ]] || { err "Test project not found: $src"; return 1; }

    rm -rf "$dest"
    cp -r "$src" "$dest"
    (
        cd "$dest"
        git init -q
        git add -A
        git commit -q -m "init"
    )
}

# Create a work repo with grit initialized
# Usage: setup_work_repo <project_name> <dest_dir>
setup_work_repo() {
    setup_git_repo "$1" "$2"
    "$GRIT" --repo "$2" init >/dev/null 2>&1
}

# Get symbol count from a grit repo
symbol_count() {
    sqlite3 "$1/.grit/registry.db" "SELECT COUNT(*) FROM symbols WHERE kind IN ('function','method')" 2>/dev/null
}

# Get shuffled symbol IDs
shuffled_symbols() {
    sqlite3 "$1/.grit/registry.db" "SELECT id FROM symbols WHERE kind IN ('function','method') ORDER BY RANDOM()" 2>/dev/null
}

# Modify a function body (insert comment lines)
modify_function() {
    local FILE="$1" FUNC="$2" TAG="$3" DIR="$4"
    local FILEPATH="$DIR/$FILE"
    [[ -f "$FILEPATH" ]] || return 0
    local LINE=$(grep -n "fn ${FUNC}\b\|function ${FUNC}\b\|def ${FUNC}\b\|const ${FUNC}\b" "$FILEPATH" 2>/dev/null | head -1 | cut -d: -f1)
    if [[ -n "$LINE" ]] && [[ "$LINE" -gt 0 ]]; then
        local INSERT=$((LINE + 1))
        if [[ "$FILE" == *.rs ]]; then
            sed -i '' "${INSERT}i\\
    // modified by ${TAG}
" "$FILEPATH" 2>/dev/null
        elif [[ "$FILE" == *.ts ]] || [[ "$FILE" == *.tsx ]] || [[ "$FILE" == *.js ]]; then
            sed -i '' "${INSERT}i\\
  // modified by ${TAG}
" "$FILEPATH" 2>/dev/null
        elif [[ "$FILE" == *.py ]]; then
            sed -i '' "${INSERT}i\\
    # modified by ${TAG}
" "$FILEPATH" 2>/dev/null
        fi
    fi
}

# Add a header comment to a file (forces merge conflicts when multiple agents touch same file)
add_file_header() {
    local FILE="$1" TAG="$2" DIR="$3"
    local FILEPATH="$DIR/$FILE"
    [[ -f "$FILEPATH" ]] || return 0
    sed -i '' "1s/^/\/\/ ${TAG} $(date +%s%N)\n/" "$FILEPATH" 2>/dev/null
}

# Print a results table header
print_header() {
    local title="$1"
    echo ""
    echo "=================================================================="
    echo "  $title"
    echo "=================================================================="
    echo ""
}

# Print a two-column comparison row
print_row() {
    printf "  %-24s  %-16s  %-16s\n" "$1" "$2" "$3"
}
