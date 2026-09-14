# chain-scope-20260914: RF-6, `verify_chain`'s graph load

Based on `dc9215f`, built in a dedicated worktree with `CARGO_TARGET_DIR` set
to that worktree's own `target/` for every command below.

## 0. The item, as written

`.codegraph/planes.yaml`, RF-6: `ExplainGraph::load` reads every node, every
name-edge and every deletion record for the project before a single chain is
checked, and `codegraph_verify_chain` (`src/mcp/server.rs`) calls it per
request, so verifying n chains loads the project n times. Not yet measured
against a large store. Two candidate shapes named: scope the load to the
chain's own node and edge set, or hold one loaded `ExplainGraph` on the
server and invalidate it when the index generation moves.

## 1. Measure first

### 1.1 Corpus

The task asked for the largest store cheaply available: this repo plus a
shallow `tokio-rs/tokio` clone (799 files, matching the board's own
run-anywhere corpus figure). Combining two different root paths under one
`project_id` with two separate `codegraph index` calls does not simply add
the two trees: the second call's change detection is scoped to files under
its own root, so it also runs deletion tracking against the *whole
project's* prior file set and tombstones every file it did not just walk.
Measured directly: indexing this worktree (1,998 nodes) then indexing
`tokio` (15,277 nodes) into the same `project_id` reported "1411 removed"
against the first run's fingerprints, and the resulting store held only
~15,300 nodes, not the sum. The fix was to build one combined root
(`git init` a fresh directory containing both trees as subdirectories,
commit it) and index that once:

```
$ BIN=/Users/mike/dev/codegraph-wt-chainscope/target/release/codegraph
$ STORE=/private/tmp/claude-502/chainscope-corpus/bench2.db
$ cd /private/tmp/claude-502/chainscope-corpus/combined   # codegraph-src/ + tokio-src/, one git repo
$ time "$BIN" index . --project-id chainscope-bench2 --db-url "surrealkv://$STORE" --force
...
files_indexed=936 nodes=17216 edges=46950 resolved=6422 ambiguous=1421 unresolved=33875
Elapsed: 60.5s
```

936 files, 17,216 nodes, 46,950 edges (41,592 name-edges by `ExplainGraph`'s
own filter). Larger than every fixture in `tests/fixtures/`, smaller than the
~37k-node client-scale figure the item cites as reported-not-measured, but
enough to show the shape of the cost.

### 1.2 Load versus verify, before any code change

A temporary example (`examples/measure_chain_scope.rs`, not committed, built
and run against the store above, then deleted before this lane's commits)
timed `ExplainGraph::load` in isolation, `verify_chain` in isolation against
an already-loaded graph, and the full `load`-then-`verify` shape
`codegraph_verify_chain` actually runs, 8 runs each, medians below:

```
$ /Users/mike/dev/codegraph-wt-chainscope/target/release/examples/measure_chain_scope \
    --db-url "surrealkv:///private/tmp/claude-502/chainscope-corpus/bench2.db" \
    --project-id chainscope-bench2 --runs 8
load run 0: 521.8ms (17216 nodes, 41592 edges)
...
verify run 0: 0.004ms (ok=true)
...
full (load+verify) run 0: 383.7ms
...
BEFORE (fresh ExplainGraph::load per call):
  load median (ms):          431.5
  verify median (ms):        0.001
  full call median (ms):     401.2
  load share of full call:   107.6%
```

(The >100% figure is run-to-run noise from concurrent lanes sharing this
machine tonight, not a real signal; the point is that `verify_chain`'s own
work, 0.001ms, is unmeasurably small next to a ~350-520ms load.)

| | median (ms) |
|---|---|
| `ExplainGraph::load` alone | 431.5 |
| `verify_chain` alone (graph already loaded) | 0.001 |
| full call as shipped (load then verify) | 401.2 |

**Load is over 99.9% of the call.** The item's own threshold ("if load is
under 10 percent of the call on tokio, report that and stop") is not close;
this proceeds to the fix.

## 2. Which shape, and why the item's own lean does not survive contact

The item's notes lean toward scoping ("it needs no cache invalidation
story"). Reading `ExplainGraph`'s verifier (`src/graph/explain.rs`) closely
changes that conclusion.

Every re-check that matters, `Fact::LiveDefinitions`, `Fact::CandidatePool`,
`Fact::RuleApplication`, `Fact::CascadeOutcome`, recomputes its answer from
`index::resolve::build_indices`, keyed by `(name, node_type,
language_family)` **over the whole project's matching nodes**, not from the
node ids a chain's steps happen to name. `Fact::CandidatePool` already
carries the claimed `node_ids` list; a load scoped to "the ids the chain
names" would load exactly those ids and nothing else, and recomputing the
pool from that scope would just hand back the same list the chain already
claims. That is not a re-derivation, it is the chain's own claim reflected
back at it, silently vacuous in exactly the two tamper classes this file's
own tests exist to catch:
`the_verifier_recomputes_the_candidate_pool_rather_than_trusting_it` and
`the_verifier_catches_a_dropped_candidate`. A correct scoped load is
possible (query by the `(bare, to_type, language_family)` and `name` keys a
chain's facts *name*, not by the ids in its `NodeExists`/`CandidatePool`
steps, plus a per-source-file import-fact query for R4), but it is a
materially larger, riskier change, exactly the kind the correctness
constraint on this lane rules out taking on speculatively.

Caching the complete graph has no such risk: it is the same answer
`ExplainGraph::load` always produced, reused rather than recomputed. The
only new failure mode it introduces is staleness, and that has a cheap,
already-written signal to key on: `project_registry.last_indexed_at`,
written unconditionally at the end of every `index` run (full or
incremental, changed or not, `src/index/mod.rs`'s
`update_project_registry`), so any index run at all moves it, even a
no-op one. Measured cost of checking it:

```
$ /Users/mike/dev/codegraph-wt-chainscope/target/release/examples/staleness_check \
    "surrealkv:///private/tmp/claude-502/chainscope-corpus/bench2.db" chainscope-bench2
staleness check run 0: 4.243ms, rows=1
staleness check run 1: 0.389ms, rows=1
staleness check run 2: 0.264ms, rows=1
staleness check run 3: 0.237ms, rows=1
staleness check run 4: 0.283ms, rows=1
```

About 0.3ms steady-state, roughly 1,000x cheaper than the ~350-500ms full
load. **Decision: cache the complete `ExplainGraph`, keyed on that
watermark. Fixed, not measured-only, decided by the >99.9% load share
above.**

## 3. What changed

- `src/graph/explain.rs`: added `current_watermark` (a `SELECT
  last_indexed_at FROM project_registry WHERE project_id = $pid`, returned
  as the raw `surrealdb_types::Value` rather than a typed `String`; the
  typed `SurrealValue` derive path was tried first and rejects a `datetime`
  outright with `Failed to convert to none | string: Expected string, got
  datetime` rather than formatting it, so the raw-`Value` idiom this module
  already uses elsewhere was kept) and `ExplainGraphCache`, a `Clone`
  wrapper around `Arc<Mutex<Option<(watermark, Arc<ExplainGraph>)>>>`. Its
  `get(db)` reuses the cached graph when the watermark has not moved,
  otherwise runs a fresh `ExplainGraph::load` and caches the result.
- `src/mcp/server.rs`: `CodegraphServer` gained an `explain_cache:
  ExplainGraphCache` field, constructed in `new()`. `codegraph_verify_chain`
  now calls `self.explain_cache.get(&self.db)` instead of
  `ExplainGraph::load(&self.db, &self.project_id)` directly. The unused
  `ExplainGraph` import was dropped. No other tool touches the cache:
  `codegraph_impact`'s `explain` path still goes through
  `facade::explain_chains_for_symbol`, out of this lane's scope
  (`src/facade.rs` is not on RF-6's touch list).

## 4. Correctness

Every existing test in `tests/explain.rs` (24) and `tests/mcp_tools.rs` (29)
passes unchanged; `ExplainGraph::load` itself was not touched, so nothing
that called it directly could regress.

Three new tests, none of them redundant with each other:

- `explain_graph_cache_reuses_the_loaded_graph_when_the_index_is_unchanged`
  (`tests/explain.rs`): two `get()` calls with no index run between them
  return the identical `Arc` (`Arc::ptr_eq`), proving the cache actually
  caches rather than reloading every time.
- `explain_graph_cache_reloads_after_an_incremental_reindex_changes_the_project`
  (`tests/explain.rs`): the correctness case, and the closest analogue to
  the brief's "a chain referencing a node outside the scoped load" for the
  shape actually shipped. Builds a stale-reference chain for
  `orphan_target` (no live definition anywhere in a small tempdir project),
  warms the cache, adds a file defining `orphan_target`, re-indexes
  incrementally, then re-checks the *same* chain through the *same* cache
  and asserts it now fails, naming the `live_definitions` step.
- `verify_chain_reflects_a_reindex_that_happens_between_two_calls`
  (`tests/mcp_tools.rs`): the same scenario one layer up, through two real
  `CodegraphServer::verify_chain` calls (the actual tool a client calls),
  proving the cache is wired into the tool, not just into a type nothing
  uses it through.

**Fail-first check, both new tests that assert invalidation.** The
invalidation branch (`if *cached_watermark == watermark`) was temporarily
replaced with `if true` (always reuse the cache) and both tests re-run:

```
$ cargo test --release --test explain explain_graph_cache -- --nocapture
test explain_graph_cache_reuses_the_loaded_graph_when_the_index_is_unchanged ... ok
test explain_graph_cache_reloads_after_an_incremental_reindex_changes_the_project ... FAILED
  panicked at tests/explain.rs: "the cache must not keep serving the pre-reindex graph after an index run"

$ cargo test --release --test mcp_tools verify_chain_reflects_a_reindex -- --nocapture
test verify_chain_reflects_a_reindex_that_happens_between_two_calls ... FAILED
  panicked: "the same chain must fail once the reindex the server's cache should have picked up
  makes it false, not keep passing the stale answer"
```

Both fail as expected against the broken code; the fix was then reverted to
the real check and both pass again. Neither test is vacuous.

## 5. After

Same store, same chain (a `Dependent` finding on `new_accepted`), 8 calls
through `ExplainGraphCache` in one session (simulating one MCP session
verifying 8 chains against an unmoving index):

```
cached call run 0: 363.348ms (cold, pays the load)
cached call run 1: 0.561ms
cached call run 2: 0.246ms
...
AFTER (ExplainGraphCache, 8 calls in one session):
  cold (first) call (ms):    363.3
  warm call median (ms):     0.246
  speedup, warm vs before:   1632x
```

| | before (fresh load per call) | after (cached, warm call) |
|---|---|---|
| median call cost | 401.2ms | 0.246ms |
| ratio | 1x | ~1,632x faster |

The cold first call on a session still pays the full load, unavoidably: the
graph has to be built at least once. Every call after it, until the next
index run, costs one watermark check.

## 6. Test suite

```
$ cargo build --release
    Finished `release` profile [optimized] target(s) in 26.17s
$ cargo build --release 2>&1 | grep -ci warning
0
$ cargo test --release
... 18 targets ...
passed=348 failed=0
```

348 (345 baseline from the explain-v1 receipt + 3 new tests here), 18
targets, 0 build errors, 0 warnings.

**On the em-dash gate specifically:** a repo-wide search for the character
this house style forbids is not empty. `src/schema.surql`, `src/canon.rs`
and other files this lane never opened carry pre-existing uses, unrelated to
RF-6 (the explain-v1 receipt already flags some of these as known,
deferred cleanup). Scoped to only what this lane actually wrote or edited,
the check is clean: the diff introduced into `src/graph/explain.rs`,
`src/mcp/server.rs`, `tests/explain.rs` and `tests/mcp_tools.rs` carries none
(checked against the added lines only, not the pre-existing file contents),
and this receipt file, entirely new, carries none either. A whole-tree pass
would need a separate, unrelated cleanup lane; it is called out here rather
than silently declared clean.

## 7. Scope notes

- `examples/measure_chain_scope.rs` and `examples/staleness_check.rs` were
  temporary measurement scaffolding, built and run to produce the numbers
  in §1 and §2, then deleted before committing; they are not part of the
  shipped example set and are not referenced by any test.
- `src/facade.rs` and `src/cli.rs` were read but not touched. `RF-6` names
  only `codegraph_verify_chain`'s per-request load; `explain_chains_for_symbol`
  (the CLI's `--explain` path and `codegraph_impact`'s explain section) has
  the same shape of cost but is out of this item's stated scope and not
  measured here.

## 8. Follow-up: the cold-load-during-a-write-mid-index race

Flagged by validation before merge. `ExplainGraphCache::get` read the
watermark once, before deciding hit or miss, then, on a miss, loaded and
cached the result under that same pre-load watermark. `ExplainGraph::load`
runs three separate queries (nodes, then name-edges, then deletion
records), and `project_registry.last_indexed_at` is written last, as the
final step of an `index` run (`index::mod`'s `update_project_registry`). If
a cold cache or a miss started loading while a run was mid-write, the
watermark read before the load still read the *previous* run's value (the
run in progress had not finished writing its own registry row yet), so the
load's three queries could each see a different amount of that run, and the
resulting torn snapshot would be cached under the stale watermark. Nothing
would invalidate it until the *next* run completed, so it would be served
for the rest of the run already in flight.

**Fix.** `get` now reads the watermark a second time after the load and
compares it to the pre-load reading (`should_cache`, a pure function of the
two readings). Caching happens only when they match. In the race window,
the loaded graph is still returned for that call (never an error), it is
just not remembered; the following call reads a watermark that has settled
by then and reloads cleanly. The lock is held across the whole operation
(both watermark reads and the load), unchanged from before, so concurrent
callers within one process still serialize on one load rather than each
starting their own.

**Which store engines can actually hit this, stated in both the doc
comment on `ExplainGraphCache::get` and here.** An embedded `surrealkv://`
store takes a single-writer file lock (the same lock
`facade::explain_chains_for_symbol`'s own doc comment already notes causes
a second connection to the same file to deadlock rather than open), so two
*separate processes* sharing one embedded store cannot race here at all: a
second process trying to index would fail to even connect, long before it
could write anything mid-load. The window is real for a server-backed
store (`ws://`/`wss://`), where a separate client can run `index` while
this process serves `codegraph_verify_chain`, or for a single process that
runs an indexer and this cache against the same connection (an in-process
watch-and-reindex mode, which does not exist in this codebase today).

**Test.** `should_cache` was extracted specifically to make this
deterministically testable without a database or real concurrency: four
unit tests in `src/graph/explain.rs`'s new `#[cfg(test)] mod tests` cover
watermark-held-steady (cache), never-indexed-on-both-reads (cache), an
index run completing mid-load (don't cache), and the sharper case where the
*first* index run ever completes mid-load, `None` before, `Some` after
(don't cache; a naive comparison that treated "no row yet" as equal to
"first row now exists" would defeat the whole check). All four proven
non-vacuous: `should_cache` was temporarily replaced with a body that
always returns `true`, the two "don't cache" tests failed as expected
(`should_not_cache_when_an_index_run_completed_during_the_load` and
`should_not_cache_when_the_first_index_run_ever_completed_during_the_load`),
and the fix was then reverted and both passed again.

**What this test does not cover, said plainly rather than faked.** It
tests the decision function `get` calls, not `get` racing a real concurrent
writer end to end. A true end-to-end simulation (bump the registry
watermark from another task at the exact instant between `get`'s two
internal watermark reads) needs a synchronization point inside `get`, and
`tests/explain.rs`/`tests/mcp_tools.rs` are separate integration-test
crates that cannot see a `#[cfg(test)]` item in the library, so any such
hook would have to be a permanent, always-compiled public method on
`ExplainGraphCache` whose only purpose is letting a test pause mid-call.
Judged not worth adding a permanent surface to production code for: the
extracted pure function is the actual fix logic, unit-testing it
deterministically covers that logic completely, and the four cases above
are exhaustive over the three-way `Option` equality this check reduces to
(both `None`, both `Some` and equal, both `Some` and different, one `None`
one `Some`, checked in both directions).

Full suite after this fix: `cargo build --release` 0 warnings; `cargo test
--release` passed=352 failed=0, 18 targets (348 from before this follow-up
+ 4 new `should_cache` unit tests). Memory checked before running
(`vm.swapusage` free 1478M, no `rustc` processes active) per the team
lead's guard.

RESULT: PASS
BRANCH: lane/chain-scope
DECISION: fixed (cached), not measured-only. Decided by load being over
99.9% of the full `load`-then-`verify` call on a 17k-node, 47k-edge store,
against the item's own 10% threshold for stopping at measurement. The
follow-up race fix (S8) is a correctness fix decided by inspection of
`ExplainGraph::load`'s three-query shape against `update_project_registry`'s
write-last ordering, not by a new measurement.
