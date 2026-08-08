# R5 perf-measurement scripts

Drivers behind the root `README.md`'s "Performance" section. Every number
published there came from one of these, run exactly as shown. Each script:

- is self-contained (stdlib only: Python 3, no pip installs; the one shell
  script is plain POSIX-ish bash);
- builds `target/release/codegraph` itself if it isn't there yet (respects
  `$CARGO_TARGET_DIR`);
- creates its own scratch `surrealkv` store(s) under `mktemp` and deletes them
  when done (pass `--keep` to any of the Python scripts to retain them for
  inspection);
- prints its own repro command as the first line of output.

Never point two of these at the same store concurrently. Embedded surrealkv
holds a datastore-level lock on its file (see the root README's Known
Limitations #2); each script's own scratch-dir-per-run avoids this by
construction, but running two of these scripts *against each other's* store
would not.

| Script | Measures |
|---|---|
| `store_open_cost.py` | Where a single `codegraph query`'s wall time goes (process spawn vs. embedded-store connect vs. query execution), cold vs. warm, across three store sizes. Re-checks D6's "37s wall / 0.8s CPU" claim directly. |
| `index_throughput.py` | Full `codegraph index` throughput (files/nodes/edges per second) on this repo's own `src/` and on the whole `tests/fixtures/` tree, plus the wall/CPU split and a connect/schema-init/index-body phase breakdown. Also times a standalone `codegraph resolve` re-run, with the caveat that it only re-touches the leftover AMBIGUOUS/UNRESOLVED edges (see its docstring). |
| `mcp_latency.py` | `codegraph serve` (MCP stdio) per-request p50/p95 over ≥50 requests for `codegraph_search`/`codegraph_impact`/`codegraph_architecture`, against the polyglot fixture and this repo's self-indexed `src/`; alongside a CLI-reopen comparison (the same queries as N separate `codegraph query` process invocations) to quantify serve-mode's amortization. |
| `run_all.sh` | Builds, then runs all three above in sequence. `./scripts/bench/run_all.sh` from a clean shell reproduces everything in the README's Performance section in one command. |

Each Python script also accepts `--help` for its full flag list (rep counts,
request counts, `--keep`).
