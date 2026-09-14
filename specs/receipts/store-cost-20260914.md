# store-cost-20260914: RF-7 shipped, and RF-3 root-caused as the same bug

Baseline `dc9215f`, built in a dedicated worktree with its own private
`CARGO_TARGET_DIR` throughout.

## 5-line answer

RF-7 is fixed: a store now records the SHA-256 of each schema document it
carries, and the DDL is skipped when that hash already matches, so the 72
`DEFINE` statements in `schema.surql` and the 47 in `schema_plan.surql` run
once per store instead of once per invocation. **RF-3 is the same bug, and is
now closed.** `codegraph query` has called `db::init_schema` since `6b487e4`
on 2026-07-10, which landed hours after the July baseline was measured and
before the 2026-07-30 re-measurement. The DDL fires after the
`Connected to SurrealDB` line that splits the benchmark's phases, so all of
it landed in `phase_rest`, which is exactly where the 12 to 17x regression
was localized and where it has now gone: medium store `phase_rest` **1632 to
1985 ms before, 35.7 to 37.9 ms after, in the same session on the same
machine**. Warm medians are back within corpus growth of the July table.

## 1. What was wrong

`db::init_schema` re-executed `src/schema.surql` in full on every invocation
that opened a store, and `plan::store::ensure_schema` did the same with
`src/schema_plan.surql` before every plan read and every plan write. Both
documents are written entirely in `DEFINE ... OVERWRITE`, so re-running them
was idempotent and therefore invisible, but never free.

```
$ grep -c "^DEFINE" src/schema.surql src/schema_plan.surql
src/schema.surql:72
src/schema_plan.surql:47
```

`specs/receipts/index-profile-20260914.md` section 3.1 said "20-odd", which
undercounts the core document by a factor of three.

Seven call sites pay it: `Index`, `Resolve`, `Clones`, `Query`, `Plan`,
`Landscape` in `src/main.rs`, and `facade::open_store`.

## 2. The fix

`src/db.rs` gains `apply_schema(db, key, source)`, which reads the version
the store recorded for `key`, compares it to the SHA-256 of `source`, and
returns `SchemaInit::Skipped` without touching the DDL when they match.
`init_schema` is now `apply_schema(db, "core", ...)` and
`plan::store::ensure_schema` is `apply_schema(db, "plan", ...)`.

Three decisions worth naming.

**The version is the document's own hash, not a counter.** A constant a human
has to remember to increment is a constant that is eventually not
incremented, and that failure mode is a store running the previous release's
DDL forever. Hashing the document means editing a `.surql` file *is* the
bump. It also preserves the self-healing property both schema files' comments
rely on: adding a field changes the hash, so every existing store re-runs the
`OVERWRITE` definitions on its next command, and only a store already
carrying the byte-identical document skips, where the DDL was a no-op anyway.

**`schema_meta` is never `DEFINE`d.** It has to be readable before any DDL
has run against a brand new store. An undefined, schemaless table is exactly
that. Defining it in `schema.surql` would put the version record behind the
DDL it exists to skip.

**A failed version read means apply, not error.** This engine errors on a
read of an undefined table rather than returning nothing, which is the
property `doctor`'s schema probe already relies on, so an error is the
ordinary first-run case. Nothing is hidden by folding it in: the only
consequence of a wrong `None` is that the DDL runs, and a genuinely broken
store fails loudly on that DDL two lines later. That is not a theory. The
first build of this change used `type::thing`, which SurrealDB 3.0.1 renamed
to `type::record`; the read swallowed the parse error and the write reported
it in full on the first smoke run.

Untouched: `db::connect`, the signin skip, the single-writer lock path, and
`--force`, which is an `index` flag and has nothing to do with schema.

## 3. Tests

`tests/schema_version.rs`, five tests, all assertions on what
`apply_schema` returned rather than on how long it took, because a
wall-clock assertion on DDL that got faster is a flaky test on a loaded
machine.

| Test | Proves |
|---|---|
| `first_open_applies_the_core_ddl_and_the_second_skips_it` | fresh store applies, second and third opens skip |
| `a_version_mismatch_re_runs_the_core_ddl` | a store at another version re-applies, and records the new one |
| `a_missing_version_record_re_runs_the_core_ddl` | a pre-versioning store is not mistaken for a current one |
| `the_plan_schema_is_versioned_separately_from_the_core_schema` | neither document's version masks the other's absence |
| `a_skipped_store_still_answers_queries_against_every_defined_table` | all eight tables are really there after a skip |

```
$ CARGO_TARGET_DIR=$PWD/target cargo test --release --test schema_version
running 5 tests
test first_open_applies_the_core_ddl_and_the_second_skips_it ... ok
test the_plan_schema_is_versioned_separately_from_the_core_schema ... ok
test a_skipped_store_still_answers_queries_against_every_defined_table ... ok
test a_version_mismatch_re_runs_the_core_ddl ... ok
test a_missing_version_record_re_runs_the_core_ddl ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Behaviour on a real store, `RUST_LOG=codegraph=debug`, fresh surrealkv file:

```
1st index: INFO  codegraph::db: Schema core applied (version 28229bc9e5ba3d0e...)
2nd index: DEBUG codegraph::db: Schema core already at version 28229bc9e5ba3d0e..., skipping DDL
query:     DEBUG codegraph::db: Schema core already at version 28229bc9e5ba3d0e..., skipping DDL
```

## 4. Measurement method

Two binaries, both `cargo build --release` in this worktree with this
worktree's own `CARGO_TARGET_DIR`:

- **before**: `dc9215f`, clean tree, `Finished release profile in 12m 09s`,
  copied to `codegraph-baseline-dc9215f` before any source edit.
- **after**: the same tree plus this change.

`specs/receipts/index-profile-20260914.md` section 2 and the Jacquard
measurement work both found that only within-run interleaved pairs survive
this machine's load, so every A/B below alternates before and after back to
back against that binary's own warm store, five repetitions, medians
reported. Each store is seeded with one discarded `index` run first, because
RF-7 is a cost paid on a store whose schema already exists. One-minute load
average was 6.5 to 14 across the A/B runs, 42 during the first baseline
sweep, and is recorded per run below.

Corpora: this worktree's `src/` (44 Rust files), a shallow clone of
`spf13/cobra`, and a shallow clone of `pallets/flask`, both under
`/private/tmp/claude-502/storecost/`.

## 5. RF-7 before and after: wall time

`python3 /private/tmp/claude-502/storecost/ab.py`, interleaved, n=5, medians.

`codegraph index <repo> --project-id <id> --db-url <url> --force`:

| Store | before | after | ratio |
|---|---:|---:|---:|
| codegraph `src/` | 5727.1 ms | 4287.8 ms | 1.34x |
| cobra | 4782.8 ms | 3595.5 ms | 1.33x |
| flask | 5420.7 ms | 3983.3 ms | 1.36x |

`codegraph query --kind summary --project-id <id> --db-url <url>`:

| Store | before | after | ratio |
|---|---:|---:|---:|
| codegraph `src/` | 1956.2 ms | 224.2 ms | 8.73x |
| cobra | 1215.4 ms | 617.9 ms | 1.97x |
| flask | 1071.4 ms | 553.0 ms | 1.94x |

Raw runs, before then after:

```
codegraph-src/index  [6244.5, 5727.1, 6199.6, 5495.3, 5391.1]  [4287.8, 4018.9, 4152.6, 4829.6, 4795.7]
codegraph-src/query  [2080.3, 2186.8, 1956.2, 1267.9, 1290.4]  [ 264.6,  244.3,  223.1,  217.9,  224.2]
cobra/index          [3802.3, 4782.8, 4715.0, 4798.9, 8671.4]  [2810.0, 3481.2, 3595.5, 3600.1, 4389.8]
cobra/query          [1113.5, 1099.1, 1463.3, 1215.4, 1260.1]  [ 608.1,  662.0,  622.4,  606.0,  617.9]
flask/index          [5420.7, 4711.6, 5393.4, 6982.0, 5651.2]  [3850.4, 3819.5, 6913.4, 4030.5, 3983.3]
flask/query          [1005.9, 1048.1, 1071.4, 1098.0, 1134.2]  [ 538.5,  553.0,  554.2,  535.2,  561.2]
```

`index` improves by a flat 1.2 to 1.4 s regardless of repo, which is the
fixed tax section 3.1 of the profile receipt predicted, landing where it said
it would. It is a third of `index`'s wall time here and it would be nearly
all of it on a four-file repo. `query` is where a fixed cost has nothing to
hide behind.

## 6. RF-7 before and after: instructions retired

Wall time on this machine moved 75% under load while instructions retired
moved 0.5%, per the profile receipt. So the load-independent version, same
interleaving, `/usr/bin/time -l` around one `query --kind summary`, n=3,
medians:

| Store | before | after | ratio |
|---|---:|---:|---:|
| codegraph `src/` | 4,535,766,354 | 1,043,442,092 | 4.35x fewer |
| cobra | 2,842,674,535 | 758,288,882 | 3.75x fewer |
| flask | 3,051,334,953 | 748,367,674 | 4.08x fewer |

```
codegraph-src  before [4033092227, 4535766354, 4840724473]  after [1052018718, 1043442092, 1033477376]
cobra          before [2679749140, 2842674535, 3181048956]  after [ 752282419,  758288882,  765472422]
flask          before [2848198183, 3051334953, 3332419442]  after [ 751322676,  748367674,  745028147]
```

The after column varies by under 2% across repetitions while the before
column varies by 10 to 17%, which is itself evidence: the removed work was
both large and variable, and what remains is stable.

## 7. RF-3: the root cause, named

`codegraph query` calls `db::init_schema`.

```
$ git log --format='%h %ad %s' --date=iso -S 'Idempotent DDL, same as Index/Resolve run on connect' -- src/main.rs
6b487e4 2026-07-10 15:50:14 -0400 feat(facade): gate-facing facade + --json machine-readable query output
```

The July baseline in README Performance section 1 was measured at "working
tree state 2026-07-10, pre-commit, commit `c3658028` plus uncommitted
changes". `c365802` is dated 2026-07-09 19:15:39. So `6b487e4` landed after
the baseline and before the 2026-07-30 re-measurement, which is the window
RF-3 asks about.

`scripts/bench/store_open_cost.py` splits each invocation at the
`Connected to SurrealDB` line, which `db::connect` emits on its last line,
before `init_schema` is called. Every one of the 72 `DEFINE` statements
therefore fell into `phase_rest`. RF-3's own note says most of the
regression is "after the store opens, inside the query itself" and reports
medium-store `phase_rest` of 758 to 857 ms. It was not inside the query. It
was the schema DDL, running between the connect log line and the query.

The script's docstring asserted the opposite, in so many words: "`query`
and `stats` (unlike `index`/`resolve`) never call `db::init_schema`
(confirmed by reading src/main.rs's command dispatch), so phase_open here is
purely the embedded surrealkv engine's connect cost, not schema DDL." That
sentence was true when it was written and false from `6b487e4` onward, and
it is why two months of analysis read a schema cost as a query cost. It is
corrected in this commit.

### 7.1 The measurement that closes it

Same script, same reps, same machine, same session, baseline binary then
fixed binary.

```
$ python3 scripts/bench/store_open_cost.py          # binary: dc9215f, load 42.24
$ python3 scripts/bench/store_open_cost.py          # binary: + RF-7 fix, load 7.32
```

Warm medians, and the phase that RF-3 localized the regression to:

| Store | July 2026-07-10 published | `dc9215f` today | with RF-7 | medium `phase_rest` |
|---|---:|---:|---:|---|
| tiny (1 file) | 37.2 ms | 586.1 ms | **49.6 ms** | |
| medium (`src/`) | 58.5 ms | 2212.7 ms | **141.3 ms** | 1632-1985 ms to **35.7-37.9 ms** |
| large (fixtures) | 40.2 ms | 667.5 ms | **57.2 ms** | |
| floor (`--help`, no DB) | 7.4 ms | 15.1 ms | 7.8 ms | |

`phase_rest` on the medium store, per repetition:

```
before: 1632.9, 1631.2, 1909.3, 1874.5, 1985.1 ms
after:    37.9,   36.6,   35.8,   36.8,   35.7 ms
```

A 45x collapse in the exact phase RF-3 named, from removing the exact work
that phase was newly carrying.

### 7.2 The residual on the medium store, accounted for

tiny and large land within 1.3 to 1.4x of the July figures. Medium lands at
141.3 ms against 58.5 ms, 2.4x. That store is "this repo's `src/`", which is
not the same corpus it was in July:

| | July 2026-07-10 | today |
|---|---:|---:|
| files | 28 | 44 |
| nodes | 503 | 1387 |
| edges | 3,020 | 6,817 |

Measured today:

```
$ codegraph index src --project-id medcount --db-url surrealkv://.../graph.db --force
$ codegraph query --kind summary --project-id medcount --db-url surrealkv://.../graph.db
nodes 1387 (function 753, import 300, struct 146, impl 87, module 68, enum 28, macro_call 4, trait 1)
edges 6817 (calls 6253, contains 393, file_ref 94, imports 39, implements 38)
```

2.8x the nodes and 2.3x the edges for 2.4x the time, on three `GROUP BY`
aggregations that scan by `project_id`. The residual is the corpus, not a
regression. `phase_open` tracks the same thing: 43 to 55 ms on tiny, 100 to
110 ms on the larger medium store, against July's 40 to 46 ms on the smaller
one.

### 7.3 The other candidates, and what excluded each

| Candidate | Excluded by |
|---|---|
| surrealdb pin `=3.0.1` (`bdf123d`, 2026-07-10 15:50:25, a downgrade from 3.2.0, landing 11 seconds after `6b487e4` and also after the baseline) | Both binaries measured above are built against the identical `=3.0.1` pin, and the fixed one restores the July warm medians to within corpus growth. A version cost that survived the fix would still be there. It is not. |
| A query-shape change in `src/graph/*.rs` between the two dates | `project_summary`, the only function the benchmark's `--kind summary` calls, has not been modified since `893599e` (2026-07-08), before the baseline. `git log --since=2026-07-10 --until=2026-07-31 -- src/graph src/db.rs src/schema.surql` returns five commits, all gate, fingerprint, clone-detection or docs work, none on this path. |
| The per-invocation schema cost, that is RF-7 | Not excluded. This is the cause, section 7.1. |
| Machine contention | Already excluded by RF-3's own floor row, and again here: the fix is measured on the same machine in the same session as the baseline sweep, at a *lower* load than the baseline, and the floor row moved 15.1 to 7.8 ms while medium moved 2212.7 to 141.3 ms. |
| `--force` (RF-8) and the resolver write-back | Never on this path. The benchmark's timed command is `query`, which takes no such flag. |

RF-3's note says the July query-side figure "was not re-measured from an
isolated, SHA-pinned build, so it stands as published". It has now been
re-measured from an isolated, SHA-pinned build in a private worktree with a
private target directory, and it reproduces: `dc9215f` gives 586 to 2213 ms
warm against July's 37 to 59 ms. The regression was real. It was RF-7.

## 8. Gates

```
$ CARGO_TARGET_DIR=$PWD/target cargo build --release
Finished `release` profile [optimized] target(s)      0 warnings
```

Dash gate, run over what this lane added rather than over the whole tree,
because `src/` carries pre-existing long dashes in module and schema
comments that predate this branch by months and are not this lane's to
rewrite:

```
$ git diff -U0 | grep "^+" | grep -nE "<long dash>|<en dash>"
(no matches, exit 1)

$ grep -nE "<long dash>|<en dash>" tests/schema_version.rs specs/receipts/store-cost-20260914.md
(no matches other than this section's own quoting of the gate command)
```

Nothing this lane wrote, in source, tests, log lines, error strings or this
receipt, contains either character.

`cargo test --release` results are recorded in section 9.

## 9. Full test run

```
$ CARGO_TARGET_DIR=$PWD/target cargo test --release
```

| Binary | Result | Passed |
|---|---|---:|
| unittests `src/lib.rs` | ok | 185 |
| unittests `src/main.rs` | ok | 0 |
| `tests/clone_detection.rs` | ok | 2 |
| `tests/deletion_tracking.rs` | ok | 7 |
| `tests/explain.rs` | ok | 24 |
| `tests/facade_integration.rs` | ok | 3 |
| `tests/fixtures_integration.rs` | ok | 9 |
| `tests/gate_specificity.rs` | ok | 3 |
| `tests/incremental_reresolution.rs` | ok | 6 |
| `tests/kill_test.rs` | ok | 1 |
| `tests/landscape.rs` | ok | 19 |
| `tests/mcp_tools.rs` | ok | 29 |
| `tests/plan_ops.rs` | ok | 11 |
| `tests/plan_schema.rs` | ok | 21 |
| `tests/regression_defects.rs` | ok | 4 |
| `tests/run_anywhere.rs` | ok | 17 |
| `tests/schema_version.rs` (new) | ok | 5 |
| `tests/structural_fingerprints.rs` | ok | 4 |
| doc-tests | ok | 0 |
| **total** | **0 failed, 0 ignored** | **350** |

Exit 0, no `warning:` lines anywhere in the run.

## 10. What this lane did not touch, and what is now stale because of it

This lane writes only `src/db.rs`, `src/plan/store.rs`,
`scripts/bench/store_open_cost.py`, `tests/schema_version.rs` and this
receipt. Three surfaces outside it now say something that is no longer
true, and are the orchestrator's to change:

- **README Known limitations item 3** calls the 12 to 17x query-side
  regression open with "root cause not yet diagnosed", and **README
  Performance section 1** says "Root cause not yet identified. This is a
  known open regression". Both are now answered, with the numbers in
  sections 5 through 7 above. Performance section 1's July table is also
  no longer comparable to today's medium row without section 7.2's
  corpus-growth note beside it.
- **`.codegraph/planes.yaml`** carries RF-7 and RF-3 as `status: planned`.
  Both are done, and RF-3's note, which says "neither is this bug", is
  exactly wrong about RF-7: it is that bug.
- **`specs/receipts/index-profile-20260914.md` section 3.1** says
  "20-odd `DEFINE` statements". It is 72 in the core document and 47 in
  the plan document.
