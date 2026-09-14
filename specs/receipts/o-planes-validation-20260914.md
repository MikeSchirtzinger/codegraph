# Planes validation, 2026-09-14

Integration and validation pass for the 2026-09-14 work. This is the last gate
before the release: it verifies what was built, it does not build.

Some sections of the internal copy are omitted from this public copy where they
only discuss the internal task board; each omission is marked inline. Every number below names the command that produced it.

**Measured at `a39603a`**, 23 commits into `ac272a5..HEAD`. Lane L5 then landed
`18d3779` while this pass was writing up. That commit touches one file,
`specs/receipts/run-anywhere-corpus-20260914.md`, and no `src/` file, so the
binary measured here is still the binary the board's final commit builds, and
every result below stands at it. Board HEAD when this receipt was committed:
`18d3779`, 24 commits. The frozen-file check in section 2 was re-run at
`18d3779` and is still empty.

All measurements come from a private `CARGO_TARGET_DIR`
(`<scratchpad>/int-target`) and a clean worktree checked out at `a39603a`. The
board established at 05:55 that all lanes share
`CARGO_TARGET_DIR=/Volumes/SSD/cargo-target` and clobber each other's
`libcodegraph.rlib`, so nothing measured there is evidence. Machine load is
reported with every timing.

## 1. Verdict table

| # | Check | Verdict | Evidence |
|---|---|---|---|
| A1a | `src/canon.rs` and `src/index/fingerprint.rs` frozen | **PASS** | `git diff ac272a5 HEAD` empty; never touched by any commit; pinned hash asserted live |
| A1b | No author or generated-by trailers | **PASS** | 0 across 23 commits |
| A1c | Commit file ownership | **PASS with 3 noted crossings** | all three authorized by the status log |
| A1d | Dangling `7ecefb7` | **CONFIRMED UNREACHABLE, premise corrected** | not a byte-identical duplicate of `884cc02` |
| A2 | `src/main.rs` em dashes removed | **PASS** | 13 lines / 14 occurrences to 0; commit `4b04848` |
| A3 | Receipt signin entry corrected | **PASS** | commit `a39603a` |
| A4a | `credo-lint` on public text | **FAIL, 1 file** | one untracked internal note, pre-existing |
| A4b | Em dashes elsewhere in `src/` | **FAIL, 1 file** | `src/stats.rs` lines 38 and 57, user facing |
| B1 | Clean-HEAD release suite | **PASS** | 340 passed, 0 failed, 18 targets, 0 warnings |
| B2a | `init` / `doctor` / bare `index` on a fresh clone | **PASS** | 140 files, 1986 nodes, 9195 edges, 5.92s |
| B2b | `query hubs`, `rdeps --explain --json` | **PASS** | 19 chains present and non-empty |
| B2c | Chain verifies through `codegraph_verify_chain` | **PASS, with 2 negative controls** | 6 steps re-derived; both tampers caught |
| B2d | `plan sync` / `touching` / `collisions` / `blast` | **PASS** | `touching src/canon.rs` returns all 4 expected items plus CI-6 |
| B2e | `plan stale` expected 0 | **FAIL, 9 items** | 8 are untracked files, 1 is a real stale plan |
| B2f | `landscape` mermaid and json | **PASS** | both render, partition rule in the header |
| B2g | `context` twice, byte identical | **PASS** | same sha256 both runs |
| B2h | `clones` count and classification | **MEASURED, hypothesis refined** | 68 groups / 343 symbols |
| B2i | Second `index` is incremental | **PASS** | 140 unchanged, 0 indexed, 0.3s against 5.5s |
| B2j | No `SurrealDB signin failed` anywhere | **PASS** | 0 occurrences in the whole transcript |
| B3 | Determinism over MCP across fresh stores | **PASS, closed** | 6 independent stores, 1 distinct output on 2 surfaces |
| B4 | Serve latency, sub-100 ms bar | **PASS, quiet machine** | all 6 p50 under the bar, load 2.94 to 3.46 |
| B5 | This receipt, no tracked modifications left | **PASS** | see section 11 |

Two checks in the brief could not be run as written because their premise did
not hold. Both are recorded in section 10 rather than silently dropped.

## 2. Hard constraint 1: the frozen files

The single most important check on this board.

```
$ git diff ac272a5 HEAD -- src/canon.rs src/index/fingerprint.rs
(no output)

$ git log --oneline ac272a5..HEAD -- src/canon.rs src/index/fingerprint.rs
(no output)
```

The first shows the files are byte-identical at HEAD. The second is the
stronger statement: no commit in the range touched them at all, so this is not
a touch-and-revert that happens to land back on the same bytes.

The fixture hash guard is `1_946_827_007_203_589_284_u64` at `src/canon.rs:600`,
written with digit separators, which is why a grep for the bare digit string
used in the board text finds nothing in `src/`. It is asserted by
`canon::canon_tests::certificate_hash_is_stable_across_processes`, and that test
**ran and passed** in the suite in section 4. The hash is therefore confirmed
unmoved by execution, not only by file identity.

## 3. Commit hygiene

**Trailers.** `git log --format='%H%n%B' ac272a5..HEAD | grep -inE
'co-authored|generated|claude|session'` returns two lines, both ordinary prose:
the subject of `a31ddc3` ("make the generated context file byte stable") and a
line in `e4932ef`'s body describing a fixture whose `.gitignore` holds
`generated/`. Inspecting the last non-empty line of all 23 messages shows every
one ending in substantive prose. **No attribution trailers.**

**Ownership.** Three commits carry a file the section 4b table assigns to
another lane. All three are authorized by the status log and none is a
collision:

| Commit | File | Table owner | Why it is fine |
|---|---|---|---|
| `3d84be8` | `src/config.rs`, `src/init.rs`, `src/doctor.rs`, `src/landscape.rs`, `src/plan/ops.rs` | L1, L3, L2 | P0 wrote the 12 stubs there by design, log 02:35 |
| `884cc02` | `src/plan/mod.rs` | P0 | L2 adding module declarations for its own files |
| `660cdc2` | `src/plan/mod.rs` | P0 | L3 adding `pub mod export;` for its own file |

Two files are shared across lanes without appearing in the ownership table at
all: `tests/common/mod.rs` (written by L7 in `9d719ea`, then L4 in `7e496c5`)
and `.gitignore` (L1, routed by L8). Neither caused a lost edit, but both are
gaps in the table rather than clean ownership.

**Dangling `7ecefb7`.** Confirmed unreachable: `git branch -a --contains
7ecefb7` is empty and `git log --all --oneline | grep 7ecefb7` finds nothing.
The brief describes it as a duplicate of `884cc02`; it is not.

```
$ git diff 7ecefb7 884cc02 --stat
 specs/receipts/planes-core-20260914.md | 24 +++++++++++++++++++-----
 1 file changed, 19 insertions(+), 5 deletions(-)
```

All ten code and fixture files are identical; the receipt was expanded before
the surviving commit was made. It is a superseded earlier version, not an exact
duplicate. Nothing is lost by leaving it to be garbage collected.

## 4. Clean-HEAD test suite

Worktree at `a39603a`, `git status --short` empty, `touch src/lib.rs` before the
run so no stale test binary could execute.

```
$ CARGO_TARGET_DIR=<private> cargo build --release
Finished `release` profile [optimized] target(s) in 7m 33s
$ CARGO_TARGET_DIR=<private> cargo test --release --no-fail-fast
```

Load average was 17.94 at start and 10.14 at end. That affects wall time only,
not pass or fail.

| Target | Result |
|---|---|
| `unittests src/lib.rs` | 183 passed, 0 failed |
| `unittests src/main.rs` | 0 passed, 0 failed |
| `tests/clone_detection.rs` | 2 passed, 0 failed |
| `tests/deletion_tracking.rs` | 7 passed, 0 failed |
| `tests/explain.rs` | 23 passed, 0 failed |
| `tests/facade_integration.rs` | 3 passed, 0 failed |
| `tests/fixtures_integration.rs` | 9 passed, 0 failed |
| `tests/gate_specificity.rs` | 3 passed, 0 failed |
| `tests/incremental_reresolution.rs` | 6 passed, 0 failed |
| `tests/kill_test.rs` | 1 passed, 0 failed |
| `tests/landscape.rs` | 19 passed, 0 failed |
| `tests/mcp_tools.rs` | 29 passed, 0 failed |
| `tests/plan_ops.rs` | 10 passed, 0 failed |
| `tests/plan_schema.rs` | 21 passed, 0 failed |
| `tests/regression_defects.rs` | 4 passed, 0 failed |
| `tests/run_anywhere.rs` | 16 passed, 0 failed |
| `tests/structural_fingerprints.rs` | 4 passed, 0 failed |
| `Doc-tests codegraph` | 0 passed, 0 failed |
| **TOTAL** | **340 passed, 0 failed, 18 targets** |

**0 warnings** across build and test. `tests/gate_specificity.rs`, which L3
reported as not-run at 06:45, ran here and passed 3 of 3.

`unittests src/main.rs` reporting 0 is correct and is the evidence for L4's
account of the count change at 06:30: L1's module-tree fix removed the
duplicate compilation, so the binary no longer carries its own copy of the
library's unit tests.

## 5. End to end on a fresh clone

`git clone /Users/mike/dev/codegraph` at `a39603a`. `.codegraph/` in the clone
holds only `planes.yaml`, so the store genuinely starts absent. Binary is the
release build from the clean worktree.

**`codegraph init`** created `.codegraph/config.toml`, kept the existing
`planes.yaml` and said so, confirmed `.gitignore` already allows both, and
ended with `Next: codegraph index`. Exit 0.

**`codegraph doctor`** printed 11 checks: 9 `[ok]`, 2 `[warn]`. Both warnings
are correct for a repository that has not been indexed yet (no schema, project
not indexed) and both carry a `fix:` line naming `codegraph index`. It read 140
files via `git ls-files` and broke them down by language. Exit 0.

**`codegraph index`, bare, no flags.** This is the path L7's fix changes and
the one `init` recommends.

```
[codegraph] discovery starting at 0.0s
[codegraph]   140 source files found by git ls-files, so .gitignore is respected
[codegraph] change detection starting at 0.0s
[codegraph] parse+store starting at 0.0s
[codegraph] resolve starting at 3.4s
[codegraph] fingerprint starting at 5.3s
  Elapsed:     5.5s
  Files:       140 scanned, 140 indexed, 0 unchanged, 0 skipped
  Nodes:       1986
  Edges:       9195
  Resolved:    1991 (22.8%)   Ambiguous: 40 (0.5%)   Unresolved: 6717 (76.8%)
real 5.92
```

**`codegraph index` a second time** took `Elapsed: 0.3s`, `140 scanned, 0
indexed, 140 unchanged, 0 skipped`. Incremental works.

**`codegraph query --kind hubs`** returned the ranked hub list. The rows now
read `compute_dependencies (function): in:3 out:71 total:74,
src/graph/dependencies.rs`, which is the house-style separator from section 8
rendering in the real binary.

**`codegraph query --kind rdeps --name build_with_planes --explain --json`**
emitted `explanations` present and non-empty, 19 chains, the first with 6 steps.

**`codegraph landscape`** rendered both `--format mermaid` (11 subsystems, the
partition rule stated verbatim in the header) and `--format json`.

**`codegraph context` run twice** over one unchanged index produced
byte-identical files, sha256 prefix `454fe08b0412` both times. L3's determinism
fix holds against the real binary.

**No `SurrealDB signin failed` line appears anywhere in the transcript**, and
the transcript contains zero em dashes.

## 6. Evidence chains verify, and the verifier is not vacuous

One chain from the `--explain` run above, fed back unchanged through the MCP
tool against the live store:

```
$ codegraph_verify_chain {"chain": <chain 0 from the rdeps run>}
dependent: build_with_planes
verify: PASS (6 step(s) re-derived against the live index)
```

A verifier that passes everything proves nothing, so two negative controls:

| Tamper | Result |
|---|---|
| step 2 `node_id` set to 32 zeroes | **FAIL**, `step 2 (node_exists): no node with id "000...0"` |
| step 2 `file_path` changed to `src/canon.rs` | **FAIL**, step 2 names the stored node and the claimed node side by side |

Both are caught, and both name the failing step. The chain loop is real end to
end: CLI produces it, MCP re-derives it, tampering breaks it.

## 7. Determinism across independently built stores

The board left this open: L4 called rdeps group order across fresh re-indexes
still unstable at 07:05, L7 claimed its total orders closed it at 05:15. L7's
own test indexes into in-memory stores and calls the library directly, so it
does not settle L4's question about independently built on-disk stores over the
wire. This does.

`tests/fixtures/polyglot` indexed into six independent on-disk surrealkv stores
under one project id. The stores are confirmed different files on disk
(`diff -rq` between two of them reports differences). The query is the
ambiguous bare name `connect`, which matches two distinct symbols
(`api::app::connect` and `worker::connect`), so group order is exercised.

| Surface | Bytes | Distinct results across 6 stores |
|---|---|---|
| `codegraph_impact {"name":"connect"}` over MCP | 306 | **1** |
| `codegraph query --kind rdeps --name connect --include-ambiguous --explain --json` | 4388 | **1** |

**Determinism is closed.** L7's query-side total orders are the fix and they
hold on the surface L4 was worried about.

One methodology note: a first attempt at the second row used
`--include_ambiguous` with an underscore, which is not the flag. Every run
produced an empty file and all six "matched" at the sha256 of the empty string.
That is recorded here because an empty-file match looks exactly like a pass.
The numbers above are from the corrected flag and are non-empty.

## 8. Serve latency

Load average 2.94 at start, 3.46 at end, both below the board's quiet-machine
bar of 6. This closes the morning item L4 left open at 03:20, where the
unchanged binary disagreed with itself 1.64x to 2.68x under load average 20.

```
$ python3 scripts/bench/mcp_latency.py --requests 60 --cli-requests 15
```

Serve mode, one long-lived process, store opened once:

| Target | Tool | p50 | p95 |
|---|---|---|---|
| polyglot | `codegraph_search` | 0.35 ms | 0.54 ms |
| polyglot | `codegraph_impact` | 0.60 ms | 0.75 ms |
| polyglot | `codegraph_architecture` | 1.34 ms | 1.67 ms |
| self-src | `codegraph_search` | 4.97 ms | 5.44 ms |
| self-src | `codegraph_impact` | 44.87 ms | 46.11 ms |
| self-src | `codegraph_architecture` | 87.60 ms | 89.74 ms |

**All six p50 figures are under 100 ms, so the bar holds.** The worst case is
`codegraph_architecture` on the larger store at 87.60 ms p50, 89.74 ms p95 and
90.52 ms max, which clears the bar but with little headroom. That is the number
to watch as the store grows.

The same queries as separate CLI invocations, each reopening the store, ran at
450.87 ms p50 (polyglot search) and 1540.14 ms p50 (self-src rdeps), which is
the fixed store-open cost of Known limitations #3 and the reason serve mode
exists.

## 9. The incremental write-back fix, measured on this repository

L7's receipt measures the fix on cobra. This is an independent check on this
repository, with interleaved pairs on a quiet machine, because the board's own
measurement lesson is that only within-run interleaved pairs are valid.

Same binary, fresh store for every run, load average 2.82 at start and 3.03 at
end.

| Round | Mode | Wall | Reported elapsed | Resolve phase |
|---|---|---|---|---|
| 1 | bare `index` | 5.53s | 5.1s | 1.80s |
| 1 | `index --force` | 4.99s | 4.5s | 1.40s |
| 2 | bare `index` | 5.53s | 5.1s | 1.90s |
| 2 | `index --force` | 5.02s | 4.6s | 1.30s |

Bare `index` is now **1.10x** the wall time of `--force` and **1.37x** on the
resolve phase alone. Before the fix the same comparison on cobra was 40.6s
against 0.9s, roughly 45x on the resolve phase
(`specs/receipts/index-profile-20260914.md` section 8.4). The pathological gap
is gone. A modest residual is expected: the incremental path still runs change
detection and a different write shape.

## 10. Two brief premises that did not hold

Recorded rather than worked around.

**There is no test-count line in `README.md`.** The brief asked for the
"338 passed at `d95bad6` plus uncommitted work" sentence to be replaced with the
clean-HEAD number. `grep` for `338`, for `passed`, and for any suite or target
count finds nothing. L6's `16b9923` had already removed the by-sha labels and
replaced them with pointers to this receipt, and `5b75c33` had already rewritten
Known limitation #9 as fixed in `3a517b2` with before and after instruction
counts. So README needed no edit and did not get one. The clean-HEAD number
those pointers promise is section 4 of this receipt.

**`plan stale` does not return 0 on a fresh clone.** It returns 9 items and 10
actionable touches. See section 12; this is a real finding, not a measurement
error.

## 11. Working tree at the end

```
$ git status --short
(no tracked modifications)
```

While this pass was measuring, `specs/receipts/run-anywhere-corpus-20260914.md`
showed as modified; that was lane L5 editing its own receipt, and L5 has since
committed it as `18d3779`. **This pass left no tracked modifications behind.**
All three of its commits were made by pathspec
(`git commit -F <msg> -- <paths>`), which is why L5's concurrently modified
receipt was never swept into any of them.

## 12. Findings handed on, not fixed here

Everything below is outside this pass's write scope. None of it is fixed.

1. **`src/stats.rs` prints two em dashes in user-facing output**, lines 38 and
   57, reached from `src/main.rs:146`. Board constraint 5 covers them. This is
   the same defect class L6 handed over for `src/main.rs` and it was missed by
   every lane because `src/stats.rs` appears nowhere in the section 4b
   ownership table. Two-line fix.

2. **`plan stale` reports `done` items as actionable, and a fresh clone is not
   a clean tree.** Of the 10 flagged touches, 9 point at files that exist in
   the maintainer's working tree but are **not tracked in git** (five untracked
   notes and specs; the filenames are omitted from the public copy). In the
   main working tree `plan stale`
   returns 0, which is what L2 and L6 measured and documented; in a clean
   checkout, which is what any other machine or agent gets, it returns 9. The
   same effect moves `plan sync` from the documented 79 resolved selectors to
   69 resolved and 10 unresolved.

   The tenth is genuinely stale and is the feature working: `RF-8` touches
   `symbol: write_updates`, and L7's `3a517b2` deleted that symbol. But RF-8's
   status is already `done`, and `plan stale` has no status filter
   (`src/plan/ops.rs:1224` filters only on `is_unresolved()`, while
   `collisions` correctly filters to `Status::Active` at `:1091`). A completed
   item pointing at a symbol the completed work deleted is the expected end
   state, not an actionable stale plan. Excluding `done` items would remove
   this false positive.

3. **One untracked internal note fails `credo-lint` hard**, 14 em dash lines.
   It is unchanged from `ac272a5`, so this is pre-existing, not a regression.
   README is 0 hard, 2 warn. Details of the internal notes are omitted from the
   public copy.

4. **`6d4fafe` left `pub fn as_tag` misindented** at `src/index/resolve.rs:93`,
   8 spaces where the rest of the impl block uses 4, from removing the
   `#[allow(dead_code)]` line above it. Cosmetic, compiles, 0 warnings.

5. **`verify_chain` leaks Rust `Debug` formatting onto the wire.** The
   file-path tamper in section 6 returns a reason string containing
   `ExplainNode { node_id: "...", name: ..., start_line: Some(786) }`. It is
   readable, but it is a struct dump in an agent-facing field.

6. **Clone-count classification, refining L6's "not root-caused" label.** At
   `a39603a`, `codegraph clones` reports **68 groups, 343 symbols** (`--min-edges 3`);
   README records 66/341 at the earlier tree. Classifying every group by where
   its members live:

   | Category | Groups | Members |
   |---|---|---|
   | every member is test code | 25 (37%) | 151 (44%) |
   | every member is production code | 26 (38%) | 82 (24%) |
   | mixed | 17 (25%) | 110 (32%) |

   Members under `tests/fixtures/` account for **9 of 343, 2.6%**.

   So the orchestrator's hypothesis is **confirmed in substance and wrong in
   location**. Structurally similar test code does dominate: the two largest
   groups in the whole report are 26 members of `index::resolve::tests::*` and
   23 members of `canon::canon_tests::*`. But that test code lives in `src/` as
   in-module `#[cfg(test)]` blocks, not in `tests/` or `tests/fixtures/`, so a
   classification that keys on the directory would have concluded the opposite.
   The largest all-production group is 13 members of `index::Progress::tick`
   (`src/index/mod.rs`), which lane L1 added tonight for the progress feature.

7. **`plan collisions` now reports 1 pair, not 0.** `RA-7` and `RA-8` both
   touch `README.md`, among 3 active items. L2 and L6 measured 0 among 5
   active; L6's status updates changed which items are active. The detector is
   working, and the pair it found is real.

8. **`plan touching src/canon.rs` returns 5 items, not the 4 the brief
   expected**: CI-1, CI-2, CI-5, RH-1 and CI-6. CI-6 is the shape-N item L6
   added tonight. All four expected items are present and bound.

## 13. Final HEAD, 2026-09-14, after the close-out commits

Sections 1 through 12 were measured at `a39603a`. Five more commits landed
after that: four close-out fixes, three of which act on findings in section 12,
plus one description change. This section re-validates at the end of them.

**Full suite re-run at `47811f3`**, clean worktree, private `CARGO_TARGET_DIR`,
`touch src/lib.rs` first, load average 4.80 at start and 7.27 at end:

```
$ cargo test --release --no-fail-fast
TOTAL: 345 passed, 0 failed, 18 targets, 0 warnings
```

Up 5 from the 340 of section 4, and each one is a close-out fix's own test:
lib 183 to 185, `explain` 23 to 24, `plan_ops` 10 to 11, `run_anywhere` 16 to
17. `canon::canon_tests::certificate_hash_is_stable_across_processes` ran and
passed again, so the frozen fixture hash is still confirmed by execution.

**Landed after the validated sha, inspected and classified:**

| Commit | Files | Class |
|---|---|---|
| `57819ad` | `src/mcp/server.rs`, `specs/receipts/mcp-surface-20260914.md` | **docs-in-code**, non-behavioral |

`57819ad` changes the `codegraph_plan_stale` tool description and one output
summary line. The diff contains no logic, only prose. `cargo test --release
--test mcp_tools` was re-run at `57819ad`: **29 passed, 0 failed**. Total
therefore stands at 345 for the tree at `57819ad`.

**Final HEAD this pass stands behind: `57819ad`.** 33 commits in
`ac272a5..HEAD`. Frozen-file check re-run there and still empty; trailer check
re-run there and still zero.

### 13.1 Em dashes in `src/`

Fixed here in `c3eaf0d`: `src/stats.rs:38` and `:57`, the two user-facing lines
section 12 item 1 reported. Also straightened `pub fn as_tag` at
`src/index/resolve.rs:93`.

The repo-wide count is **not** zero and the honest number is worth stating.
Across every `.rs` under `src/`: **4 lines carry an em dash inside a string
literal**, and 320 carry one in a comment or doc comment, which no user reads.
Of the 4:

| Location | Reachable? |
|---|---|
| `src/canon.rs:601` | test only, inside `#[cfg(test)]` from line 443 |
| `src/graph/dependencies.rs:830` | test only, inside `#[cfg(test)]` from line 708 |
| `src/index/fingerprint.rs:487` | test only, inside `#[cfg(test)]` from line 415 |
| `src/canon.rs:301` | **production**, an `assert!` message in `pub fn canonical`, no enclosing `cfg(test)` |

So the accurate claim is: **zero em dashes in reachable user-facing output,
with one exception that board hard constraint 1 forbids fixing**,
`src/canon.rs:301`. It is listed for Mike rather than touched.

`rustfmt` was deliberately not applied to `src/index/resolve.rs`. Passing a
path to `cargo fmt --check` after a bare double-hyphen separator does not
restrict it: the path filter is ignored and the whole crate is checked. Running
`rustfmt --check` on that one file shows it wants **37 hunks**, at lines 92,
745, 985, 1036, 1244 and 32 more from 1556 to 2080. Applying it would have buried a
one-line whitespace fix inside an unrelated reformat of a file edited all
night. A whitespace-only `cargo fmt` commit is a separate decision.

### 13.2 `plan stale` and `plan collisions` re-measured

L2's `47811f3` restricts `stale` to `planned` and `active` items, which is
section 12 item 2's first half. It works: RF-8 is `done` and touches
`write_updates`, the symbol `3a517b2` deleted, and it no longer appears.

**In the maintainer's working tree, `plan stale` is 0.** Indexed into a scratch
store so nothing was written to that repository:

```
$ codegraph index . --project-id miketree2 --db-url surrealkv://<scratch>
  Files: 140 scanned, 140 indexed      Nodes: 1998      Edges: 9354
$ codegraph plan sync  --project-id miketree2 --db-url surrealkv://<scratch>
  Selectors:   78 resolved, 0 ambiguous, 1 unresolved
  Why:         1 no_such_symbol
$ codegraph plan stale --project-id miketree2 --db-url surrealkv://<scratch>
=== Stale plans (0 items, 0 actionable touches) ===
Every touch in the roadmap still binds to live code.
```

The single unresolved selector is RF-8's `write_updates`, correctly held back
from `stale` because the item is done.

**In a fresh clone at the final sha, `plan stale` is 7 items and 8 actionable
touches**, from **4 distinct files**:

```
=== Stale plans (7 items, 8 actionable touches) ===
  8 no_such_file
```

The four files are untracked internal notes and specs; the table of filenames
is omitted from the public copy. All four exist in the maintainer's working
tree and none is tracked in git.

This is the feature working correctly on both sides: the files resolve where
they exist and are reported missing where they do not. One further item dropped
out of this list relative to section 12 because the file it touches has since
been committed and now resolves.

`plan collisions` on the fresh clone: **1 pair among 3 active items**, RA-7 and
RA-8, both touching `README.md`. Unchanged from section 12 item 7 and correct.

### 13.3 First run on a repository with no top-level `src/`

L1's `4212b36` derives the starter `planes.yaml` touches from the repository
instead of guessing. Checked against a layout nothing in this project resembles:
a copy of the retrofit Gradle monorepo from lane L5's corpus, a real git
repository with **no top-level `src/`** and 306 `.java` files spread across
nested modules.

```
$ codegraph init
  created  .codegraph/config.toml
  created  .codegraph/planes.yaml
  .gitignore  added .codegraph/* with negations for planes.yaml and config.toml
```

The generated starter item touches `file: README.md` and
`glob: "retrofit-adapters/**"`. `retrofit-adapters` is the correct choice: it
holds 120 supported source files, more than any other top-level directory
(`retrofit` 93, `retrofit-converters` 80).

```
$ codegraph index
  Files: 341 scanned, 341 indexed     Nodes: 6510     Edges: 18947    Elapsed: 28.1s
$ codegraph plan sync
  Written:     1 planes, 1 items, 143 touch rows from 2 selectors
  Selectors:   2 resolved, 0 ambiguous, 0 unresolved
  Touch rows:  143 resolved, 0 ambiguous, 0 unresolved
```

**0 unresolved, as intended.** A first `plan sync` on an unfamiliar repository
with an unfamiliar layout resolves everything it shipped.

### 13.4 Section 12 items now closed

| Item | Status |
|---|---|
| 1, `src/stats.rs` em dashes | **fixed**, `c3eaf0d` |
| 2, `plan stale` reports done items | **fixed**, `47811f3` |
| 3, internal note em dashes | **fixed**, `c8b1c6f`, 14 to 0 |
| 4, `resolve.rs:93` indent | **fixed**, `c3eaf0d` |
| 5, `verify_chain` Debug dump | **fixed**, `b8b4ecb` |
| 6, clones classification | measurement, no fix needed |
| 7, `plan collisions` 1 pair | correct behavior, no fix needed |
| 8, `plan touching` returns 5 | correct behavior, no fix needed |

Still open, and not a defect anyone could close in this pass: the four
untracked files in 13.2; the em dash in the frozen `src/canon.rs:301`; the 37
rustfmt hunks in `src/index/resolve.rs`; and `credo-lint`'s `ai-disclosure`
rule, whose only two remaining hard failures are both false positives in
untracked internal notes.

### 13.5 `plan` flag placement: checked, and there is no papercut

An earlier run disclosed that a `codegraph plan sync` ran without an honored
`--db-url` and wrote rows into this repository's own
`.codegraph/graph.db`. The suspected cause was flag placement, since P0 put
`--db-url` and `--project-id` on the `Plan` parent rather than on each
subcommand. Checked directly, against a fresh clone so nothing was written
anywhere that matters:

| Invocation | Parses | `--db-url` honored |
|---|---|---|
| `codegraph plan sync --db-url mem://` | yes | **yes**, connects to `mem://` |
| `codegraph plan --db-url mem:// sync` | yes | **yes**, connects to `mem://` |
| `codegraph plan stale --project-id zzz --db-url mem://` | yes | **yes** |
| `codegraph --db-url mem:// plan sync` | **no** | refused before it runs |

Both placements after the `plan` keyword work. `global = true` on the parent
does exactly what it is meant to, so the flag is accepted on either side of the
subcommand and is honored in both positions. **There is no flag-placement
papercut to record.**

The one invocation that fails puts the flag before the `plan` keyword, where it
is not a top-level option and never was. It fails loudly, before touching a
store, and the message already names the fix:

```
$ codegraph --db-url mem:// plan sync
error: unexpected argument '--db-url' found

  tip: 'plan --db-url' exists

Usage: codegraph [OPTIONS] <COMMAND>
```

That is the behavior wanted from a first-run error: it refuses, it says what
was wrong, and it prints the working form. No change needed.

So the disclosed write did not come from a misplaced flag. On the evidence
available here, an invocation simply omitted `--db-url` and took the documented
default, which is the repository's own store. The effect on the repository is
nil: `.codegraph/graph.db` is gitignored (`.gitignore:2`, `.codegraph/*`), so
no tracked file changed and `git status` is unaffected. The store's write ahead
log carries today's timestamp and the store is a derived artifact that
`codegraph index` rebuilds from scratch. Worth knowing, not worth fixing.

This pass never wrote to that store: every command it ran against Mike's
working tree passed `--db-url` pointing at a scratch path.
