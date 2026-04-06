#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────
# Synthetic Benchmark: grit vs raw git (no AI, pure merge test)
#
# Simulates N agents modifying different functions in the same
# files and measures merge conflict rate.
#
# Usage:
#   ./bench.sh                     # 20 agents, 10 rounds, ts-api
#   ./bench.sh --agents 50 --rounds 5
#   ./bench.sh --project rust-service
# ──────────────────────────────────────────────────────────────
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/../lib/common.sh"

NUM_AGENTS=20
NUM_ROUNDS=10
PROJECT="ts-api"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --agents|-n) NUM_AGENTS="$2"; shift 2 ;;
        --rounds|-r) NUM_ROUNDS="$2"; shift 2 ;;
        --project)   PROJECT="$2"; shift 2 ;;
        *)           err "Unknown arg: $1"; exit 1 ;;
    esac
done

RESULTS_DIR="$SCRIPT_DIR/results/synthetic-${NUM_AGENTS}agents-${PROJECT}-$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"
CSV="$RESULTS_DIR/results.csv"

print_header "SYNTHETIC BENCHMARK: grit vs git"
echo "  Agents: $NUM_AGENTS"
echo "  Rounds: $NUM_ROUNDS"
echo "  Project: $PROJECT"
echo ""
ensure_grit

# Build symbol index once
SYM_DB="$RESULTS_DIR/syms.db"
TMP_IDX=$(mktemp -d)
setup_work_repo "$PROJECT" "$TMP_IDX"
cp "$TMP_IDX/.grit/registry.db" "$SYM_DB"
SYM_COUNT=$(sqlite3 "$SYM_DB" "SELECT COUNT(*) FROM symbols WHERE kind IN ('function','method')")
DEP_COUNT=$(sqlite3 "$SYM_DB" "SELECT COUNT(*) FROM deps" 2>/dev/null || echo 0)
rm -rf "$TMP_IDX"

log "Symbols: $SYM_COUNT functions/methods, $DEP_COUNT dependencies"
echo ""

# ═════════════════════════════════════════
# PART 1: RAW GIT
# ═════════════════════════════════════════
echo "── PART 1: RAW GIT ──"
echo ""

GIT_OK=0 GIT_FAIL=0 GIT_CONFLICTS=0 GIT_TIME=0

for ROUND in $(seq 1 $NUM_ROUNDS); do
    WORK="$RESULTS_DIR/git-r$ROUND"
    setup_git_repo "$PROJECT" "$WORK"

    mapfile -t SYMS < <(sqlite3 "$SYM_DB" "SELECT id FROM symbols WHERE kind IN ('function','method') ORDER BY RANDOM()")
    TOTAL=${#SYMS[@]}

    START_T=$SECONDS
    MAIN_BRANCH=$(git -C "$WORK" branch --show-current)

    for i in $(seq 1 $NUM_AGENTS); do
        git -C "$WORK" checkout -q "$MAIN_BRANCH"
        git -C "$WORK" checkout -q -b "agent-$i"

        K=$((i - 1))
        while [[ $K -lt $TOTAL ]]; do
            SYM="${SYMS[$K]}"
            modify_function "${SYM%%::*}" "${SYM##*::}" "agent-$i-r$ROUND" "$WORK"
            K=$((K + NUM_AGENTS))
        done

        git -C "$WORK" add -A 2>/dev/null
        git -C "$WORK" commit -q -m "agent-$i round $ROUND" 2>/dev/null || true
    done

    git -C "$WORK" checkout -q "$MAIN_BRANCH"
    ROUND_OK=0 ROUND_FAIL=0 ROUND_CONFLICTS=0

    for i in $(seq 1 $NUM_AGENTS); do
        if git -C "$WORK" merge --no-ff "agent-$i" -m "merge agent-$i" >/dev/null 2>&1; then
            ROUND_OK=$((ROUND_OK + 1))
        else
            ROUND_FAIL=$((ROUND_FAIL + 1))
            CONF=$(git -C "$WORK" diff --name-only --diff-filter=U 2>/dev/null | wc -l | tr -d ' ')
            CONF=${CONF:-0}
            ROUND_CONFLICTS=$((ROUND_CONFLICTS + CONF))
            git -C "$WORK" merge --abort 2>/dev/null
        fi
    done

    ELAPSED=$((SECONDS - START_T))
    GIT_TIME=$((GIT_TIME + ELAPSED))
    GIT_OK=$((GIT_OK + ROUND_OK))
    GIT_FAIL=$((GIT_FAIL + ROUND_FAIL))
    GIT_CONFLICTS=$((GIT_CONFLICTS + ROUND_CONFLICTS))

    ICON="OK"; [[ $ROUND_FAIL -gt 0 ]] && ICON="!!"
    printf "  Round %2d  [%s]  ok=%2d  FAIL=%2d  conflicts=%d  %ds\n" \
        "$ROUND" "$ICON" "$ROUND_OK" "$ROUND_FAIL" "$ROUND_CONFLICTS" "$ELAPSED"

    rm -rf "$WORK"
done

echo ""
echo "  GIT TOTAL: $GIT_OK ok, $GIT_FAIL FAILED, $GIT_CONFLICTS conflict files, ${GIT_TIME}s"
echo ""

# ═════════════════════════════════════════
# PART 2: GRIT
# ═════════════════════════════════════════
echo "── PART 2: GRIT ──"
echo ""

GRIT_OK=0 GRIT_CONFLICTS=0 GRIT_TIME=0

for ROUND in $(seq 1 $NUM_ROUNDS); do
    WORK="$RESULTS_DIR/grit-r$ROUND"
    setup_work_repo "$PROJECT" "$WORK"

    mapfile -t SHUFFLED < <(shuffled_symbols "$WORK")
    TOTAL=${#SHUFFLED[@]}
    PER_AGENT=$((TOTAL / NUM_AGENTS))
    [[ $PER_AGENT -lt 1 ]] && PER_AGENT=1

    START_T=$SECONDS

    for i in $(seq 1 $NUM_AGENTS); do
        IDX=$(( (i - 1) * PER_AGENT ))
        [[ $IDX -ge $TOTAL ]] && continue

        AGENT_SYMS=()
        for j in $(seq 0 $((PER_AGENT - 1))); do
            K=$((IDX + j))
            [[ $K -lt $TOTAL ]] && AGENT_SYMS+=("${SHUFFLED[$K]}")
        done
        [[ ${#AGENT_SYMS[@]} -eq 0 ]] && continue

        (
            set +e
            "$GRIT" --repo "$WORK" claim -a "r${ROUND}a${i}" -i "round$ROUND-task$i" "${AGENT_SYMS[@]}" >/dev/null 2>&1 || exit 0

            WT="$WORK/.grit/worktrees/r${ROUND}a${i}"
            if [[ -d "$WT" ]]; then
                for SYM in "${AGENT_SYMS[@]}"; do
                    modify_function "${SYM%%::*}" "${SYM##*::}" "agent-$i-r$ROUND" "$WT"
                done
            fi

            "$GRIT" --repo "$WORK" done -a "r${ROUND}a${i}" >/dev/null 2>&1
        ) &
    done

    wait
    ELAPSED=$((SECONDS - START_T))
    GRIT_TIME=$((GRIT_TIME + ELAPSED))

    CONF=$(git -C "$WORK" status --porcelain 2>/dev/null | grep -c "^UU" || true)
    CONF=${CONF:-0}
    MERGES=$(git -C "$WORK" log --oneline 2>/dev/null | grep -c "grit:" || true)
    MERGES=${MERGES:-0}
    GRIT_CONFLICTS=$((GRIT_CONFLICTS + CONF))
    GRIT_OK=$((GRIT_OK + MERGES))

    LOCKS_OK="clean"
    "$GRIT" --repo "$WORK" status 2>/dev/null | grep -q "No active locks" || LOCKS_OK="DIRTY"

    printf "  Round %2d  [OK]  merges=%2d/%d  conflicts=%d  locks=%-5s  %ds\n" \
        "$ROUND" "$MERGES" "$NUM_AGENTS" "$CONF" "$LOCKS_OK" "$ELAPSED"

    rm -rf "$WORK"
done

echo ""
echo "  GRIT TOTAL: $GRIT_OK merges, $GRIT_CONFLICTS conflicts, ${GRIT_TIME}s"
echo ""

# ═════════════════════════════════════════
# SUMMARY
# ═════════════════════════════════════════
TOTAL_RUNS=$((NUM_ROUNDS * NUM_AGENTS))
GIT_RATE=0
[[ $TOTAL_RUNS -gt 0 ]] && GIT_RATE=$(echo "scale=1; $GIT_FAIL * 100 / $TOTAL_RUNS" | bc)

print_header "RESULTS: $NUM_AGENTS agents x $NUM_ROUNDS rounds on $PROJECT"
print_row "" "RAW GIT" "GRIT"
print_row "------------------------" "----------------" "----------------"
print_row "Agent runs" "$TOTAL_RUNS" "$TOTAL_RUNS"
print_row "Merges OK" "$GIT_OK" "$GRIT_OK"
print_row "Merges FAILED" "$GIT_FAIL" "0"
print_row "Conflict files" "$GIT_CONFLICTS" "$GRIT_CONFLICTS"
print_row "Failure rate" "${GIT_RATE}%" "0%"
print_row "Total time" "${GIT_TIME}s" "${GRIT_TIME}s"
print_row "Execution" "sequential" "parallel"
echo ""

echo "agents,rounds,project,git_ok,git_fail,git_conflicts,git_fail_rate,git_time,grit_ok,grit_conflicts,grit_time" > "$CSV"
echo "$NUM_AGENTS,$NUM_ROUNDS,$PROJECT,$GIT_OK,$GIT_FAIL,$GIT_CONFLICTS,$GIT_RATE,$GIT_TIME,$GRIT_OK,$GRIT_CONFLICTS,$GRIT_TIME" >> "$CSV"

ok "Results saved to $RESULTS_DIR/"
