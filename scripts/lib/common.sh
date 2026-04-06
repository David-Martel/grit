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

# Create a work repo from a test project
# Usage: setup_work_repo <project_name> <dest_dir>
setup_work_repo() {
    local project="$1" dest="$2"
    local src="$REPO_ROOT/test-projects/$project"
    [[ -d "$src" ]] || { err "Test project not found: $src"; return 1; }

    rm -rf "$dest"
    cp -r "$src" "$dest"
    cd "$dest"
    git init -q
    git add -A
    git commit -q -m "init"
    "$GRIT" --repo "$dest" init >/dev/null 2>&1
    cd - >/dev/null
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
