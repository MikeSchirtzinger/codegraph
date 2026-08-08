# 1,015 stale references, and every one of them was wrong

I have a structural gate that refuses a code-change plan when a rename left
call sites pointing at a symbol that no longer exists. I pointed it at my own
repository (a clean checkout, nothing modified, suite green) and asked it
about every source file, one at a time. It reported **1,015 stale references
across 37 files**. All 1,015 were false positives.

That count is a property of the code as it stood before commit `833c17f`, so
you cannot reproduce it on today's tree. The fix is in, and the same sweep now
returns zero. It is the measurement recorded in that commit's own message,
quoted verbatim in `docs/records/833c17f.md` (this repository's public
history starts at the published tree, so the pre-fix code is not checkable
out from here). Today's code reproduces the
*after*: `codegraph index . --project-id self` (default embedded store at
`surrealkv://.codegraph/graph.db`), then
`for f in $(git ls-files '*.rs'); do cargo run --example gate_verdict -- surrealkv://.codegraph/graph.db self "$f"; done`,
one process at a time because the store holds a file-level lock. This is the
writeup of how that number reached zero without weakening the property the gate
exists to enforce.

## The rule that feels obviously right

Every `calls`/`implements`/`member_of` edge is born with `to_id = ""` and a
`to_name` captured verbatim from source. A resolver cascade tries to bind each
one to a real definition and labels the outcome: RESOLVED (a real node id),
AMBIGUOUS (several candidates survived, listed, never traversed), or UNRESOLVED
(no id, no candidates). UNRESOLVED is not an error state. Most such edges are
calls into the standard library and third-party crates, exactly what you expect
from a tool that indexes one repository without reading its dependencies.

That creates a problem for the one question the gate cares about: *did a
rename miss a call site?* A missed call site is an UNRESOLVED edge, but so is
every call to `HashMap::new`, and an UNRESOLVED edge has no `to_id`, so no
traversal can reach it. The only way to find it is to scan project-wide for
UNRESOLVED edges that still *name* the symbol you're asking about, and the
obvious way to do that is text: normalize the separators, take the bare tail,
compare. That is what `find_stale_references` did. One line of reasoning, no
schema required, and wrong.

## The worst single file

The largest pile landed on `src/graph/dependencies.rs`, the file implementing
the scan itself, which I had not touched.
`cargo run --example gate_verdict -- surrealkv://.codegraph/graph.db self src/graph/dependencies.rs`
returned 38 stale references against it.

**37 of the 38 were `cursor.node()`.** The extractor walks each parse tree
with a tree-sitter `TreeCursor`, so `cursor.node()` appears all over this
codebase. The capture normalizes to `cursor::node`, and tree-sitter is an
external crate, so the edge is UNRESOLVED and correctly always will be. Its
bare tail is `node`, and that module has a `fn node()` test helper. Query the
graph about `node`, bare-tail every UNRESOLVED edge onto it, and you get 37
stale references to a helper that is alive, unrenamed, and has nothing to do
with tree-sitter.

**The 38th was a crate-prefixed call** to a symbol that was present and
unrenamed: the call site spells the crate name out, so the capture is a strict
*superset* of the target's crate-relative `qualified_name`, the qualified rules
miss it, and it lands in UNRESOLVED with a bare tail equal to the live symbol's
own name. The naive rule flags a symbol as stale using a reference to that very
symbol.

Note the shape of that failure: the gate got noisier the more ordinary the code
was.

## The fix: stop keeping a second copy of the matching rule

The resolver already forbids exactly the fallback the scan was performing: a
qualified `to_name` is the exclusive territory of the exact-match and
suffix-match rules and never falls back to bare-tail matching if both miss.
That rule is documented on the resolver's own entry point, and it exists
because of two production false positives: `HashMap::new` and, yes,
`cursor.node()`. The scan was a second, independent implementation of "does
this reference name that symbol," disagreeing with the first in precisely the
case the first had been hardened against.

So the fix is not a heuristic. It makes the scan apply `resolve_one`'s rule
instead of a text match, in three cases (`docs/records/833c17f.md`):

1. A **bare** capture matches only within the cascade's candidate-pool key:
   bare name *plus* target type *plus* language family. The type component
   matters: `Lcg { .. }` is captured as a `function`, so the pool never held
   the live `struct Lcg`. The language family comes from
   the resolver's own table, not a second copy. `resolve::language_family`
   became `pub` for that reason.
2. A **qualified** capture must be exact-or-`::`-suffix compatible with a live
   definition's `qualified_name`, *in that direction only*. That constraint
   kills finding #38: a capture that is a superset of the live name is not a
   suffix match against it, and is not evidence of anything being stale.
3. Only when the queried name has **no live definition left** (the
   renamed-away case, reached through deletion tracking) does the bare tail
   alone decide. That is the kill-test's exact shape, and it stays matched.

## The humbler fix in the same commit

The second change is smaller and more embarrassing: drop any capture
containing a newline. A name is a single-line token in all six languages, so a
multi-line `to_name` is never a symbol. It is a wrapped receiver chain
captured whole, like `items\n.iter()\n.find`, unresolvable by construction and
carrying a bare tail (`find`, `bind`, `context`) that collides freely with real
project names. The drop happens at `add_name_edge`, the one choke point all six
extractors share.

The measured effect, as `833c17f` recorded it on the tree as it then stood:
**759 captures dropped**, name-edges considered **5,384 → 4,625**, UNRESOLVED
**4,219 → 3,460**, with the AMBIGUOUS count (28), the derived `file_ref` count
(106) and the fingerprint count (698) bit-identical across the change. That
last part is the check that matters: an UNRESOLVED edge was never a fingerprint
input. Those absolutes have drifted since.
`codegraph index . --project-id self --force` reproduces the shape of that
block today, not the 2026-07-22 figures.

The first fix cannot cover this one. When a rename removes the last live
definition of a name, the scan *correctly* falls back to the bare tail. Before
the filter, a junk multi-line capture's tail was `find`, so a rename with
every caller correctly updated still got rejected.

## What "fixed" means here

**1,015 → 0 stale references, across every file**, via the sweep above. The
acceptance case, `src/graph/dependencies.rs`, goes 38 → 0 and comes back clean
on both counts. The suite went from 229 to 240 green (`cargo test`).

One thing that sweep shows which the headline does not: stale references are
only one of the verdict's two rejection reasons. `is_clean()`
(`src/facade.rs:66`) also requires the ambiguous-refusal list to be empty, and
on a repo-root self-index 35 of 119 files carry at least one (140 in total).
That is not the bug returning. It is the resolver declining to guess between
definitions that genuinely collide. 130 of the 140 come from
`tests/fixtures/`, a deliberate name-collision corpus that a repo-root index
drops into the same namespace as `src/`. Index only `src/`, the scope a real
audit would use, and it falls to 6 refusals in 2 of 32 files, all one true
cause: `get_str` is defined at `src/graph/hub_nodes.rs:23` and again at
`src/graph/mod.rs:148`, so a bare `get_str` call has two live candidates and
the gate refuses to pick. That is the tool working. The two counts are
separate fields on every verdict, and only the stale count is claimed to be
zero here.

`tests/gate_specificity.rs` is the file I'd want a reviewer to open first
(`cargo test --test gate_specificity`, 3 integration tests). It builds one
small tree that is correct by construction, carrying all four false-positive
capture shapes above, then asserts both halves *on that same tree*: a fully
clean verdict for every file individually and all at once (which that fixture
can satisfy only because it plants no name collisions, a property of the
fixture and not a claim about every tree) **and** the converse, that renaming
one definition away while leaving a caller behind still rejects, and that a
*completed* rename stays clean despite the multi-line chain. Pinning both
together is the point: a scan that always returns nothing passes the
specificity test alone. All three fail against pre-fix code.

## This was the second half. Sensitivity came first

The specificity fix only made sense because the gate could already catch the
thing it exists to catch, and that took its own fix two months earlier
(`docs/records/e4572d6.md`). The paths-based entry point queried only *live* symbols,
and an incomplete rename removes the old name from the live set by definition,
so the file whose symbol was renamed away could never be asked about the old
name, and the gate passed silently. A kill-test failure, not a theoretical gap.
The fix records definitions that vanish from a re-indexed file in a
`deleted_symbol` table. Those rows are query targets, never verdicts, since a
rejection still requires a live UNRESOLVED edge on that name. Proof is
`tests/deletion_tracking.rs` (`cargo test --test deletion_tracking`, 7 tests
end-to-end against a real store, in a 192-green suite).

Sensitivity first, then specificity. A gate with only one of them is not
half-useful. It is useless in both directions, just differently.

## What this is not

This is not compiler-grade resolution and does not try to be: no type
inference, no overload, trait, or dynamic-dispatch resolution, no compiler or
LSP integration. Import-informed resolution is Rust-only today, for a data reason
rather than a policy one. Five of the six extractors create import nodes but
never wire them into an edge, so that rule has nothing to read. One wart
survived this fix and the commit says so: the CLI and MCP printers kept
emitting "no live symbol currently has this name" unconditionally whenever the
stale list was non-empty, including when the query did match live symbols. A
message-construction bug, not a scan bug. Fixed since, in the tree you are
reading: the result now carries the live-definition count the scan itself
computed, so the message can no longer claim more than the scan knows.

## The generalization

I spent a commit on 1,015 findings that harmed nothing because a gate which
rejects a tree nobody touched trains its operator to ignore it. The first time
it fires on a clean checkout you read it. The third time you add the flag that
skips it, and from then on it catches nothing, including the one real
incomplete rename that is its only reason to exist.

A false-positive rate is not a quality metric. It is an adoption metric. The
number that mattered here was never 1,015. It was the probability that the
next person to see a rejection believes it.
