# planes-core-20260914: receipt for lane L2

Lane L2 turned the eight
stubbed functions in `src/plan/ops.rs` into working commands over three new
tables, and ran them against this repository's own roadmap.

Every number below names the command that produced it. Nothing in this lane
returns a placeholder: a command that cannot answer returns an error.

## 1. What landed

| File | Role |
|---|---|
| `src/schema_plan.surql` | DDL for `work_plane`, `work_item`, `work_touch` |
| `src/plan/store.rs` | the three tables, their DDL loader, and the transactional replace |
| `src/plan/resolve.rs` | binds one `touches:` entry to the graph |
| `src/plan/ops.rs` | the eight commands |
| `tests/plan_ops.rs` | ten end to end tests |
| `tests/fixtures/planes/ops-{rust,rename,index-lag}.yaml` | the roadmaps those tests sync |

`src/plan/mod.rs` gained two `pub mod` lines and two doc lines. No other
lane's file was edited.

## 2. DDL execution

`db::init_schema` runs only `schema.surql`, and `db.rs` belongs to lane L1,
so `src/schema_plan.surql` is executed by `plan::store::ensure_schema` as a
single `include_str!` query before the first read or write of any plan
table.

Every definition is `DEFINE ... OVERWRITE`, matching `schema.surql` rather
than `IF NOT EXISTS`. The difference is load-bearing. `IF NOT EXISTS` leaves
a previously defined table alone, so a store created by an older build would
keep that build's field list forever and reject a row carrying a field added
later. `OVERWRITE` re-applies the current definition on every run, which is
what makes running it on every `sync` correct rather than merely harmless.
`OVERWRITE` redefines a table; it does not delete records, which is the
property `code_node` already depends on across incremental re-indexes.

Indexes, as the board requires: `project_id` on all three tables,
`work_touch.item` for the forward incidence lookup, and
`(project_id, work_touch.to_id)` for the inverse one. A UNIQUE index on
`(project_id, plane_id)` and on `(project_id, item_id)` guards the
uniqueness that `plan::schema::validate` already checks in the file, so a
roadmap cannot become duplicated in the store by any route.

## 3. Atomicity of `sync`

One `BEGIN TRANSACTION ... COMMIT TRANSACTION`, submitted as a single
multi-statement query, containing three deletes and up to three inserts. A
crash part way through leaves the previous roadmap intact rather than half
of the new one: there is no visible state between the delete and the insert.

No generation column and no flip. The driver gives real transactions on both
the embedded and the remote engine, so the weaker scheme would buy nothing
and would cost a column every reader has to filter on.

Validation runs before any of this. `schema::load` refuses to hand back a
document with any violation and its error lists all of them, so a malformed
file can never half replace a good one.

## 4. Touch resolution

`src/plan/resolve.rs`. Deterministic: same file plus same index gives the
same rows byte for byte. Every candidate list and every glob expansion is
sorted before it leaves the module, and no result is built by iterating a
`HashMap`.

**This is not the r1 to r6 cascade, deliberately.** The cascade is context
sensitive: R3 prefers a definition in the *calling file*, R4 narrows by that
file's import facts, R5 by its language family. A plan has no calling file.
`- symbol: sync` is written by a human in a YAML document, not captured at a
call site, so those three rules have no input. Running them here would mean
inventing a context, and an invented context is the kind of silent guess the
RESOLVED / AMBIGUOUS / UNRESOLVED vocabulary exists to refuse. What is
reused, by calling rather than copying, is the context-free part:
`index::resolve::normalize_separators` and `bare_name`, so a plan and a call
site agree on what `a.b.C` means.

Rules as implemented:

**`file:`** The path is normalized to the shape `code_node.file_path`
carries (forward slashes, no `./`, no leading or trailing slash) and looked
up in the union of `code_node.file_path` and `file_metadata.file_path`. The
second is what lets a file that parsed to zero symbols still count as
indexed. A hit is RESOLVED and binds to the path itself, since a path has no
node of its own in this graph. A miss is UNRESOLVED with one of three
reasons, which is one confidence with three different remedies:

| Reason | Condition | Remedy |
|---|---|---|
| `no_such_file_in_index` | on disk, codegraph has a grammar for it, no row | re-index |
| `file_type_not_indexed` | on disk, no grammar for its extension | none, and the plan is not stale |
| `no_such_file` | on neither disk nor index | fix the plan |

The middle one was added after the dogfood: all 23 unresolved touches in
this repo's roadmap are its specs, its README, and `src/schema.surql`.
Reporting those as "re-run codegraph index" would have been wrong advice 23
times out of 23, and counting them as stale work makes `plan stale`
unreadable on the repository it was built for. The grammar question is
answered by calling `index::parser::extension_to_language`, not by a second
copy of that table, so a grammar added there changes this answer on the same
commit. "On disk" is judged against `project_registry.root_path`, the root
the project was actually indexed at, falling back to the planes file's
grandparent directory when the project was never registered.

**`symbol:`** A raw containing `::`, `.`, or `/` is a qualified name and
gets an exact `qualified_name` lookup first. A bare raw skips that step,
because comparing a bare string against `qualified_name` can only ever match
a top level symbol the bare lookup already finds. Either way the fallback is
an exact `name` lookup on the bare tail. One match is RESOLVED, several are
AMBIGUOUS with every candidate node id listed and no pick, none is
UNRESOLVED with reason `no_such_symbol`. `import` nodes are excluded from
both lookups, the same exclusion `graph::dependencies::matching_roots`
applies to every name-anchored query: an import statement is a reference,
not a definition, and counting every importing file as a candidate would
make almost every symbol touch AMBIGUOUS for no information gain.

A failed qualified lookup falls through to the bare tail rather than
stopping. `Commands::Index` naming a variant whose computed
`qualified_name` is `cli::Commands::Index` is a spelling difference, not a
stale plan. The fallback can only widen the candidate set, and a widened set
that is not a singleton comes back AMBIGUOUS with everything listed.

**`glob:`** Expanded against indexed file paths. `**` matches zero or more
whole segments, `*` matches within one segment and never crosses `/`, `?` is
one character. At least one match is RESOLVED with one row per matched file;
none is UNRESOLVED with reason `no_glob_match`. The matcher is forty lines
in `resolve.rs` rather than a new crate (the board forbids new dependencies
without justification, and there is none for this); the within-segment part
is the standard single-backtrack linear algorithm, pinned by a test that
would hang an exponential one.

**One entry is not one row.** A glob matching four files is four incidences
on the hyperedge, which is what board section 3.2 defines a `work_touch` row
to be and what keeps collision detection and blast radius a plain set
intersection. Both numbers are reported everywhere, because neither is
derivable from the other: `selectors` counts entries as written,
`touches` counts incidences.

## 5. Build and tests

```
$ cargo build --release
Finished `release` profile [optimized] target(s) in 26.97s
```

Warnings: **11 before this lane, 0 after**, counted with
`touch src/lib.rs src/main.rs && cargo build --release 2>&1 | grep -cE "^warning"`.
The 11 at baseline belonged to lanes L1, L3, and L4, in flight at the time;
they fixed their own. This lane introduced none.

Measured in an isolated worktree checked out at this lane's commit, so the
numbers describe committed code rather than whatever the shared working tree
held at the moment the suite ran:

```
$ git worktree add --detach <scratch>/wt 7ecefb7
$ cd <scratch>/wt && cargo test --release
test result: ok. 152 passed; 0 failed   (lib)
test result: ok. 109 passed; 0 failed   (bin)
test result: ok.  20 passed; 0 failed   (tests/mcp_tools.rs)
test result: ok.  10 passed; 0 failed   (tests/plan_ops.rs)
... 13 further binaries, all ok ...
```

382 passed, 0 failed across all 17 test binaries.

The shared working tree at the same moment ran 426 passed, 2 failed. Both
failures are `verify_chain_rejects_a_forged_rule_and_a_forged_candidate_pool`
and `verify_chain_rejects_a_swapped_node_id_and_names_the_step` in
`tests/mcp_tools.rs`, and both come from another lane's uncommitted edits to
`src/graph/explain.rs`, `src/graph/dependencies.rs`, `src/graph/hub_nodes.rs`
and `tests/explain.rs`. Both pass at this lane's commit, shown above. This
lane touches none of those files.

### Fail first

Every test in `tests/plan_ops.rs` calls at least one of the eight functions.
To show they cannot pass on the stub tree, all eight bodies were mutated
back to `anyhow::bail!` (a scripted edit that inserts the bail as the first
statement of each function, leaving types and signatures untouched):

```
$ cargo test --release --test plan_ops
test result: FAILED. 0 passed; 9 failed
$ cargo test --release --lib plan::ops
test result: FAILED. 2 passed; 2 failed
```

Both `lint` unit tests and all nine integration tests failed with
`codegraph plan <cmd> is not yet implemented (lane L2)`. The bodies were
restored and both suites returned to green. The tenth integration test
(`the_three_file_reasons_are_told_apart_against_one_index`) was written
after this check and calls `sync` and `stale`, both of which bail on the
stub tree.

### What the ten tests pin

- `sync_binds_every_confidence_and_names_every_reason`: exact selector and
  row tallies, both partitions checked; the AMBIGUOUS entry's candidate list
  compared against the node ids of `alpha::helper` and `beta::helper`
  loaded from the store, and asserted to carry no pick.
- `the_three_file_reasons_are_told_apart_against_one_index`: a file written
  after the index ran, a file that never existed, and a file with no
  grammar, all UNRESOLVED, all with different reasons. Re-indexing clears
  exactly the one whose message said it would.
- `a_glob_becomes_one_incidence_per_matched_file`, `sync_twice_is_idempotent`
  (row counts, ids, and every reader's output identical across two syncs),
  `touching_finds_the_item_by_file_and_by_symbol`,
  `collisions_finds_the_intended_pair_and_not_the_decoy` (and not the
  `planned` item on the same file, nor the `abandoned` whole-tree glob),
  `stale_names_the_item_its_touches_and_their_reasons`,
  `list_and_show_read_back_what_sync_wrote`.
- `blast_returns_the_reverse_dependencies_the_manifest_documents`: the
  expected set is `tests/fixtures/rust/expected.yaml`'s cases b and
  bonus-r2, the only two RESOLVED callers of `db::connection::connect`, so
  this test and the resolver's own tests are pinned to one ground truth.
- `renaming_a_touched_symbol_makes_the_plan_stale`: the feature's reason to
  exist, end to end. A temporary copy of the fixture is indexed, the
  roadmap syncs with both touches bound, `log_startup` is renamed to
  `log_boot`, the copy is re-indexed, and the same unmodified roadmap
  re-syncs. The file touch still binds; the symbol touch is UNRESOLVED with
  reason `no_such_symbol`, and `plan stale` names the item.

## 6. Dogfood against this repository

Store under
`/private/tmp/claude-502/-Users-mike-dev-codegraph/055dbe68-1589-43c3-8946-f023302f0957/scratchpad/l2/graph.db`,
project id `codegraph-l2`, roadmap `.codegraph/planes.yaml` unmodified.

```
$ codegraph index . --project-id codegraph-l2 --tier full --force --db-url surrealkv://<scratch>/graph.db
  Files:       140 scanned, 140 indexed, 0 unchanged, 0 skipped
  Nodes:       1913
  Edges:       8690
  Elapsed:     6.4s
```

```
$ codegraph plan sync --project-id codegraph-l2 --db-url surrealkv://<scratch>/graph.db
=== Plan Sync ===
  Project:     codegraph-l2
  Source:      /Users/mike/dev/codegraph/.codegraph/planes.yaml
  Index:       140 files, 1913 symbols
  Written:     6 planes, 28 items, 221 touch rows from 61 selectors
  Selectors:   38 resolved, 0 ambiguous, 23 unresolved
  Touch rows:  198 resolved, 0 ambiguous, 23 unresolved
  Why:         23 file_type_not_indexed (no action possible)
```

### The confidence breakdown over 61 selectors

| Verdict | Selectors | Touch rows |
|---|---|---|
| RESOLVED | 38 | 198 |
| AMBIGUOUS | 0 | 0 |
| UNRESOLVED | 23 | 23 |

All 7 `symbol:` touches and all 5 `glob:` touches resolved. The 5 globs and
33 file touches expanded to 198 incidences.

### Every unresolved touch, classified

Enumerated from `plan sync --json`:

| Count | Path | Extension | Classification |
|---|---|---|---|
| 4 | `README.md` | `.md` | file type codegraph does not index |
| 2 | `src/schema.surql` | `.surql` | same |
| 1 | `specs/explain-v1.md` | `.md` | same |
| 1 | `specs/receipts/pub-prep-verify-20260730.md` | `.md` | same |
| 1 | `.codegraph/planes.yaml` | `.yaml` | same |
| 14 | internal notes and specs | `.md` | same |

The 14 internal rows are collapsed into one line here; their filenames are
omitted from the public copy. The counts and the classification are unchanged.

**23 of 23 are the `file_type_not_indexed` case.** Every one of those paths
exists on disk (checked by the resolver against
`project_registry.root_path`), and every extension is outside
`index::parser::extension_to_language`'s table. So:

- **Genuinely stale touches in `.codegraph/planes.yaml`: 0.** Nothing to
  route to L6.
- **Gaps in this lane's resolution: 0.** No path that codegraph could have
  indexed failed to bind.
- The remaining question is a product one, not a defect: codegraph indexes
  source code, and a roadmap legitimately points at specs and docs. Those
  touches record real intent that the graph has nothing to bind to. They are
  reported with a reason that says so and are excluded from `plan stale`'s
  actionable count.

### The other commands

```
$ codegraph plan stale ...
=== Stale plans (14 items, 0 actionable touches) ===
  23 file_type_not_indexed (no action possible)

$ codegraph plan collisions ...
=== Collisions among active work (0 pairs, 5 active items) ===
No two active items touch the same code.
```

Zero collisions is the correct answer here, checked by hand: the five
`active` items are RA-1 (`src/plan/schema.rs`, `src/cli.rs`, `src/main.rs`),
RA-3 (`src/init.rs`, `src/doctor.rs`, `src/config.rs`), RA-4
(`src/plan/ops.rs`, `sync`, `src/schema.surql`), RA-5 (`src/landscape.rs`,
`LandscapeFormat`, `src/context.rs`), RA-9 (`src/index/resolve.rs`,
`ResolutionTrace`, `AttemptOutcome`). Five disjoint sets, which is what the
board's own file-ownership table was designed to produce. The positive case
is covered by the fixture test, which finds exactly one pair and rejects
three decoys.

```
$ codegraph plan touching src/canon.rs ...
=== Planned work touching 'src/canon.rs' (4 items) ===
  CI-1 [research/planned] <title omitted from the public copy>
    via file: src/canon.rs [RESOLVED] src/canon.rs [bound]
  CI-2 [research/done] <title omitted from the public copy>
  CI-5 [research/planned] <title omitted from the public copy>
  RH-1 [fix/planned] <title omitted from the public copy>

$ codegraph plan touching canon_search ...
=== Planned work touching 'canon_search' (1 items) ===
  CI-1 [research/planned] ...
    via symbol: canon_search [RESOLVED] bf1f9560630b4ce8aba6865e0a85f7e0 [bound]

$ codegraph plan blast RA-4 --depth 2 ...
=== Blast radius of RA-4 (depth 2) ===
  84 seed(s) reach 11 node(s) in 2 file(s)
  1 touch(es) never bound, so this reach is a lower bound: src/schema.surql

$ codegraph plan lint
  6 planes, 28 items, 61 touches
  No problems found.
```

## 7. One deviation from the brief, measured

The brief said to call `graph::dependencies::get_reverse_dependencies` for
the blast walk. `blast` calls `graph::dependencies::reverse_dependency_paths`
instead, the pure core in the same module, over one load of the project's
nodes and edges.

Both are entry points onto one BFS. `reverse_dependency_paths`'s own
documentation states it mirrors the adapter's policy exactly: the same
`matching_roots` seeding, RESOLVED edges only, the same depth cutoff, the
same first-visit-wins rule, and it lives beside the adapter so the two can
be diffed in review rather than drifting. So this is still the codebase's
own walk, not a second copy of it.

The reason is that `get_reverse_dependencies` reloads every node and every
edge of the project **on each call**, and the blast seed set is unbounded: a
single `- glob: "tests/fixtures/**"` expands to every symbol in every
fixture file. Measured on codegraph's own index (1,927 nodes, 8,362
name-edges), 188 seeds through each path in one process:

| Path | Time for 188 seeds |
|---|---|
| `get_reverse_dependencies` per seed | 19.59 s |
| one load then `reverse_dependency_paths` per seed | 0.156 s |

125.8x. Measured with a temporary integration test that indexed this repo
into an in-memory store and timed both loops back to back; the test was
deleted after the measurement and is not part of the suite. The real
consequence: `plan blast RF-1` (188 seeds, from two selectors that expand to
73 incidences) returns in 0.69 s of user CPU. Through the adapter it would
have taken about twenty seconds on a 140-file repository, and minutes on a
1,573-file one.

Reverting this is a three-line change if the named API is preferred.

## 8. What this lane did not do

- `plan brief` and `codegraph landscape` are lane L3's, dispatched from
  `main.rs` to `landscape::brief`. Untouched.
- `lint` reports every rule violation together, but a file that cannot be
  **parsed** comes back as an error rather than as violations. There is no
  document to enumerate rules over, and `ViolationCode` (lane P0's) has no
  variant that could honestly describe a YAML syntax error.
- `collisions` considers only `active` items, which is the model's own rule
  (`Status::Active`: "Only active items participate in collision
  detection"). Including `planned` was offered in the brief and declined: a
  roadmap records far more planned work than anyone is doing, so including
  it would make almost every pair collide and bury the signal an agent is
  actually checking for. `CollisionsReport.active_items` reports the
  eligible population so an empty result is never ambiguous.

---

## 9. Follow-up, 2026-09-14: a file that exists binds

The dogfood in section 6 found 23 of 61 touches UNRESOLVED, every one of
them a spec, a README, or `src/schema.surql`: files that are present,
correct, and outside the set of file types codegraph has a grammar for. That
made `plan stale` report 14 items and 23 touches on a roadmap with nothing
wrong with it. A staleness signal that is 100 percent noise on the
repository it was built for is not a staleness signal.

The defect was in the design, not in the resolution. UNRESOLVED was carrying
two different facts: "the plan names something that is not there" and "the
code graph has no node for this". Only the first is a stale plan.

### What changed

A `file:` touch whose path exists in the working tree now binds **RESOLVED**
to that path, carrying a new boolean `indexed`: true when the graph holds a
row for it, false when it does not. `glob:` is expanded against the union of
the indexed paths and the working tree, one row per match, each with its own
`indexed`. Confidence keeps its exact meaning; the extra fact gets its own
column rather than being smuggled into the verdict.

UNRESOLVED is now reserved for what it should always have meant. The reason
vocabulary shrank to match: `no_such_file`, `no_such_symbol`,
`no_glob_match`, `ambiguous_candidates`. `no_such_file_in_index` and
`file_type_not_indexed` are gone, because nothing produces them any more and
a machine-readable field carrying dead vocabulary is worse than one that
does not.

The working tree list is found the way the indexer finds its own:
`git ls-files --cached --others --exclude-standard` first, so gitignore
semantics come for free and a file written a second ago is matchable, with a
directory walk and the same skip list as the fallback.
`index::discover_source_files` could not be reused because it filters to
languages with grammars, which is the exact filter that has to be lifted here.

Downstream: `stale` flags only file touches whose path does not exist,
symbol touches that did not bind, and globs that matched nothing. `touching`
and `collisions` work unchanged on unindexed paths, since the target is the
path either way. `blast` skips unindexed targets, because a path with no
node has nothing to walk from, and reports `skipped_unindexed` rather than
dropping them silently: a reach computed over fewer seeds is a lower bound.
`show` and `list` mark an unindexed touch with a plain "(not indexed)".

### Dogfood after the change

Same store, same 140-file index, same unmodified `.codegraph/planes.yaml`.

```
$ codegraph plan sync --project-id codegraph-l2 --db-url surrealkv://<scratch>/graph.db
=== Plan Sync ===
  Index:       140 files, 1913 symbols
  Worktree:    252 files, found by git ls-files, so .gitignore is respected
  Written:     6 planes, 28 items, 303 touch rows from 61 selectors
  Selectors:   61 resolved, 0 ambiguous, 0 unresolved
  Touch rows:  303 resolved, 0 ambiguous, 0 unresolved
  Not indexed: 105 of those resolved rows bound to a path the code graph holds nothing for

$ codegraph plan stale ...
=== Stale plans (0 items, 0 actionable touches) ===
Every touch in the roadmap still binds to live code.

$ codegraph plan collisions ...
=== Collisions among active work (0 pairs, 5 active items) ===
No two active items touch the same code.
```

| | Before | After |
|---|---|---|
| Selectors RESOLVED | 38 | **61** |
| Selectors AMBIGUOUS | 0 | 0 |
| Selectors UNRESOLVED | 23 | **0** |
| Touch rows | 221 | 303 |
| Rows bound to an unindexed path | not represented | 105 |
| Items reported stale | 14 | **0** |

The row count rose from 221 to 303 because a glob now sees the whole working
tree: `glob: "tests/fixtures/**"` picks up the fixture manifests and the
non-Rust fixture sources it always meant, and `glob: "scripts/bench/*.py"`
now matches at all. That is the same correction as the file case, applied to
globs.

`plan show RA-8`, which was two UNRESOLVED touches and is the clearest case,
now reads:

```
  Touches (2):
    file: README.md [RESOLVED] README.md (not indexed)
    file: .codegraph/planes.yaml [RESOLVED] .codegraph/planes.yaml (not indexed)
  Blast:       0 seeds reach 0 nodes in 0 files at depth 3
```

`plan blast RA-4 --depth 2` is unchanged at 84 seeds reaching 11 nodes, and
now says why its one `src/schema.surql` touch contributed nothing:
"1 touch(es) bound to a path the code graph holds nothing for, and were
skipped as seeds".

### Tests

`tests/plan_ops.rs` stays at 10. The reason-distinguishing test was replaced
by `a_real_but_unindexed_path_binds_and_is_never_stale`, which pins all of
the new behavior against one index in one run: a file written after the
index ran binds RESOLVED with `indexed: false`; a file with no grammar binds
RESOLVED with `indexed: false`; a file that is not there is the only
UNRESOLVED one and the only thing `plan stale` reports; a glob matches a
working tree file the indexer would never have offered it; `touching` finds
an unindexed path; `blast` skips three unindexed targets and says so; and
re-indexing flips `indexed` from false to true **without changing the
confidence**, which is the proof that the plan was never the thing that was
wrong. Two unit tests in `ops.rs` cover the rendering and the JSON shape,
and one in `resolve.rs` asserts the two removed reason strings no longer
parse.

Measured in an isolated worktree at this commit's tree, so concurrent edits
in the shared tree cannot flatter the numbers:

```
$ cargo build --release        # 0 warnings
$ cargo test --release --no-fail-fast
passed=337 failed=0            # three consecutive runs, identical
```

---

## 10. Follow-up, 2026-09-14: a finished plan is not a stale plan

The integration pass found `stale` filtering on the touch and ignoring the
item, while `collisions` already filtered on `Status::Active`. On this
repository that showed up as item RF-8, which is `done` and touches
`write_updates`, a symbol commit `3a517b2` deleted. The item had landed; the
report asked the reader to fix it anyway.

`stale` now considers `planned` and `active` items only. A `done` item
describes work that already shipped, and the code it named has every right
to have moved on since, so a touch that no longer binds there is a record of
history rather than a plan pointing at nothing. `abandoned` is the same case
from the other direction: work deliberately dropped, kept in the file so the
decision stays visible. Reporting either asks the reader to close something
already closed, which is how a staleness report fills with entries nobody
can act on.

Nothing else changed. `sync` still reports the unresolved touch, because a
sync reports what it bound whatever the item's status, and `plan show RF-8`
still shows it, because that is the item's own detail view. Only the
roadmap-wide staleness report narrows.

### Evidence

```
$ codegraph index . --project-id codegraph-l2 --tier full --force --db-url surrealkv://<scratch>/graph.db
  Files:       140 scanned, 140 indexed
  Nodes:       1997

$ codegraph plan sync ...
  Written:     6 planes, 32 items, 309 touch rows from 79 selectors
  Selectors:   78 resolved, 0 ambiguous, 1 unresolved
  Not indexed: 112 of those resolved rows bound to a path the code graph holds nothing for

$ codegraph plan sync ... --json   # the one unresolved selector
RF-8 symbol: write_updates -> no_such_symbol

$ codegraph plan stale ...
=== Stale plans (0 items, 0 actionable touches) ===
Every touch in the roadmap still binds to live code.
```

The roadmap grew to 32 items and 79 selectors as other lanes recorded their
work, which is why these numbers differ from section 9.

`tests/plan_ops.rs` gains `stale_ignores_finished_and_abandoned_work`
against a new fixture, `tests/fixtures/planes/ops-done-item.yaml`, whose
three items all carry an UNRESOLVED symbol touch and differ only in status.
The test asserts the report names `LIVE-1` alone, then rewrites the same
file with the `done` item reopened as `planned` and asserts it is then named
too, which is what pins the exclusion to the status rather than to the
touch. Removing the two-line status filter fails it:

```
  left: ["DONE-1", "GONE-1", "LIVE-1"]
 right: ["LIVE-1"]
test result: FAILED. 0 passed; 1 failed
```

With the filter restored, `tests/plan_ops.rs` is 11 passed, 0 failed.

### One thing for lane L4

`codegraph_plan_stale`'s description in `src/mcp/server.rs` is now wrong in
two places, and that file is L4's:

1. "Returns every work item carrying an UNRESOLVED touch" should say every
   `planned` or `active` work item.
2. The whole passage about the `actionable` count being "deliberately
   smaller than the raw total" because "a roadmap that cites its own
   specification documents carries touches that can never bind" describes
   behavior that commit `d95bad6` removed. Those touches now bind RESOLVED
   with `indexed: false` and never reach this report, so `actionable` equals
   the item count and the paragraph should go.
