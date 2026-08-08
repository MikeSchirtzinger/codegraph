#!/usr/bin/env bash
# Runs every R5 perf-measurement script in sequence, building the release
# binary first if it's missing. Re-runnable from a clean shell: each script
# creates its own scratch stores under mktemp and cleans them up itself, so
# nothing here depends on a prior run or on any other process's DB (embedded
# surrealkv exclusive-locks its file -- see README.md's Known Limitations --
# so never point two of these at the same store concurrently).
#
# Repro: ./scripts/bench/run_all.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

echo "=== 1/4: cargo build --release ==="
cargo build --release

echo
echo "=== 2/4: store-open cost (query/stats path: cold/warm, tiny/medium/large stores) ==="
python3 scripts/bench/store_open_cost.py

echo
echo "=== 3/4: index+resolve throughput (this repo's src/, and the whole fixture tree) ==="
python3 scripts/bench/index_throughput.py

echo
echo "=== 4/4: MCP serve-mode latency (p50/p95) + CLI-reopen comparison ==="
python3 scripts/bench/mcp_latency.py

echo
echo "Done. See README.md's Performance section for how these numbers are reported."
