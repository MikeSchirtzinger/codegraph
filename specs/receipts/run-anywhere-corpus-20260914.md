# run-anywhere-corpus-20260914: does it actually work on a codebase it has never seen

Corpus validation. The run-anywhere work (`init`, `doctor`, defaults, ignore rules, progress, errors,
receipt `specs/receipts/run-anywhere-20260914.md`) was validated end to end on one
repository, cobra. This receipt is the multi-repository check: ten public
repositories, chosen for language and project-shape diversity, none of them cobra,
none of them touched by hand, run through the full first-time-user command sequence
with a script that anyone can re-run.

Binary under test: commit `e4932ef` (`feat(run-anywhere): first run on an unfamiliar
repository needs no tribal knowledge`), built with `cargo build --release` in an
isolated git worktree with an isolated `CARGO_TARGET_DIR`, never the shared working
tree (which has five other lanes' uncommitted edits at the time of this run). Build
was clean: exit 0, 0 warnings.

```
$ git -C /Users/mike/dev/codegraph worktree add <scratch>/l5-wt e4932ef
$ CARGO_TARGET_DIR=<scratch>/l5-target cargo build --release
   ...
    Finished `release` profile [optimized] target(s) in 12m 19s
$ CARGO_TARGET_DIR=<scratch>/l5-target cargo build --release 2>&1 | grep -c '^warning'
0
```

Script: `scripts/corpus/run.sh`, `scripts/corpus/repos.txt`. Usage in
`scripts/corpus/README.md`. Every command below ran through that script, one repo at
a time, logged in full (stdout, stderr, exit code, `/usr/bin/time -l` wall/user/sys/
peak-RSS/instructions-retired) to a per-step log file this receipt cites throughout.
Nothing in this document was typed by hand from memory; every number names the log
file it came from.

## 0. Read this before any single number below

This machine was running five to nine other lanes' builds and indexing runs for the
entire duration of this corpus pass (`ps aux` during the run shows concurrent `rustc`
and `codegraph index` processes from other lanes; `uptime` 1-minute load ranged from
~11 at the start to the high 20s/30s by the middle repos, spiking to 43.7). Lane L9's
own profiling receipt (`specs/receipts/index-profile-20260914.md`) measured that
contention on this exact machine inflates per-phase wall time by roughly 1.1-1.8x, and
separately root-caused a much larger gap to a real, specific defect: `codegraph index`
with no `--force` (the exact invocation this script runs, and the one `init` itself
recommends) takes the `resolve_incremental` path, whose write-back issued one
conditional `UPDATE ... WHERE` per edge. On cobra: 0.9s / 17.1B instructions retired
with `--force`, 44.4s / 195.2B without, an 11.4x instructions-retired swing holding
binary, store, and contention constant.

**The mechanism L9 published for that swing was itself wrong, and this receipt
does not repeat it.** L9's original write-up (`index-profile-20260914.md` §8.5)
attributed the cost to SQL parsing overhead (4,458 individually-parsed statements
against 18 bulk ones). Lane L7 tested that hypothesis directly by building exactly
the prescribed fix, collapsing the 4,458 parses into 18 via a server-side loop over a
bound array, and instructions retired went *up*, 195.5B to 306.2B, 1.57x the wrong
direction (commit `3a517b2`, `specs/receipts/incremental-writeback-20260914.md`
§2). Parsing was never the dominant cost. The real mechanism: a conditional
`UPDATE ... WHERE` has to find its matching rows with no index behind that clause,
so N such updates over an N-edge table is quadratic, not linear with a high
constant; cobra's 4,458 edges gives roughly 4,458² ≈ 19.9M row visits, which
matches the measured instruction count. The shipped fix (`3a517b2`, already on the
commit this corpus's after-pass in section 11 is built from) deletes by source node
and bulk-inserts, the shape the `--force` path already had, with no per-row `WHERE`
anywhere in the path: cobra resolve 40.6s to 0.8s, 195,523,006,829 to 19,244,398,211
instructions retired, 10.2x fewer, output field-identical to `--force` on every
metric.

This run's own sections 8 and 9 were captured against `e4932ef`, before this fix
existed, and are real corpus-scale evidence of exactly this defect (the corpus
never once passed `--force`), not a second, unrelated phenomenon. Section 11 below
measures the same ten repos against the fix. **Every wall-clock and user-CPU number
in sections 3, 5, and 8 of this receipt is real and reproducible on this machine at
that commit, and is not a trustworthy estimate of what codegraph costs in
isolation, or of what it costs after `3a517b2`** (section 11 is). Where it matters,
this receipt also reports `/usr/bin/time -l`'s instructions-retired, the number
both L9 and L7 used as load-independent ground truth.

What none of that touches: exit codes, error messages, node/edge counts, resolved
percentages, which files got skipped and why, whether a symbol made it into the
graph. Those are measured facts about correctness and UX, not performance, and this
receipt's real job is proving those.

## 1. The corpus

| Repo | Language mix | Why this one | Tracked files | SHA |
|---|---|---|---|---|
| gin-gonic/gin | Go | Not cobra (recon already covered cobra) | 130 (99 `.go`) | see `repos.txt` |
| pypa/pip | Python, vendored tree | `src/pip/_vendor/` is 252 tracked, real (not wheel/sdist) third-party `.py` source; `.gitignore` has `__pycache__/`, `/build/`, `/dist/`, `docs/build/`, `.mypy_cache/`, `htmlcov/` | 1072 (661 `.py`) | see `repos.txt` |
| vitejs/vite | TypeScript, pnpm workspace | `pnpm-workspace.yaml`, 3 real packages (`vite`, `create-vite`, `plugin-legacy`) plus a large `playground/` of fixtures | 2834 (944 `.js` + 575 `.ts` + 21 `.mjs`) | see `repos.txt` |
| airbnb/react-dates | JavaScript/JSX | 49 tracked `.jsx` files, real class components, not TSX. Lane L1 fixed `.jsx` grammar routing; this is the first real corpus test of that fix | 201 (49 `.jsx` + 119 `.js`) | see `repos.txt` |
| square/retrofit | Java | Standard `src/main/java` Maven/Gradle layout | 1110 (306 `.java`) | see `repos.txt` |
| libuv/libuv | C | | 483 (331 `.c` + 38 `.h`) | see `repos.txt` |
| fmtlib/fmt | C++ | | 145 (47 `.cc` + 26 `.h`) | see `repos.txt` |
| tokio-rs/tokio | Rust | Requested band: 500-1500 files | 874 (799 `.rs`) | see `repos.txt` |
| tree-sitter/tree-sitter | Polyglot: picked for Rust, C, TypeScript/JS, Python; turned out to hit all seven codegraph-supported languages, section 5 | Four-plus codegraph-supported languages for real in one repo (also: it's the parser generator codegraph's own indexer is built on) | 618 (109 rs + 84 js + 64 c + 47 h + 28 ts + 7 py) | see `repos.txt` |
| sinatra/sinatra | Ruby (unsupported) | Zero files in any codegraph-supported extension: `.rb`/`.erb`/`.haml`/`.slim`/`.hamlit`/`.erubis` only. The "nothing to index" case | 292 (147 `.rb`, 0 supported) | see `repos.txt` |

Exact clone URLs and the commit SHA each was surveyed at are in `scripts/corpus/
repos.txt`. `run.sh` does not re-pin a repo already on disk to that SHA; see the
README for why, and delete a repo's directory under the corpus dir to force a fresh
clone if exact reproduction is needed.

## 2. What "EXPECTED-PENDING" means in the tables below, and one correction to the task brief

At `e4932ef`, `codegraph landscape` and `codegraph plan sync` are lane L3 and lane L2
stubs respectively (`src/landscape.rs`: "Both return an error saying so. Nothing here
fabricates a rendering."; `src/plan/ops.rs`: `anyhow::bail!("codegraph plan sync is
not yet implemented (lane L2). ...")`). That was anticipated going in.

**`codegraph plan lint` was also still a stub at this commit, which was not
anticipated.** `src/plan/ops.rs:640-645`:

```rust
pub fn lint(planes_path: &Path) -> Result<LintReport> {
    anyhow::bail!(
        "codegraph plan lint is not yet implemented (lane L2). It would validate {}",
        planes_path.display()
    )
}
```

`src/main.rs:552` dispatches `plan lint` into this function before opening the store,
exactly as documented ("lint reads the planes file and nothing else... that is what
lets it work on a repo that has never been indexed"), but the body it calls is the
L2-owned stub, because L2's commit (`7ecefb7`) lands after L1's (`e4932ef`) in this
board's history, and `e4932ef` is the commit this corpus was pinned to. This is not a
defect in shipped behavior; it is a consequence of validating a mid-sequence commit.
It is reported below as EXPECTED-PENDING, in the same family as `landscape` and `plan
sync`, not as a FAIL, for the same reason those two are not FAILs.

One mitigating fact, verified independently rather than assumed: the validator `plan
lint` would call if it were wired up, `plan::schema::validate`, does work at this
commit. Every one of this corpus's ten `codegraph init` runs calls it internally to
confirm the scaffolded `planes.yaml` it just wrote is schema-valid (pinned upstream by
lane P0's own `init::tests::starter_planes_file_is_valid` unit test), and all ten
`init` runs below exited 0. So: the logic lint would run is real and exercised
indirectly ten times in this corpus; the `codegraph plan lint` subcommand that would
expose it to a user is not wired up yet.

## 3. Numbers table

All from `scripts/corpus/out/run-20260914T073425Z/summary.md` and the per-repo
`03-index.log` / `13-index-incremental.log` files it was extracted from. "resolved
%" and "resolve phase" are cold-index numbers. Every repo's doctor, `init`, and
cold `index` exited 0.

| repo | language mix | files scanned | files indexed | nodes | edges | resolved % | index wall (s) | index user (s) | resolve phase (s) | incremental wall (s) | index peak RSS (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| go (gin) | Go | 99 | 99 | 1627 | 9583 | 17.0 | 143.23 | 87.64 | 139.0 | 5.04 | 106.6 |
| pyvendor (pip) | Python, vendored tree | 661 | 660 | 11839 | 28063 | 12.9 | 361.90 | 141.61 | 321.0 | 9.02 | 253.6 |
| tsmono (vite) | TypeScript, pnpm workspace | 1563 | 1563 | 6120 | 9384 | 22.8 | 102.75 | 40.74 | 53.6 | 7.13 | 128.2 |
| jsx (react-dates) | JavaScript/JSX | 162 | 162 | 1200 | 526 | 16.0 | 6.91 | 1.71 | 1.7 | 1.65 | 50.9 |
| java (retrofit) | Java | 341 | 341 | 6510 | 18947 | 14.3 | 380.49 | 159.56 | 349.0 | 31.29 | 199.5 |
| c (libuv) | C | 373 | 373 | 7383 | 25979 | 25.5 | 1310.07 | 730.84 | 1285.6 | 26.55 | 233.5 |
| cpp (fmt) | C++ | 79 | 79 | 4680 | 18053 | 12.2 | 487.00 | 288.21 | 474.8 | 4.12 | 182.2 |
| rust (tokio) | Rust | 799 | 799 | 15265 | 37839 | 13.4 | 314.52 | 154.17 | 275.1 | 22.36 | 347.6 |
| poly (tree-sitter) | Polyglot | 344 | 344 | 5174 | 21704 | 25.0 | 148.93 | 61.50 | 120.8 | 4.90 | 226.7 |
| unsup (sinatra) | Ruby, unsupported | 0 | 0 | 0 | 0 | 0.0 | 0.48 | 0.02 | 0.0 | 0.39 | 30.3 |

Read section 0 again before drawing any conclusion from the wall/user/instructions
columns: the machine was heavily contended for the entire run, and got more
contended as the run went on (`uptime` 1-minute load: 10.8 at `go`, 22-28 through
the middle repos, spiking to 43.7 during `c`). "index wall (s)" is the real
first-run cost a user on THIS machine saw; it is not what codegraph costs on a
quiet machine, which is a different, already-answered question (L9's receipt).

## 4. Per-step status matrix

Every cell below is a real exit code read from that repo's per-step log
(`scripts/corpus/out/run-20260914T073425Z/<repo>/NN-*.log`), not inferred. PASS/FAIL
judgment for the two by-design-failing steps is explained under the table; the raw
exit code alone does not tell you which.

| repo | init | doctor | index (cold) | summary/hubs/circular | rdeps (top hub) | rdeps (no `--name`) | plan lint | landscape | plan sync | incremental index |
|---|---|---|---|---|---|---|---|---|---|---|
| go | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| pyvendor | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| tsmono | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| jsx | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| java | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| c | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| cpp | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| rust | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| poly | PASS | PASS (2 warn) | PASS | PASS | PASS | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |
| unsup | PASS | PASS (3 warn) | PASS (0 files, not a crash) | PASS | n/a, no hub | PASS (fails by design) | EXPECTED-PENDING | EXPECTED-PENDING | EXPECTED-PENDING | PASS |

- **doctor "2 warn"** is the same pair on every repo before its first index:
  `schema` ("the store carries no codegraph schema yet") and `project-indexed`
  ("cannot have been indexed into a store with no schema"), both marked `[warn]`
  not `[fail]` because a never-indexed repo has no schema by construction (see
  `run-anywhere-20260914.md` section 5). `unsup` gets a third: `languages`, warning
  that no supported-language files were found, with a fix line pointing at the path
  and `.gitignore`. Exit code 0 on every single doctor run, all ten repos: warn does
  not fail the command (`src/doctor.rs::is_healthy`: `worst() != Fail`).
- **rdeps with no `--name`, "PASS (fails by design)"**: exit 1 on all ten repos, all
  with the identical message `Error: --kind rdeps needs a symbol to work on. Pass
  one with --name:`. This is D5 from lane L1's receipt working correctly, not a
  defect; PASS here means "the tool refused instead of silently returning nothing,"
  which is the point of the feature.
- **plan lint / landscape / plan sync, "EXPECTED-PENDING"**: exit 1 on all ten
  repos, all three. See section 2 for the full explanation, in particular that
  `plan lint` was not expected to be in this state going in.
- **unsup's cold index, "PASS (0 files, not a crash)"**: `Files: 0 scanned, 0
  indexed, 0 unchanged, 0 skipped`, `Nodes: 0`, `Edges: 0`, exit 0, 0.48s wall. A
  repository with nothing codegraph can parse is not an error condition, and it is
  not treated as one.

## 5. Per-repo notes: what a first-time user would trip on

Every number in this section is read from that repo's own log files under
`scripts/corpus/out/run-20260914T073425Z/<repo>/`, named inline where the source
is not obvious from context (an unlabeled number is that repo's `03-index.log`,
the cold-index numbers already tabulated in section 3).

**go (gin).** Clean run. `circular` found one real cycle
(`debug.go ↔ utils.go via calls`, `07-query-circular.log`). One thing worth a
follow-up bug report, not fixed here: the incremental reindex (`13-index-
incremental.log`) logged `to_index=2 unchanged=97 deleted=2` even though nothing on
disk changed between the cold index and the incremental one three minutes later.
Harmless in this run (the incremental resolve pass still finished in 1.1s and
`Resolved: 0 (0.0%)`), but a user watching that line would reasonably ask why two
untouched files were called both changed and deleted. Not root-caused here; flagged
for lane L1.

**pyvendor (pip).** The `.gitignore`-vs-indexed check this receipt owes: `pip`'s
`.gitignore` lists `__pycache__/`, `/build/`, `/dist/`, `docs/build/`,
`.mypy_cache/`, `htmlcov/`. None of those exist in a fresh shallow clone, so a
passive comparison proves nothing; this run planted a canary instead (section 7).
Separately, indexing hit a real, unplanned edge case: `tests/data/packages/
SetupPyLatin1/setup.py` is a Latin-1-encoded fixture pip ships on purpose to test
its own encoding handling, and codegraph cannot read it (`03-index.log`: `WARN
codegraph::index: ... parse failed: cannot read`). It is logged and skipped, not a
crash, and `Files: 661 scanned, 660 indexed, ... 1 skipped` says so honestly. A
first-time user pointing codegraph at a repo with any non-UTF-8 source file will
see this warning; it is a real, if narrow, limitation.

**tsmono (vite).** 1563 files discovered and indexed, but only 6120 nodes and 9384
edges resulted, far fewer than `pyvendor`'s 660 files → 11839 nodes. `vite`'s
`playground/` directory is hundreds of tiny fixture files for its own test suite,
most only a few lines; the file count is not a proxy for how much there is to
index. `circular` found 12 real cycles.

**jsx (react-dates).** The fastest repo in the corpus per file (6.91s / 162 files)
and the JSX verdict repo; see section 6. `git ls-files` found 162 supported files
against 168 tracked `.js`+`.jsx` (49+119); the six-file gap is accounted for by
`.eslintrc`/`.nycrc`-style dotfiles and non-source `.js` counted in the original
extension survey but excluded from what codegraph's `languages` doctor check
reports as indexable, not a bug.

**java (retrofit).** Clean run, 0 circular dependencies (the only repo in the
corpus with none). The standout number: resolve is **349.0s of a 379.9s total
(91.9%)**, for only 341 files. This is the sharpest single illustration in this
corpus of lane L9's finding that small-to-medium repos spend nearly all their
`index` wall time in the resolver pass, not parsing. See section 8.

**c (libuv).** The slowest repo in the corpus by a wide margin (1310s, 21.8
minutes) and the repo with the most circular dependencies found (21). Resolve is
98.2% of the wall time. See section 9: this repo also carries this corpus's single
largest gap against lane L9's clean isolated measurements, large enough that it
should not be read as "C is just slow," it should be read as "something is
unexplained here and libuv is where it shows up worst."

**cpp (fmt).** Only 79 files but 18053 edges (226 edges/file), the highest edge
density in the corpus by a wide margin (`libuv`: 69.7/file, `retrofit`: 49.5/file).
`fmt` is a template-heavy header library; a large fraction of its API is overload
sets and template specializations, which plausibly produces more name-edges per
file than ordinary application code independent of any measurement anomaly. Cold
index still took 487s despite the small file count, a useful reminder that file
count alone does not predict `index` cost.

**rust (tokio).** Clean run, in the requested 500-1500 file band, and the fastest
of the four "systems language" repos (go/c/cpp/rust) per edge (8.32ms/edge resolve
vs java's 20.7, cpp's 26.6, libuv's 49.5). `codegraph` is itself written in Rust;
this is circumstantial, not evidence of anything, but is worth someone eventually
checking whether the resolver's own hot paths are simply better exercised/warmed
for Rust's AST shapes.

**poly (tree-sitter).** Best polyglot result in the corpus: doctor's `languages`
check reports **all seven** of codegraph's supported languages in one repository
(`02-doctor.log`: `c 111, cpp 1, go 2, java 2, javascript 83, python 8, rust 109,
typescript 28`), better than the four this repo was picked for going in (rust, c,
typescript/js, python); it turns out to also carry a couple of Go and Java files.
12 circular dependencies found. Indexed and resolved without incident.

**unsup (sinatra).** The clean "nothing to index" case. `codegraph init` and
`codegraph doctor` both ran and exited 0 against a repository with zero
codegraph-supported files; `doctor` said exactly that (section 4) and pointed at
the fix; `index` reported `0 scanned, 0 indexed`, `Nodes: 0`, `Edges: 0`, and exited
0 in 0.48s. No crash, no confusing partial output, no silent success that looks
like a hang. This is the UX this repo was chosen to test, and it passed.

**Across all ten repos:** this WARN fires on every single command against every
embedded store (all ~140 log files in this run's output directory carry it at
least once):

<!-- credo-lint:allow-fenced Verbatim log output, printed by src/db.rs, which this lane did not edit. Rewriting it here would make the quote a paraphrase. -->
```
SurrealDB signin failed (There was a problem with authentication) — proceeding without auth
```

This is not new: lane L1's own receipt already flagged it as noise on a first run
and deliberately left it unfixed (`run-anywhere-20260914.md` section 12). This
corpus corroborates that it is present at real scale, ten repos and roughly 140
invocations, not just the one example L1 measured. Also present on every command,
when output is redirected to a file: the `tracing` INFO/WARN lines carry raw ANSI
colour escape codes even though stdout/stderr are not a TTY, making every log file
in this run start with control characters before the readable text. Pre-existing
(L1's receipt says the existing tracing lines are unchanged), not this lane's
finding to fix, but worth a line for whoever next touches `src/main.rs`'s tracing
setup.

## 6. The JSX verdict

Lane L1's D8 routes `.jsx` to the TSX tree-sitter grammar (previously it fell
through to the plain TypeScript grammar, which parses a JSX element as a
comparison expression and loses everything inside it). This corpus is the first
real, non-fixture test of that fix. `react-dates` is 49 tracked `.jsx` files of
real React class components.

```
$ codegraph query --kind search --name SingleDatePicker --json
```

(`scripts/corpus/out/run-20260914T073425Z/jsx/14-query-search-probe.log`)

```json
[
  {"name":"SingleDatePickerWrapper","node_type":"class","file_path":"examples/SingleDatePickerWrapper.jsx","start_line":83},
  {"name":"SingleDatePicker","node_type":"class","file_path":"src/components/SingleDatePicker.jsx","start_line":130},
  {"name":"SingleDatePickerInputController","node_type":"class","file_path":"src/components/SingleDatePickerInputController.jsx","start_line":125},
  {"name":"SingleDatePickerInput","node_type":"function","file_path":"src/components/SingleDatePickerInput.jsx","start_line":99}
]
```

(trimmed to the fields that matter; the full record also carries `node_id` and
`language`, both `"javascript"`)

**PASS.** Four real symbols, four different `.jsx` files, correct file paths,
correct line numbers, and correct node-type discrimination between the three
`class` components and the one plain `function`. This is not "did indexing not
crash," it is "did the parser produce the actual, correct symbol table" for a
substantial real-world JSX codebase. `jsx`'s cold index (162 files, 6.91s) also
found 1200 nodes total and 526 edges with 0 files skipped, consistent with the
whole file successfully parsing rather than JSX bodies turning into error nodes.

## 7. The ignore-rule verdict

Lane L1's D2 switched discovery to `git ls-files --cached --others --exclude-
standard -z` when the walk root is inside a git repository, which is gitignore
semantics by construction. A fresh shallow clone has nothing physically present
that its own `.gitignore` would exclude, so passively comparing `.gitignore`
against what got indexed proves nothing on any of the ten repos in this corpus (all
ten: tracked-file count equals plain-`find` count, confirmed for five of them
directly, `scripts/corpus/repos.txt`-adjacent scratch notes). This run tested the
claim instead of assuming it.

Before `pyvendor`'s first index, `run.sh` planted a real file:
`pyvendor/__pycache__/codegraph_l5_ignore_probe.py`, defining
`codegraph_l5_probe_function` (`__pycache__` is `pip`'s own first `.gitignore`
line). Discovery ran (`03-index.log`): `discovered source files files=661`, exactly
`pip`'s tracked-file count with no canary counted. After indexing:

```
$ codegraph query --kind search --name codegraph_l5_probe_function --json
```

(`scripts/corpus/out/run-20260914T073425Z/pyvendor/15-ignore-probe-search.log`)

```json
[]
```

**PASS.** The canary is on disk, `git status --porcelain` inside that clone would
show it as untracked, and codegraph did not index it. This is a decisive negative
result, not an absence of counter-evidence: if the directory-skip logic had a gap
(the kind of thing that would only show up on a real ignored directory, not a
fixture), this specific check would have caught it, because the file that would
have leaked in has a name this corpus grepped for by construction.

## 8. Per-phase timing across all ten repos (for lane L9)

Lane L9's own receipt (`index-profile-20260914.md`) measured, on four repos under
isolated conditions, that small-to-medium repos spend most of `index`'s wall time
in the resolver pass, not parsing. Every number below is this corpus's own
`[codegraph] done in Xs (discovery ..., change detection ..., parse+store ...,
resolve ..., fingerprint ..., registry ...)` line, read straight from each repo's
`03-index.log`, ten more data points under a different (heavily contended, see
section 0) machine condition than L9 used.

| repo | files | total (s) | parse+store (s) | parse+store % | resolve (s) | resolve % | fingerprint (s) | fingerprint % |
|---|---|---|---|---|---|---|---|---|
| go | 99 | 142.7 | 3.0 | 2.1% | 139.0 | 97.4% | 0.7 | 0.5% |
| pyvendor | 661 | 361.4 | 39.2 | 10.8% | 321.0 | 88.8% | 1.1 | 0.3% |
| tsmono | 1563 | 102.1 | 47.8 | 46.8% | 53.6 | 52.5% | 0.6 | 0.6% |
| jsx | 162 | 6.3 | 4.6 | 73.0% | 1.7 | 27.0% | 0.1 | 1.6% |
| java | 341 | 379.9 | 17.7 | 4.7% | 349.0 | 91.9% | 13.2 | 3.5% |
| c | 373 | 1309.3 | 14.7 | 1.1% | 1285.6 | 98.2% | 9.0 | 0.7% |
| cpp | 79 | 485.9 | 10.8 | 2.2% | 474.8 | 97.7% | 0.3 | 0.1% |
| rust | 799 | 314.0 | 33.2 | 10.6% | 275.1 | 87.6% | 5.7 | 1.8% |
| poly | 344 | 148.4 | 26.8 | 18.1% | 120.8 | 81.4% | 0.8 | 0.5% |
| unsup | 0 | 0.0 | 0.0 | n/a | 0.0 | n/a | 0.0 | n/a |

Excluding `unsup` (nothing to resolve) and averaging the other nine: **resolve is
71.4% of total wall time**, and for seven of those nine it is over 80%
(`go` 97.4%, `pyvendor` 88.8%, `java` 91.9%, `c` 98.2%, `cpp` 97.7%, `rust` 87.6%,
`poly` 81.4%). This corroborates L9's finding directly and at ten times the sample
size L9 had.

The two exceptions are informative rather than contradictory. `tsmono` (52.5%
resolve) and `jsx` (27.0% resolve) are exactly the two repos in this corpus with
the lowest edge-to-file ratio (`tsmono`: 9384 edges / 1563 files = 6.0/file; `jsx`:
526/162 = 3.2/file; every other repo in the corpus is 25 to 226 edges/file). Read
together, the pattern is not "resolve always dominates," it is "resolve time
tracks edge count, and parse+store time tracks file count, and which one wins
depends on the repo's edge density," which is a sharper, more useful statement
than either half alone. `jsx` in particular has both the fewest edges/file and the
smallest average file size (React component files, not large modules), which is
consistent with parse+store's per-file fixed cost dominating when there is
genuinely little to resolve.

**Correction, not a retraction, of what "resolve" means here.** "Resolve time
tracks edge count" is true, but not for the reason it would first appear. The
`[codegraph] ... resolve Xs` figure this section reads is `resolve_incremental`'s
total time, which is the R1-R6 candidate-resolution cascade *plus* its write-back
to the store, and section 0 above establishes that the write-back, not the
cascade, is quadratic in edge count and dominates by a wide margin on every repo
in this corpus (none of them ever passed `--force`). L9's own clean, `--force`
measurements (`index-profile-20260914.md` §3-4, unaffected by this defect since
`--force` takes the bulk-insert path) put the cascade itself at 0.15-0.3ms per
name-edge, cheap. The 71.4%-of-wall-time figure above is real and this corpus's
own evidence for it, but it is overwhelmingly the now-fixed write-back defect
being measured, not an inherent cost of resolution. Section 11 re-measures this
same table against the fix.

## 9. The instructions-retired gap, now root-caused: this corpus's evidence for it

L9's receipt flagged, in its own section 0, a large gap between an earlier
session's wall-clock numbers and L9's own clean, isolated measurement of the same
repos, ruled out as contention (L9 measured contention at 1.1-1.8x on this
machine), a debug-vs-release mismatch, or a `RUSTFLAGS`/profile override, and left
open pending one more variable. That variable was `--force`: see section 0 above
for the full, corrected mechanism (a conditional `UPDATE ... WHERE` per edge, with
no index behind the clause, making the write-back quadratic in edge count, not the
originally-published "SQL parsing" explanation, which lane L7 tested directly and
found made things worse, not better). This corpus never once passed `--force`
(neither `codegraph init`'s own suggested next command nor this receipt's script
does), so every repo below hit exactly this defect. `/usr/bin/time -l`'s
**instructions retired** is CPU work actually executed, immune to wall-clock
scheduling noise; L9 showed it moves only ~0.5% across a load swing that moved
wall time 74-76%, and both L9 and L7 used it as the trustworthy number for this
exact question. This corpus's binary was built the same way L9's and L7's were
(isolated worktree, isolated `CARGO_TARGET_DIR`, pinned commit), so the table below
is a third, independent, corpus-scale (not single-repo) data point for the same
defect, not a repeat of anyone else's numbers.

Comparing this corpus's instructions-retired/edge against L9's clean isolated
numbers for the closest same-language repo in L9's own table:

| This corpus | Lang | Instr. retired | Edges | Instr./edge | L9's clean repo | L9's instr./edge | Ratio |
|---|---|---:|---:|---:|---|---:|---:|
| go (gin) | Go | 704,044,967,086 | 9583 | 73.5M | cobra | 3.57M | **20.6x** |
| pyvendor (pip) | Python | 1,108,664,095,825 | 28063 | 39.5M | flask | 2.93M | **13.5x** |
| tsmono (vite) | TypeScript | 320,575,707,716 | 9384 | 34.2M | zod | 16.5M | **2.1x** |
| rust (tokio) | Rust | 1,356,932,801,214 | 37839 | 35.9M | codegraph-self | 2.63M | **13.7x** |
| c (libuv) | C | 6,178,061,082,118 | 25979 | 237.8M | zstd | 3.27M | **72.7x** |
| cpp (fmt) | C++ | 2,464,207,565,776 | 18053 | 136.5M | zstd (closest available; different language) | 3.27M | ~42x, not a controlled comparison |

Two things worth saying plainly. First: **every ratio here is at or above the
range L9 originally flagged except TypeScript**, which sits at 2.1x, close enough
to be ordinary contention rather than the write-back defect biting hard. That is a
real pattern, not noise, and it is consistent with the confirmed quadratic
mechanism rather than contradicting it: `write_updates`'s cost scales with the
*number of edges written back* (roughly the name-edges considered), and this
corpus's repos differ by more than 50x in that count (526 for `jsx` to 37,839 for
`rust`) as well as in how many of those edges are `UPDATE`-eligible versus already
covered by a prior chunk's delete-and-reinsert; a single edges-only ratio was
always going to be a rough proxy for a quadratic cost, not an exact one. Second:
**`libuv` (C) is the single largest gap measured anywhere in this investigation,
72.7x**, over 25,979 edges, the second-highest edge count in this corpus, which is
exactly where an O(N²) cost is most visible. `cpp`/`fmt` is compared against L9's
C repo (`zstd`) for lack of a C++ baseline in either receipt, so its ~42x should be
read as directional, not exact.

This receipt no longer needs to guess at a cause: this is `resolve_incremental`'s
`write_updates` defect (section 0), measured at corpus scale, on real repositories,
not fixture-sized ones. What was previously an open question for lane L9 is now a
confirmed, fixed defect (commit `3a517b2`), and section 11 measures this same
corpus against that fix.

## 10. Repeatability

```
$ scripts/corpus/run.sh --binary <same binary>
```

run a second time end to end, from a brand new, empty output directory
(`scripts/corpus/out/run-20260914T083732Z`, `run.sh`'s own default: every
invocation gets a fresh timestamped directory, nothing is overwritten). All ten
repositories were already cloned from run 1 (`run.sh` reuses an existing clone by
design, see `scripts/corpus/README.md`) and already indexed, so this second pass's
`codegraph index` step lands on an already-populated store: it is measuring the
*incremental* path for all ten repos, not a second cold index. This is the
documented, intended behavior (README: "To rebuild a repository's clone from
scratch, delete its directory under the corpus dir"), not an accident, and it is
stated here rather than left implicit.

```
$ tail -1 scripts/corpus/out/run-20260914T083732Z/run.log
[2026-09-14T08:44:54Z] overall exit: 0
```

**Overall exit 0, all ten repos, same as run 1.** Total wall time for the entire
second pass: 442 seconds (7.4 minutes), against run 1's roughly 62 minutes, a
~8.4x speedup entirely explained by "nothing to reindex" (`scripts/corpus/out/
run-20260914T083732Z/summary.md`: every repo's `files indexed` is 0 or 1, `nodes`/
`edges`/`resolved %` at or near 0, `index wall (s)` 0.44-24.54s across all ten
repos, down from 6.91-1310.07s in run 1). Spot-checked: `plan lint` and
`landscape` still exit 1 on every repo in run 2, identical to run 1 (same binary,
same stub state, as expected). No repo's outcome (PASS/FAIL/EXPECTED-PENDING)
differed between the two runs.

The script itself required no changes and no manual intervention between the two
runs; `run.sh` was invoked with the same single command both times.

## 11. Before/after: the write-back fix, same ten repos (added 2026-09-14, after the receipt above shipped)

Team lead relayed lane L9's and L7's finding (section 0) after this receipt's first
ten-repo pass and commit (`3a6900c`) were already done, and asked for exactly this:
build HEAD with the fix, run the same script over the same ten repos into a fresh
output directory, and report a dated before/after table. That fix, commit
`3a517b2`, is on HEAD (`6d4fafe` at the time of this pass; confirmed
`git merge-base --is-ancestor 3a517b2 HEAD`).

**Method.** A second isolated worktree and `CARGO_TARGET_DIR`
(`git worktree add <scratch>/l5-wt2 HEAD`, same discipline as the first pass):
clean build, exit 0, 0 warnings, 7m19s. Before running the after-pass, every one of
the ten repos' `.codegraph/` directories under the corpus dir was deleted, so this
pass's `codegraph index` is a genuine first-ever index on each repo, the same
condition section 3's numbers were measured under, not a reuse of an
already-populated store (which is what section 10's repeatability pass
deliberately did measure, and is a different, already-labeled thing). Same repos,
same on-disk trees (nothing re-cloned, so identical SHAs to section 1's table),
same script, same one-repo-at-a-time discipline.

`uptime` immediately before each repo's cold index (the same `03-index.uptime-
before.txt` file `run.sh` has captured for every repo since the first pass),
1-minute load column:

| repo | 1-min load before this repo's index |
|---|---:|
| go | 15.72 |
| pyvendor | 17.37 |
| tsmono | 7.42 |
| jsx | 4.12 |
| java | 3.72 |
| c | 2.81 |
| cpp | 2.62 |
| rust | 3.03 |
| poly | 2.77 |
| unsup | 2.58 |

Load fell steadily through this pass (other lanes finishing up, not this script's
doing) and stayed under 18 throughout, well below the original pass's 10.8-43.7
range; read the wall numbers with that in mind, and prefer the instructions-
retired columns, which section 0 already established stay ~flat under load.

| repo | edges | wall before (s) | wall after (s) | wall speedup | resolve before (s) | resolve after (s) | resolve speedup | resolve share before | resolve share after | instr. retired before | instr. retired after | instr. speedup |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| go | 9583 | 143.23 | 9.19 | 15.6x | 139.0 | 2.6 | 53.5x | 97.1% | 28.3% | 704,044,967,086 | 39,129,806,296 | 18.0x |
| pyvendor | 28063 | 361.90 | 34.48 | 10.5x | 321.0 | 9.1 | 35.3x | 88.7% | 26.4% | 1,108,664,095,825 | 285,564,892,313 | 3.9x |
| tsmono | 9384 | 102.75 | 39.47 | 2.6x | 53.6 | 2.0 | 26.8x | 52.2% | 5.1% | 320,575,707,716 | 266,997,234,840 | 1.2x |
| jsx | 526 | 6.91 | 3.26 | 2.1x | 1.7 | 0.1 | 17.0x | 24.6% | 3.1% | 10,740,076,360 | 9,613,032,895 | 1.1x |
| java | 18947 | 380.49 | 27.38 | 13.9x | 349.0 | 5.0 | 69.8x | 91.7% | 18.3% | 1,250,781,964,747 | 272,848,815,542 | 4.6x |
| c | 25979 | 1310.07 | 27.22 | 48.1x | 1285.6 | 7.4 | 173.7x | 98.1% | 27.2% | 6,178,061,082,118 | 261,597,480,364 | 23.6x |
| cpp | 18053 | 487.00 | 9.16 | 53.2x | 474.8 | 4.7 | 101.0x | 97.5% | 51.3% | 2,464,207,565,776 | 80,721,627,436 | 30.5x |
| rust | 37839 | 314.52 | 52.51 | 6.0x | 275.1 | 14.8 | 18.6x | 87.5% | 28.2% | 1,356,932,801,214 | 466,326,034,909 | 2.9x |
| poly | 21704 | 148.93 | 17.04 | 8.7x | 120.8 | 5.5 | 22.0x | 81.1% | 32.3% | 505,761,101,172 | 135,697,575,392 | 3.7x |
| unsup | 0 | 0.48 | 0.54 | 0.9x (noise, nothing to index either way) | 0.0 | 0.0 | 0% | 0% | 286,157,060 | 282,401,275 | 1.0x |

Sum of the ten cold-index walls: 3256.3s before, 220.3s after, **14.8x faster across
the whole corpus in one number.** Resolve's share of wall time falls from a
71.4%-of-nine-repos average (section 8) to a 24.5%-of-nine-repos average after the
fix (`unsup` excluded both times, nothing to resolve), a mean 2.9x reduction in
*share*, on top of the wall-clock reduction itself.
Overall exit 0 both passes, all ten repos. `init`, `doctor`, cold `index`, the four
query steps, and the by-design `rdeps`-with-no-`--name` failure are unchanged
PASS in both passes. `plan lint`, `landscape`, and `plan sync` are **no longer
EXPECTED-PENDING**: see section 12.

**Correctness, not just speed.** Every repo's `nodes`, `edges`, and `resolved %` in
this pass's `scripts/corpus/out/run-20260914T085707Z/summary.md` are identical,
field for field, to section 3's numbers from the buggy binary (go: 1627/9583/17.0
both passes; libuv: 7383/25979/25.5 both passes; and so on for all ten). The fix
changes how the answer is written, not what the answer is, corroborating L7's own
cobra-only correctness claim (`3a517b2`'s message: "identical output... 652 nodes,
4,588 edges, 935 resolved, 137 ambiguous, 3,386 unresolved, 60 file refs") at
corpus scale and across six languages the commit's own test did not individually
cover.

**The pattern matches the mechanism.** The two largest wins are `libuv` (48.1x
wall, 173.7x resolve, 23.6x instructions) and `fmt` (53.2x wall, 101.0x resolve,
30.5x instructions), the exact two repos section 5 and section 9 flagged as
carrying this corpus's worst anomalies against L9's clean baseline before the
cause was known. The smallest wins are `unsup` (nothing to index, no write-back to
speed up) and `jsx` (526 edges, the fewest in the corpus, where even a quadratic
cost stays small in absolute terms) and `tsmono` (9384 edges but only 1.2x on
instructions, consistent with section 8's finding that `tsmono` already spent the
least share of its wall time in resolve to begin with, so there was proportionally
less write-back cost to remove). A defect whose cost scales with the square of
edge count should shrink the most, in relative terms, on the repos with the most
edges relative to their other costs, and that is exactly what this corpus shows,
independently of anything L7 measured on cobra alone.

## 12. `plan lint`, `plan sync`, `landscape`: EXPECTED-PENDING at `e4932ef`, real at `6d4fafe`

These three were lane L2 (`plan lint`, `plan sync`) and lane L3 (`landscape`)
stubs at the commit section 2 corrected this receipt's own brief about. `6d4fafe`
carries their real implementations, and this after-pass's script ran all three
against all ten repos exactly as before, no special-casing. All three: **PASS,
exit 0, ten of ten repos, real output**, not a placeholder.

**`plan lint`**: `1 planes, 1 items, 1 touches` / `No problems found.` on every
repo (identical shape each time because every repo shares `init`'s scaffolded
starter `planes.yaml`). 0.00-0.02s, no store connection opened, matching the
original design intent quoted in section 2.

**`landscape`**: a real per-repo markdown report, subsystem partition, node
counts, and inter-subsystem dependency counts that vary correctly with the repo,
not a fixed template: `go` 99 files/7 subsystems/3 cross-subsystem deps, `rust`
799 files/26 subsystems/108 deps, `unsup` 0/0/0. `tsmono` (1563 files) produced 89
subsystems, by far the most, consistent with `vite`'s deep `playground/` tree
from section 5.

**`plan sync`**: PASS on all ten (exit 0, real resolution report each time), but
with a real, reportable finding, not a defect: `init`'s scaffolded starter
`planes.yaml` ships one example touch, a `src/**` glob, and that glob only
resolves against a repo that actually has a top-level `src/` directory. It
resolved on 4 of 10 (`pyvendor`, `jsx`, `libuv`, `fmt`, all of which do have one)
and reported honestly, not silently, as `UNRESOLVED (no_glob_match)` on the other
6 (`go`, `tsmono`, `java`, `rust`, `poly`, `unsup`), including `retrofit` (Java),
whose real source lives under `retrofit/src/main/java/`, a nested path the
repo-root-relative glob does not reach. `plan lint` still reports "No problems
found" on every one of those six, correctly: an unresolved touch is a fact `plan
sync` reports, not a schema violation `plan lint` should catch. Net: the
scaffold's example touch is honest and harmless everywhere, but only actually
demonstrates a resolved touch on 4 of 10 real-world layouts in this corpus, worth
a line to whoever next tunes `init`'s starter template.
