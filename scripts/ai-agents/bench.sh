#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────
# AI Agent Benchmark: launch N real AI agents (claude/gemini)
# in parallel, each coordinating via grit.
#
# Usage:
#   ./bench.sh                          # 10 agents, claude, ts-api
#   ./bench.sh --agents 20              # 20 agents
#   ./bench.sh --agents 50 --provider gemini
#   ./bench.sh --agents 30 --provider claude --project rust-service
#   ./bench.sh --sweep                  # run 10,20,30,50 agents
# ──────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/../lib/common.sh"

# ── Defaults ──
NUM_AGENTS=10
PROVIDER="claude"
PROJECT="ts-api"
SWEEP=false
STAGGER=0.5

# ── Parse args ──
while [[ $# -gt 0 ]]; do
    case "$1" in
        --agents|-n)  NUM_AGENTS="$2"; shift 2 ;;
        --provider|-p) PROVIDER="$2"; shift 2 ;;
        --project)    PROJECT="$2"; shift 2 ;;
        --sweep)      SWEEP=true; shift ;;
        --stagger)    STAGGER="$2"; shift 2 ;;
        *)            err "Unknown arg: $1"; exit 1 ;;
    esac
done

# Validate provider
case "$PROVIDER" in
    claude|gemini) ;;
    *) err "Provider must be 'claude' or 'gemini', got: $PROVIDER"; exit 1 ;;
esac

RESULTS_BASE="$REPO_ROOT/scripts/ai-agents/results"
mkdir -p "$RESULTS_BASE"

# ── Task descriptions (rotated across agents) ──
TASKS=(
    "Add input validation. Check parameter types and throw descriptive errors for invalid inputs."
    "Add logging at function entry and exit. Log function name, parameters, and return value."
    "Add error handling with try-catch. Catch specific errors and rethrow with context."
    "Add documentation comments describing purpose, parameters, return value, and examples."
    "Optimize for performance. Add early returns and reduce unnecessary allocations."
    "Add telemetry. Track execution time, call count, and error rate."
    "Add null/undefined safety checks at the start of the function."
    "Add retry logic for operations that might fail transiently."
    "Add rate limiting checks. Throw if called too frequently."
    "Add structured error codes instead of string error messages."
    "Add timeout handling for async operations."
    "Add circuit breaker pattern for external calls."
    "Add data sanitization for user-provided inputs."
    "Refactor into smaller helper functions where the function is too long."
    "Add feature flag checks at the start of the function."
    "Add caching with TTL where appropriate."
    "Add deprecation warnings where old patterns are used."
    "Add request/response type validation using runtime checks."
    "Convert callbacks to async/await. Add proper error propagation."
    "Add unit test scaffolding as comments showing expected behavior."
)

# ── Run benchmark for a given agent count ──
run_bench() {
    local N="$1"
    local RUN_ID="${PROVIDER}-${N}agents-${PROJECT}-$(date +%Y%m%d_%H%M%S)"
    local RESULTS_DIR="$RESULTS_BASE/$RUN_ID"
    mkdir -p "$RESULTS_DIR"

    print_header "AI AGENT BENCHMARK: $N x $PROVIDER on $PROJECT"
    ensure_grit

    # Setup work repo
    local WORK="$RESULTS_DIR/repo"
    log "Setting up $PROJECT..."
    setup_work_repo "$PROJECT" "$WORK"

    local TOTAL_SYMS
    TOTAL_SYMS=$(symbol_count "$WORK")
    local PER_AGENT=$((TOTAL_SYMS / N))
    [[ $PER_AGENT -lt 1 ]] && PER_AGENT=1

    log "Symbols: $TOTAL_SYMS total, ~$PER_AGENT per agent"
    log "Provider: $PROVIDER"
    log "Launching $N agents..."
    echo ""

    # Get shuffled symbols
    mapfile -t ALL_SYMBOLS < <(shuffled_symbols "$WORK")

    # Launch agents
    local PIDS=()
    local START_TIME
    START_TIME=$(date +%s)

    for i in $(seq 1 "$N"); do
        local AGENT_ID="${PROVIDER}-agent-$i"
        local OFFSET=$(( (i - 1) * PER_AGENT ))

        # Slice symbols for this agent
        local SYMS=()
        for j in $(seq 0 $((PER_AGENT - 1))); do
            local IDX=$((OFFSET + j))
            if [[ $IDX -lt $TOTAL_SYMS ]]; then
                SYMS+=("${ALL_SYMBOLS[$IDX]}")
            fi
        done
        [[ ${#SYMS[@]} -eq 0 ]] && continue

        local TASK_IDX=$(( (i - 1) % ${#TASKS[@]} ))
        local TASK="${TASKS[$TASK_IDX]}"
        local SYM_LIST=$(printf '"%s" ' "${SYMS[@]}")

        local AGENT_PROMPT="You are agent '$AGENT_ID' working on the repository at '$WORK'.
You MUST use the grit CLI at '$GRIT' to coordinate with other agents.

STEP 1: Claim your symbols
Run: $GRIT --repo $WORK claim -a $AGENT_ID -i \"$TASK\" $SYM_LIST

If any symbol is BLOCKED, skip it and work only on the granted ones.

STEP 2: For each GRANTED symbol, make this modification:
$TASK

The symbols are in format 'file::function_name'.
Edit the function in the worktree at $WORK/.grit/worktrees/$AGENT_ID/
Keep changes minimal and focused on the assigned symbols only.

STEP 3: When done, release your locks:
Run: $GRIT --repo $WORK done -a $AGENT_ID

IMPORTANT:
- ONLY modify functions you have successfully claimed
- Do NOT modify any other code
- Keep changes small and focused
- If claim fails for a symbol, SKIP it
- Always run 'done' at the end"

        log "  agent-$i: ${#SYMS[@]} symbols (${SYMS[0]%::*}...)"

        "$PROVIDER" -p "$AGENT_PROMPT" > "$RESULTS_DIR/agent-$i.log" 2>&1 &
        PIDS+=($!)

        # Stagger launches
        sleep "$STAGGER"
    done

    log "All $N agents launched. Waiting..."
    echo ""

    # Wait for all
    for pid in "${PIDS[@]}"; do
        wait "$pid" 2>/dev/null || true
    done

    local END_TIME
    END_TIME=$(date +%s)
    local DURATION=$((END_TIME - START_TIME))

    # ── Verify results ──
    local PASS=0 FAIL=0
    for i in $(seq 1 "$N"); do
        local LOGFILE="$RESULTS_DIR/agent-$i.log"
        if [[ -f "$LOGFILE" ]]; then
            if grep -qi "released\|done\|grit done\|Released" "$LOGFILE" 2>/dev/null; then
                ((PASS++))
            else
                ((FAIL++))
            fi
        else
            ((FAIL++))
        fi
    done

    local LOCKS_REMAINING
    LOCKS_REMAINING=$("$GRIT" --repo "$WORK" status 2>/dev/null | grep -c ">" || echo "0")

    cd "$WORK"
    local CONFLICT_COUNT
    CONFLICT_COUNT=$(git status --porcelain 2>/dev/null | grep -c "^UU" || echo "0")
    local MERGE_COUNT
    MERGE_COUNT=$(git log --oneline 2>/dev/null | grep -c "grit: merge" || echo "0")

    # Check queue
    local QUEUE_COUNT
    QUEUE_COUNT=$("$GRIT" --repo "$WORK" queue list 2>/dev/null | grep -c ">" || echo "0")

    # ── Print results ──
    print_header "RESULTS: $N x $PROVIDER on $PROJECT"
    echo "  Provider:           $PROVIDER"
    echo "  Agents launched:    $N"
    echo "  Agents succeeded:   $PASS"
    echo "  Agents failed:      $FAIL"
    echo "  Duration:           ${DURATION}s"
    echo "  Merges completed:   $MERGE_COUNT"
    echo "  Git conflicts:      $CONFLICT_COUNT"
    echo "  Remaining locks:    $LOCKS_REMAINING"
    echo "  Queue entries:      $QUEUE_COUNT"
    echo "  Total symbols:      $TOTAL_SYMS"
    echo ""

    if [[ $CONFLICT_COUNT -eq 0 ]]; then
        ok "ZERO GIT CONFLICTS with $N parallel $PROVIDER agents"
    else
        err "$CONFLICT_COUNT conflicts detected"
    fi

    if [[ $LOCKS_REMAINING -eq 0 ]]; then
        ok "All locks properly released"
    else
        warn "$LOCKS_REMAINING locks still held"
    fi

    echo ""
    echo "  Agent logs: $RESULTS_DIR/"
    for i in $(seq 1 "$N"); do
        local LOGFILE="$RESULTS_DIR/agent-$i.log"
        if [[ -f "$LOGFILE" ]]; then
            local SIZE
            SIZE=$(wc -c < "$LOGFILE" | tr -d ' ')
            local LAST
            LAST=$(tail -1 "$LOGFILE" 2>/dev/null | head -c 60)
            printf "    agent-%02d: %6s bytes — %s\n" "$i" "$SIZE" "$LAST"
        fi
    done

    # Save summary CSV
    echo "provider,agents,project,pass,fail,duration_s,merges,conflicts,locks_remaining,queue,symbols" > "$RESULTS_DIR/summary.csv"
    echo "$PROVIDER,$N,$PROJECT,$PASS,$FAIL,$DURATION,$MERGE_COUNT,$CONFLICT_COUNT,$LOCKS_REMAINING,$QUEUE_COUNT,$TOTAL_SYMS" >> "$RESULTS_DIR/summary.csv"

    echo ""
    ok "Results saved to $RESULTS_DIR/"
}

# ── Main ──
if $SWEEP; then
    for N in 10 20 30 50; do
        run_bench "$N"
        echo ""
        echo "────────────────────────────────────────"
        echo ""
    done
else
    run_bench "$NUM_AGENTS"
fi
