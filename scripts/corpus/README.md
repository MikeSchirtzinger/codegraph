# Corpus validation

Proves that `codegraph init` / `doctor` / `index` / `query` / `plan lint` /
`landscape` work the first time, on a codebase codegraph has never seen,
with no flags and no tribal knowledge.

The receipt this produces lives at
`specs/receipts/run-anywhere-corpus-20260914.md`. Read that for the actual
findings. This file is only about running the script.

## What it does

`run.sh` reads `repos.txt` (ten public repositories, chosen for language and
project-shape diversity: one Go, one Python with a vendored tree, one
TypeScript workspace monorepo, one JSX-heavy React codebase, one Java, one
C, one C++, one Rust project, one deliberately polyglot project, and one
that is almost entirely outside codegraph's supported languages). For each
repository, in order, never two at once:

1. shallow-clones it if not already present
2. `codegraph init`
3. `codegraph doctor`
4. `codegraph index` (a cold, full index; timed and logged)
5. `codegraph query --kind summary`, `hubs`, `circular`, and `rdeps --name
   <the top hub>` (all without `--project-id`, proving the default
   resolution works)
6. `codegraph query --kind rdeps` with no `--name`, which is supposed to
   fail with a clear message rather than silently succeed
7. `codegraph plan lint`
8. `codegraph landscape` and `codegraph plan sync`
9. for two repositories, an extra targeted `query --kind search` proving a
   specific real symbol (a vendored Python function, a `.jsx` React
   component) actually made it into the graph
10. `codegraph index` again (incremental, timed and logged separately)

Every command runs under `/usr/bin/time -l`, so wall time, user/sys CPU, and
peak RSS are all captured, not just pass/fail.

## Prerequisites

- `git`, `jq` (`brew install jq` if missing), and a `bash` on `PATH`.
- A release build of `codegraph`. From the repository root:
  ```
  cargo build --release
  ```
  `run.sh` defaults to `target/release/codegraph` if you do not pass
  `--binary`. Point it at a different binary explicitly if you built
  somewhere else (a separate worktree, a different target dir); the script
  never guesses silently, it prints and logs exactly which binary it used.

## Running it

One command, from anywhere:

```
scripts/corpus/run.sh
```

This clones repositories (first time only) into
`$HOME/.cache/codegraph-corpus` and writes a fresh, timestamped output
directory under `scripts/corpus/out/`. Re-running later reuses the clones
(so it does not re-download several hundred megabytes every morning) but
always writes a new output directory, so nothing from a previous run is
overwritten or silently merged.

To rebuild a repository's clone from scratch, delete its directory under
the corpus dir and re-run:

```
rm -rf "$HOME/.cache/codegraph-corpus/pyvendor"
scripts/corpus/run.sh
```

Full flag list:

```
scripts/corpus/run.sh \
  --repos scripts/corpus/repos.txt \
  --binary target/release/codegraph \
  --corpus-dir "$HOME/.cache/codegraph-corpus" \
  --out scripts/corpus/out/run-2026-09-15
```

`run.sh` exits non-zero if and only if some repository's first (cold)
`codegraph index` failed. A stub bailing (see below), or the deliberate
`rdeps` failure in step 6, does not fail the run.

## Output

```
<out>/
  run.log            driver log: what ran, when, in what order
  results.tsv         one row per repo, the machine-extracted numbers
  summary.md          the same numbers as a markdown table
  <repo-name>/
    00-clone.log
    01-init.log
    02-doctor.log
    03-index.log                       cold index
    03-index.uptime-before.txt         `uptime` immediately before it
    04-query-summary.log
    05-query-hubs.log
    06-query-hubs-json.log             used to find the top hub's name
    07-query-circular.log
    08-query-rdeps-tophub.log
    09-query-rdeps-noname.log          expected to fail, see repos.txt/README
    10-plan-lint.log
    11-landscape.log
    12-plan-sync.log
    13-index-incremental.log           second, incremental index
    13-index-incremental.uptime-before.txt
    14-query-search-probe.log          only if repos.txt sets probe_symbol
    15-ignore-probe-search.log         only if repos.txt sets ignore_probe_dir
```

`summary.md` is generated mechanically from the logs and is deliberately
dumb: a blank cell means the pattern it looks for was not in the log, which
is itself informative (a stub bailed before printing a number, or a
language-less repository produced zero nodes) but is not the same thing as
analysis. The hand-written receipt at
`specs/receipts/run-anywhere-corpus-20260914.md` is where the numbers get
interpreted, including which non-zero exit codes are the tool working
correctly and which ones are not.

## Two things in `repos.txt` worth knowing about before reading a log

**`ignore_probe_dir`** (set for the Python repo only): before the first
index, `run.sh` plants a file at `<repo>/__pycache__/codegraph_l5_ignore_probe.py`
defining `codegraph_l5_probe_function`. `__pycache__` is the kind of
directory `.gitignore` conventionally excludes. After indexing, the script
searches for that exact function name. Finding it would mean a directory
that should have been skipped got indexed anyway; not finding it is the
pass case, and is checked, not assumed.

**`probe_symbol`**: a real, known symbol name from that repository (a
vendored third-party function for the Python repo, a React component class
for the JSX repo). After indexing, `run.sh` searches for it by name. This is
the concrete check behind "did the parser actually handle this file",
rather than trusting an exit code of 0 to mean the same thing.

## Expected stubs at this commit

This corpus was run against commit `e4932ef` (lane L1's run-anywhere
commit), which is built on the board's P0 contract but lands before lane
L2's `codegraph plan` implementation. `landscape`, `plan sync`, and (this
was not anticipated going in; see the receipt) `plan lint` bail with a
message naming lane L2 rather than doing anything, at that commit. `run.sh`
still runs all three and logs whatever they print; it does not special-case
them. The receipt records this as EXPECTED-PENDING, not FAIL, and says
exactly which commit changes it.

## No em dashes

Per house style, nothing this script prints or writes uses an em dash.
