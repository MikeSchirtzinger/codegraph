# Extractor gaps G1 to G4, closed against the replay audits

Built in a dedicated worktree created at `6d7841a`.

The finding this work answers is a replay audit of the structural gate. The gate
returned a clean verdict on continuedev/continue at `5b5532f7`, a commit that
deletes `extensions/cli/src/tools/searchAndReplace/` while leaving
`extensions/cli/src/tools/preprocess.test.ts` importing
`searchAndReplaceInFileTool` from it and calling it five times. The project's
own next commit, `fea84252` "fix: broken test", deletes the import and the
122-line test block, so the repository adjudicates the defect without any
judgement from us. Three independent gaps had to close before the gate could
see it. A fourth, from a replay audit of a private Rust monorepo, produced that
audit's one false positive. That audit's evidence file and its replay script
are omitted from the public copy.

All four are fixed. Both replays now give the right answer, and the answer
changed in the direction the source tree says it should: Continue reports the
dangling reference it really has, and the private monorepo reports nothing
where it has nothing.

## Result

| gap | what it was | fix | test |
|---|---|---|---|
| G1 | an exported const bound to an object literal produced no node | `src/index/extractors/typescript.rs`, `extract_value_bindings` | `exported_const_object_literal_is_a_definition` |
| G2 | code inside `describe` and `it` callbacks produced zero edges | `src/index/extractors/typescript.rs`, `extract_module_scope_calls` | `calls_inside_describe_and_it_callbacks_are_extracted` |
| G3 | a deleted symbol in receiver position never matched | `src/graph/dependencies.rs`, `is_stale_candidate` | `deleted_receiver_is_a_stale_reference` |
| G4 | a Rust closure binding was not a definition | `src/index/extractors/rust.rs`, `extract_closure_bindings` | `a_named_closure_binding_is_a_definition` |

Each fix ships with its own specificity test, so the sensitivity it buys
cannot be bought by weakening the converse:

| gap | specificity test | what it pins |
|---|---|---|
| G1 | `inner_scope_object_literal_is_not_a_definition` | a local config object is not a project-wide definition |
| G2 | `a_file_with_no_module_scope_code_gets_no_module_node`, `module_scope_pass_does_not_double_count` | lazy creation, and one call means one edge |
| G3 | `live_receiver_is_not_a_stale_reference`, `receiver_rule_never_matches_on_the_tail` | the 833c17f class stays closed |
| G4 | `a_plain_let_binding_is_not_a_definition`, `a_closure_body_is_not_walked_for_calls_twice` | only a named closure, attributed once |

## Environment

Every cargo command ran under `nice -n 10` with
`CARGO_TARGET_DIR=/Users/mike/dev/codegraph-wt-extractor/target`, on a 16 GB
machine shared with three other build lanes, each run gated on
`sysctl vm.swapusage` reporting under 3500M used.

## Fail first

Each fix was disabled at its call site, the new tests run against the
unfixed code, then the fix restored. Nothing else changed between the two
runs.

### G1 and G2

```
$ cargo test --lib typescript::

running 7 tests
test index::extractors::typescript::tests::class_method_qualified_name ... ok
test index::extractors::typescript::tests::inner_scope_object_literal_is_not_a_definition ... ok
test index::extractors::typescript::tests::a_file_with_no_module_scope_code_gets_no_module_node ... ok
test index::extractors::typescript::tests::top_level_function_qualified_name ... ok
test index::extractors::typescript::tests::calls_inside_describe_and_it_callbacks_are_extracted ... FAILED
test index::extractors::typescript::tests::module_scope_pass_does_not_double_count ... FAILED
test index::extractors::typescript::tests::exported_const_object_literal_is_a_definition ... FAILED

---- calls_inside_describe_and_it_callbacks_are_extracted stdout ----
the call through the imported tool must be captured, got []

---- module_scope_pass_does_not_double_count stdout ----
assertion `left == right` failed: got ["helper"]
  left: 0
 right: 1

---- exported_const_object_literal_is_a_definition stdout ----
the exported const must be a definition node

test result: FAILED. 4 passed; 3 failed; 0 ignored; 0 measured; 186 filtered out
```

### G3

```
$ cargo test --lib graph::dependencies::

running 13 tests
test graph::dependencies::tests::qualified_capture_never_bare_tails_onto_a_live_symbol ... ok
test graph::dependencies::tests::crate_prefixed_capture_of_a_live_symbol_is_not_stale ... ok
test graph::dependencies::tests::pool_key_mismatch_is_not_a_stale_reference ... ok
test graph::dependencies::tests::receiver_rule_never_matches_on_the_tail ... ok
test graph::dependencies::tests::live_receiver_is_not_a_stale_reference ... ok
test graph::dependencies::tests::deleted_receiver_is_a_stale_reference ... FAILED

---- deleted_receiver_is_a_stale_reference stdout ----
assertion `left == right` failed: a call through a deleted receiver is a stale reference, got []
  left: 0
 right: 1

test result: FAILED. 12 passed; 1 failed; 0 ignored; 0 measured; 180 filtered out
```

The two specificity tests pass against the unfixed code as well, which is
the point of them: they assert behavior the fix must leave alone.

### G4

```
$ cargo test --lib extractors::rust::

running 7 tests
test index::extractors::rust::tests::a_closure_body_is_not_walked_for_calls_twice ... ok
test index::extractors::rust::tests::a_plain_let_binding_is_not_a_definition ... ok
test index::extractors::rust::tests::crate_root_files_produce_bare_qualified_names ... ok
test index::extractors::rust::tests::top_level_function_qualified_name ... ok
test index::extractors::rust::tests::impl_method_qualified_name_uses_bare_type ... ok
test index::extractors::rust::tests::nested_mod_function_qualified_name ... ok
test index::extractors::rust::tests::a_named_closure_binding_is_a_definition ... FAILED

---- a_named_closure_binding_is_a_definition stdout ----
the closure binding must be a definition node

test result: FAILED. 6 passed; 1 failed; 0 ignored; 0 measured; 189 filtered out
```

## The new `is_stale_candidate` rule

Old rule, unchanged in every respect except one added branch:

```rust
let normalized = normalize_separators(raw_to_name);
if bare_name(&normalized) != queried_bare {
    return false;
}
```

New:

```rust
let normalized = normalize_separators(raw_to_name);
if bare_name(&normalized) != queried_bare {
    return live.is_empty() && names_a_receiver(&normalized, queried_bare);
}
```

`names_a_receiver` splits the normalized capture on `::`, drops the last
segment, and asks whether any remaining segment equals the queried bare name.
So `searchAndReplaceInFileTool::preprocess!` names
`searchAndReplaceInFileTool` in receiver position, and
`cursor::node` names `cursor`.

In words: when the queried name has no live definition left anywhere in the
project, a capture whose non-final segments include that name is a stale
reference through the vanished symbol.

### Why this cannot re-open the class 833c17f closed

That class was a **qualified capture bare-tailing onto a symbol that is still
there**. The production shape was `cursor.node()`, the tree-sitter method
call this repo's own extractor makes, reported against
`graph::dependencies::tests::node`, which had never moved. It was 37 of the
38 stale references a self-index produced on an unmodified
`src/graph/dependencies.rs`, and closing it took the count from 1,015 across
37 files to 0.

Three independent reasons the receiver rule cannot bring it back.

1. **It is gated on the empty-live-set branch.** The entire class is decided
   by the `live` comparison further down, which is byte-for-byte unchanged.
   While one definition of the queried name survives, `names_a_receiver` is
   never consulted at all. Every false positive 833c17f removed was a capture
   matched against a live definition, so every one of them still takes the
   old path.
2. **It never reads the tail.** `names_a_receiver` pops the last segment
   before looking. The 833c17f shape is a tail match by construction, so it
   cannot reach the new branch by that route either, live set or not. The
   test `receiver_rule_never_matches_on_the_tail` pins both halves.
3. **It is narrower in kind than a branch that already existed.** When
   `live.is_empty()`, the scan already matched a bare capture
   unconditionally, and has since before 833c17f. The new branch matches a
   strict subset of the additional captures that regime could plausibly
   admit: only those that spell the deleted name out as a receiver segment.

The residual is the mirror image and is deliberate, unchanged from the
existing doc comment: a rename whose bare name is also still defined
elsewhere takes the strict branch, so a qualified stale capture pointing at
the vanished one can still be missed.

## Replay, Continue at 5b5532f7

Scripted end to end as `scripts/replay/continue-5b5532f7.sh`. It lives in
`scripts/` rather than `tests/` because it clones from github.com; nothing
under `tests/` reaches the network, and `scripts/corpus/run.sh` sets the same
precedent for corpus work that does.

```
$ cargo build --release && cargo build --release --example gate_verdict
$ scripts/replay/continue-5b5532f7.sh
```

The script pins the two commits, indexes the parent, applies the replayed
commit, re-indexes incrementally so deletion tracking runs, then gates on the
four deleted paths. Gate output:

```json
{
  "target": {
    "Paths": [
      "tools/searchAndReplace/index.ts",
      "tools/searchAndReplace/parseArgs.ts",
      "tools/searchAndReplace/parseBlock.ts",
      "tools/searchAndReplace/findSearchMatch.ts"
    ]
  },
  "resolved_dependents": [],
  "ambiguous_refusals": [],
  "stale_references": [
    {
      "from_name": "preprocess.test",
      "from_file": "tools/preprocess.test.ts",
      "to_name": "searchAndReplaceInFileTool.preprocess!",
      "to_type": "function"
    }
  ],
  "name_ambiguous": false,
  "matched_symbols": [],
  "graph_empty": false
}
```

The audit's own verdict line is now answered: the gate flags
`preprocess.test.ts`. The symbol query returns the same finding five times,
once per call site, which matches the five calls the evidence file names at
lines 367, 400, 414, 436 and 461. The capture reads `preprocess!` because
the source writes the TypeScript non-null assertion
`searchAndReplaceInFileTool.preprocess!(...)`.

`matched_symbols` stays empty, and that is correct rather than a remaining
gap: nothing in the surviving tree defines `searchAndReplaceInFileTool`,
which is exactly why the reference is dangling.

### The two isolated single-file indexes the evidence file recorded

Same method, same files, same commit, rerun with the fixed binary:

| file | evidence, before | now |
|---|---|---|
| `tools/searchAndReplace/index.ts` | 10 nodes, 9 import and 1 interface, 0 edges | 11 nodes, 15 edges |
| `tools/preprocess.test.ts` | 11 nodes, all import, 0 edges | 12 nodes, 131 edges |

One added node each: the `object` definition for the export, and the
`module` node for the test file's callback code.

### Whole-scope index deltas, and what they cost

Parent tree, `extensions/cli/src`, 370 files:

| metric | evidence, before | now |
|---|---|---|
| nodes | 2,636 | 2,905 |
| edges | 3,165 | 17,946 |
| resolved | 458 | 1,885 |
| ambiguous | 30 | 149 |
| unresolved name edges | 2,243 | 15,478 |

Stated plainly, because it is the largest single number this lane moves: the
TypeScript graph for this repository was missing roughly five sixths of its
call edges, and closing G2 recovers them. Resolved bindings go up 4.1x, which
is the signal. Unresolved captures go up 6.9x, which is the cost, and it is
the same cost every language pays for module-scope calls into libraries the
index does not contain. The unresolved floor is noise the stale scan already
has to reason about, and 833c17f is the record of how expensive that noise
can get. Nothing here re-opens it, because specificity is decided by the live
set, not by the size of the unresolved pool, and `tests/gate_specificity.rs`
still passes on its adversarial tree. It is still worth watching on the next
self-index.

## Replay, a private Rust monorepo

The replay script for this one is omitted from the public copy, because the
subject repository is private and no reader could run it. It takes no network
and makes no clone: it lays each revision of the scope down with `git archive`
into a throwaway repo, which never touches the source repository's checkout,
index, or worktree list.

Gate verdict on the seven touched paths, against the evidence file's figures:

| metric | evidence, before | now |
|---|---|---|
| matched symbols | 389 | 390 |
| resolved dependents | 137 | 138 |
| ambiguous refusals | 0 | 0 |
| stale references | 1 | 0 |

The one added matched symbol and the one added dependent are the same node:
the `base_event` closure, now a definition at
`event_converter::EventConverter::agui_to_proto::base_event`. The symbol
query returns it with `agui_to_proto` as a resolved dependent and an empty
`stale_references`, where before it returned the finding 24 times, once per
call site.

The false positive is gone at its source rather than filtered downstream. R3,
the same-file bare-name rule, now binds those 24 calls, so they are RESOLVED
and never reach the stale scan at all.

Parent index, 69 files: 2,264 nodes and 9,895 edges, against the evidence's
2,260 and 9,891. Exactly four nodes and four `contains` edges more, which is
four named closures in that crate.

## Full suite

```
$ cargo test --release
```

360 passed, 0 failed, 0 ignored, across the lib unit tests and every
integration binary:

| binary | passed |
|---|---|
| unittests src/lib.rs | 198 |
| clone_detection | 2 |
| deletion_tracking | 7 |
| explain | 24 |
| facade_integration | 3 |
| fixtures_integration | 9 |
| gate_specificity | 3 |
| incremental_reresolution | 6 |
| kill_test | 1 |
| landscape | 19 |
| mcp_tools | 29 |
| plan_ops | 13 |
| plan_schema | 21 |
| regression_defects | 4 |
| run_anywhere | 17 |
| structural_fingerprints | 4 |

The lib count is 198 against 185 before this lane: 13 new tests, 7 in the
TypeScript extractor, 3 in the Rust extractor, 3 in `graph::dependencies`.
`plan_ops` is 13 against 11, the two module-node tests below. Every
pre-existing test passes unchanged.

```
$ cargo build --release
   Compiling codegraph v0.1.0
    Finished `release` profile [optimized] target(s) in 21.67s
```

Zero warnings, on a forced recompile of the crate rather than a cache hit.

```
$ codegraph plan lint
  6 planes, 36 items, 87 touches
  No problems found.
```

## Expected-count changes

Exactly one expectation changed anywhere in the suite. No `expected.yaml`
manifest needed an edit: the manifests assert named cases by lookup, not
totals, and every case in every one of them still holds.

**`tests/mcp_tools.rs`, `WIRE_POLYGLOT_ARCH_NODE_TYPES`.** A byte-for-byte
captured wire string for `codegraph_architecture` over the polyglot fixture.
Three counts moved, all of them one node and one edge in
`tests/fixtures/polyglot/client/src/index.ts`, which ends in a top-level
`run();`:

| field | before | after | why |
|---|---|---|---|
| `module` node type | absent | 1 | G2 gives that file a module-scope node |
| `typescript` | 8 | 9 | the same node |
| `calls` | 12 | 13 | the top-level `run()` call, previously invisible |

`function`, `import`, `go`, `python` and `file_ref` are unchanged, and the
new hub row reads `index (module): in:0 out:1 total:1, in client/src/index.ts`.

## Follow-up: a nested binding was attributed twice

A defect this lane introduced, caught in validation. Five lines of
TypeScript produced three `calls` edges for two source calls:

```ts
function outer() {
  const inner = () => {
    helper();
  };
  inner();
}
```

Fail-first output, naming the duplicate and the order it arrived in:

```
$ cargo test --lib typescript::

---- a_nested_binding_owns_its_calls_and_the_outer_walk_stops stdout ----
assertion `left == right` failed: two source calls, two edges: ["helper", "inner", "helper"]
  left: 3
 right: 2

test result: FAILED. 8 passed; 1 failed
```

Mechanism. `walk` dispatches `function_declaration` to `extract_function`,
which walks the whole body for calls, and then falls through into its own
child recursion, which reaches the `lexical_declaration` and hands it to
`extract_value_bindings`. That claims `inner` as a definition and walks its
value for calls, which the outer walk had already covered. Not the
module-scope pass: that one stops at `function_declaration` and never
entered `outer` at all.

### The attribution rule

**A call belongs to the nearest enclosing definition, once.** So `helper`
belongs to `inner`, and `inner` belongs to `outer`. Three walks record calls
in this extractor, and the rule holds because each stops where the next
starts, with no walk needing to see another's results:

| walk | covers | stops at |
|---|---|---|
| `extract_function` | a named function's body | a claimed binding |
| `bind_value` | a claimed binding's value | nothing below it claims anything |
| `extract_module_scope_calls` | everything no definition claimed | every declaration that owns its own walk |

The two stop sets are deliberately different sizes, and the second
specificity test pins why. The module-scope pass stops at a class, whose
methods it must not claim. The function-body walk does not, because
`extract_method` never walks a body, so a nested class's calls have to stay
with the function around them rather than vanish. For the same reason the
stop is `is_claimed_binding`, which asks whether that exact declarator became
a definition, rather than "any declarator": an object literal below module
level is never claimed, so stopping there would drop the calls inside it with
nothing to pick them up.

Tests: `a_nested_binding_owns_its_calls_and_the_outer_walk_stops` and
`an_inner_object_literal_leaves_its_calls_with_the_function`.

### This diverges from the Rust closure rule, on purpose

G4 keeps a Rust closure's calls with the enclosing function, pinned by
`a_closure_body_is_not_walked_for_calls_twice`. The two languages get
different answers because the construct plays a different role in each. A
Rust closure is a value that runs inline inside the function that binds it,
and the Rust extractor models no nested definitions at all, so the enclosing
function is the nearest definition. In TypeScript an arrow binding is the
dominant way a module defines a function, `export const handler = () =>
...`, so it has to be a first-class caller in its own right. Recorded rather
than smoothed over; unifying them is a real question for a later pass.

### Counts that moved

Re-measured after the fix, everything else held fixed.

| measurement | before the fix | after |
|---|---|---|
| Continue parent, edges | 18,401 | 17,946 |
| Continue parent, resolved | 1,930 | 1,885 |
| Continue parent, ambiguous | 153 | 149 |
| Continue parent, unresolved | 15,478 | 15,478 |

455 duplicate edges on one 370-file tree, which is the scale of the defect.
Nodes are unchanged at 2,905, as they must be: nothing about which node owns
a call changes how many nodes exist. The replay still passes and reports the
same finding five times, once per call site.

The two isolated single-file measurements did not move, and neither did
`tests/mcp_tools.rs`. Both are files without an arrow binding nested inside
a named function, which is the only shape this touches. In particular the
polyglot wire capture `WIRE_POLYGLOT_ARCH_NODE_TYPES` is byte-for-byte
unchanged from the edit G2 required, still `module: 1`, `typescript: 9`,
`calls: 13`, and `architecture_without_a_filter_is_byte_identical_to_the_captured_wire_output`
passes without a second re-capture.

## Follow-up: the module node reached a second name-keyed index

Found in validation, not by this lane. `src/plan/resolve.rs` builds its own
bare-name index over every node and excluded only `import`, so the synthetic
module node G2 adds was a candidate for a `- symbol:` roadmap touch. Two ways
that is wrong, both reproduced before the fix:

- `- symbol: utils` with nothing anywhere defining `utils` bound silently to
  the module node for `utils.ts`. An author who named a symbol that does not
  exist was told their plan was fine. Before G2 that touch was UNRESOLVED,
  which is a visible error.
- `- symbol: utils` with a real `function utils` in another file went
  AMBIGUOUS across the function and the module node, where it used to
  resolve.

Fail-first output, both tests against the unfixed resolver:

```
$ cargo test --release --test plan_ops module_node

---- a_module_node_never_satisfies_a_symbol_touch stdout ----
assertion `left == right` failed: nothing defines `utils`, so the touch must not bind:
  Selectors:   1 resolved, 0 ambiguous, 0 unresolved
  left: 0
 right: 1

---- a_real_definition_still_binds_past_the_module_node stdout ----
MN-1 symbol: utils [AMBIGUOUS] candidates: 1999d63b05fedb9d76544132d68a2816, 7a41c8fcdc3ce546f2e6551f4abf55f0 (ambiguous_candidates)
    utils matched 2 symbols by bare name utils: utils::utils (module in utils.ts), helpers::utils (function in helpers.ts). Write the qualified name to pick one
  left: 0
 right: 1

test result: FAILED. 0 passed; 2 failed
```

Fixed by excluding `module` from that index the way `import` already was.
Fixture: `tests/fixtures/planes/ops-module-node.yaml`. The second test also
asserts the module node is still in the store, so it proves an exclusion
rather than an absence.

The exclusion covers Rust's inline `mod` nodes too, not only TypeScript's
synthetic one. That is the same judgement for the same reason, a module is a
container rather than a definition an author writes, and no existing test
depended on binding one.

### The `object` decision

`object` is deliberately **not** excluded, and stays bindable by
`- symbol:`. It is a name the author wrote in source, `export const
searchAndReplaceInFileTool = { ... }`, which is exactly the thing a symbol
touch is for; the whole point of G1 is that this name is a definition. The
`module` node is the opposite case: nobody wrote it, it is named after a
file, and an author who means the file writes `- file: utils.ts`.

The doc comment on `module_scope_name` claimed the `module` type meant the
node "cannot shadow a real symbol in the resolver's pools". True of the
resolver, whose pools are keyed on (bare name, `to_type`, language family),
and not true of every name-keyed lookup in the tree, which is how this got
through. The comment now says which index it is talking about, names the one
that did see the node, and flags the question for any new one.

## Recorded, not fixed

- **TypeScript class methods do not extract calls.** `extract_method` adds
  the node and the `contains` edge but never walks the body, so a method's
  references are absent from the graph. Larger than G2 in likely impact, and
  outside this lane's scope; recorded here rather than guessed at.
- **`const f = function () {}` is still not a definition.** Same class as the
  arrow-function binding, not one of the measured gaps, deliberately left out
  so every count change in this lane traces to a named finding.
- **A nested class's methods still have no walk of their own.** Their calls
  stay attributed to the function around them, which is why the function-body
  walk's stop set is narrower than the module-scope pass's. Same root cause
  as the class-method gap above.
