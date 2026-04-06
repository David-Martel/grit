#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────
# Sweep Benchmark: run synthetic tests across multiple agent
# counts and projects, producing a CSV for graphing.
#
# Usage:
#   ./bench.sh                              # default sweep
#   ./bench.sh --agents "10 20 30 50"       # custom counts
#   ./bench.sh --projects "ts-api rust-service py-ml"
#   ./bench.sh --rounds 3 --iterations 5
# ──────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/../lib/common.sh"

AGENT_COUNTS="10 20 30 50"
PROJECTS="ts-api"
NUM_ROUNDS=5
NUM_ITERATIONS=3

while [[ $# -gt 0 ]]; do
    case "$1" in
        --agents|-n)     AGENT_COUNTS="$2"; shift 2 ;;
        --projects)      PROJECTS="$2"; shift 2 ;;
        --rounds|-r)     NUM_ROUNDS="$2"; shift 2 ;;
        --iterations|-i) NUM_ITERATIONS="$2"; shift 2 ;;
        *)               err "Unknown arg: $1"; exit 1 ;;
    esac
done

RESULTS_DIR="$SCRIPT_DIR/results/sweep-$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"
CSV="$RESULTS_DIR/sweep.csv"

print_header "SWEEP BENCHMARK"
echo "  Agent counts:  $AGENT_COUNTS"
echo "  Projects:      $PROJECTS"
echo "  Rounds:        $NUM_ROUNDS"
echo "  Iterations:    $NUM_ITERATIONS"
echo ""
ensure_grit

echo "project,agents,iteration,git_ok,git_fail,git_conflicts,git_time,grit_ok,grit_conflicts,grit_time" > "$CSV"

for PROJECT in $PROJECTS; do
    # Pre-build symbol DB
    SYM_DB="$RESULTS_DIR/syms-$PROJECT.db"
    TMP_IDX=$(mktemp -d)
    setup_work_repo "$PROJECT" "$TMP_IDX"
    cp "$TMP_IDX/.grit/registry.db" "$SYM_DB"
    SYM_COUNT=$(sqlite3 "$SYM_DB" "SELECT COUNT(*) FROM symbols WHERE kind IN ('function','method')")
    rm -rf "$TMP_IDX"

    log "Project $PROJECT: $SYM_COUNT symbols"

    for NUM_AGENTS in $AGENT_COUNTS; do
        echo ""
        echo "── $PROJECT / $NUM_AGENTS agents ──"

        for ITER in $(seq 1 $NUM_ITERATIONS); do
            # ── GIT ──
            GIT_OK=0 GIT_FAIL=0 GIT_CONFLICTS=0 GIT_TIME=0

            for ROUND in $(seq 1 $NUM_ROUNDS); do
                WORK=$(mktemp -d)
                setup_git_repo "$PROJECT" "$WORK"

                mapfile -t SYMS < <(sqlite3 "$SYM_DB" "SELECT id FROM symbols WHERE kind IN ('function','method') ORDER BY RANDOM()")
                TOTAL=${#SYMS[@]}

                START_T=$SECONDS
                cd "$WORK"
                MAIN=$(git branch --show-current)

                for i in $(seq 1 $NUM_AGENTS); do
                    git checkout -q "$MAIN"
                    git checkout -q -b "agent-$i"
                    K=$((i - 1))
                    while [[ $K -lt $TOTAL ]]; do
                        SYM="${SYMS[$K]}"
                        modify_function "${SYM%%::*}" "${SYM##*::}" "a$i" "$WORK"
                        K=$((K + NUM_AGENTS))
                    done
                    git add -A 2>/dev/null
                    git commit -q -m "a$i" 2>/dev/null || true
                done

                git checkout -q "$MAIN"
                for i in $(seq 1 $NUM_AGENTS); do
                    if git merge --no-ff "agent-$i" -m "m" >/dev/null 2>&1; then
                        GIT_OK=$((GIT_OK + 1))
                    else
                        GIT_FAIL=$((GIT_FAIL + 1))
                        C=$(git diff --name-only --diff-filter=U 2>/dev/null | wc -l | tr -d ' ')
                        GIT_CONFLICTS=$((GIT_CONFLICTS + C))
                        git merge --abort 2>/dev/null
                    fi
                done

                GIT_TIME=$((GIT_TIME + SECONDS - START_T))
                rm -rf "$WORK"
            done

            # ── GRIT ──
            GRIT_OK=0 GRIT_CONFLICTS=0 GRIT_TIME=0

            for ROUND in $(seq 1 $NUM_ROUNDS); do
                WORK=$(mktemp -d)
                setup_work_repo "$PROJECT" "$WORK"

                mapfile -t SHUFFLED < <(shuffled_symbols "$WORK")
                TOTAL=${#SHUFFLED[@]}
                PER=$((TOTAL / NUM_AGENTS))
                [[ $PER -lt 1 ]] && PER=1

                START_T=$SECONDS

                for i in $(seq 1 $NUM_AGENTS); do
                    IDX=$(( (i - 1) * PER ))
                    [[ $IDX -ge $TOTAL ]] && continue

                    AGENT_SYMS=()
                    for j in $(seq 0 $((PER - 1))); do
                        K=$((IDX + j))
                        [[ $K -lt $TOTAL ]] && AGENT_SYMS+=("${SHUFFLED[$K]}")
                    done
                    [[ ${#AGENT_SYMS[@]} -eq 0 ]] && continue

                    (
                        "$GRIT" --repo "$WORK" claim -a "i${ITER}n${NUM_AGENTS}r${ROUND}a${i}" -i "t" "${AGENT_SYMS[@]}" >/dev/null 2>&1 || exit 1
                        WT="$WORK/.grit/worktrees/i${ITER}n${NUM_AGENTS}r${ROUND}a${i}"
                        if [[ -d "$WT" ]]; then
                            for SYM in "${AGENT_SYMS[@]}"; do
                                modify_function "${SYM%%::*}" "${SYM##*::}" "a$i" "$WT"
                            done
                        fi
                        "$GRIT" --repo "$WORK" done -a "i${ITER}n${NUM_AGENTS}r${ROUND}a${i}" >/dev/null 2>&1
                    ) &
                done

                wait
                GRIT_TIME=$((GRIT_TIME + SECONDS - START_T))

                cd "$WORK"
                C=$(git status --porcelain 2>/dev/null | grep -c "^UU" || echo "0")
                M=$(git log --oneline 2>/dev/null | grep -c "grit: merge" || echo "0")
                GRIT_CONFLICTS=$((GRIT_CONFLICTS + C))
                GRIT_OK=$((GRIT_OK + M))

                rm -rf "$WORK"
            done

            echo "$PROJECT,$NUM_AGENTS,$ITER,$GIT_OK,$GIT_FAIL,$GIT_CONFLICTS,$GIT_TIME,$GRIT_OK,$GRIT_CONFLICTS,$GRIT_TIME" >> "$CSV"

            TOTAL_RUNS=$((NUM_ROUNDS * NUM_AGENTS))
            GIT_RATE=$(echo "scale=1; $GIT_FAIL * 100 / $TOTAL_RUNS" | bc 2>/dev/null || echo "?")
            printf "  iter %d:  git fail=%3d/%d (%s%%)  grit conflicts=%d  git=%ds grit=%ds\n" \
                "$ITER" "$GIT_FAIL" "$TOTAL_RUNS" "$GIT_RATE" "$GRIT_CONFLICTS" "$GIT_TIME" "$GRIT_TIME"
        done
    done
done

echo ""
ok "Sweep results saved to $CSV"
echo ""
echo "CSV contents:"
cat "$CSV"
