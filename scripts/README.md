# Benchmark Scripts

## Directory Structure

```
scripts/
  ai-agents/      Real AI agent benchmarks (claude -p / gemini -p)
  synthetic/       No-AI merge conflict benchmarks (grit vs raw git)
  sweep/           Multi-config sweep benchmarks (vary agent count & project)
  lib/             Shared helpers
```

## AI Agent Benchmark (`ai-agents/`)

Launch real AI agents in parallel, each coordinating via grit.

```bash
# 10 claude agents on ts-api
./scripts/ai-agents/bench.sh

# 20 gemini agents on rust-service
./scripts/ai-agents/bench.sh --agents 20 --provider gemini --project rust-service

# Sweep: 10, 20, 30, 50 agents
./scripts/ai-agents/bench.sh --sweep

# All combinations
./scripts/ai-agents/bench.sh --sweep --provider claude
./scripts/ai-agents/bench.sh --sweep --provider gemini
```

## Synthetic Benchmark (`synthetic/`)

Pure merge-conflict stress test. No AI, just simulated edits.

```bash
# 20 agents, 10 rounds
./scripts/synthetic/bench.sh

# 50 agents, 5 rounds on rust-service
./scripts/synthetic/bench.sh --agents 50 --rounds 5 --project rust-service
```

## Sweep Benchmark (`sweep/`)

Run across multiple agent counts and projects, output CSV.

```bash
# Default: 10,20,30,50 agents on ts-api
./scripts/sweep/bench.sh

# Custom sweep
./scripts/sweep/bench.sh --agents "10 20 30 50" --projects "ts-api rust-service py-ml" --iterations 5
```

## Test Projects

Available in `test-projects/`:

| Project | Language | Description |
|---------|----------|-------------|
| `ts-api` | TypeScript | REST API with auth, middleware |
| `rust-service` | Rust | HTTP service with handlers |
| `py-ml` | Python | ML pipeline |
| `pi-calc` | Rust + TS | Full-stack calculator |

## Results

Each benchmark creates a timestamped results directory with:
- Agent logs (`agent-*.log`)
- Summary CSV (`summary.csv` or `results.csv`)
- Work repo (cleaned up after run)
