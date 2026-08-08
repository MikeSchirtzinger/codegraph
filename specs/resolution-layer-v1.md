# Spec: Resolution Layer v1 (make codegraph's graph trustworthy)

> Step 1 of the spine (`frontier/STRATEGY-2026-07-09.md`). Every defect below was
> empirically verified 2026-07-09 by building, self-indexing (26 files → 301 nodes /
> 1,869 edges), and running every query kind. This spec turns those findings into a
> buildable, testable work plan. No rename decisions here (deferred).

## Why this is the highest-leverage work in the portfolio

The same fix simultaneously: (a) removes the governance gate's kill-condition (a graph
that is confidently wrong cannot gate agent dispatch: red-team T2), (b) is the
foundation of proof-carrying synthesis (T1 needs structure it can trust), (c) makes the
MCP context server honestly sellable (red-team's Fourth Option), and (d) is the first
instance of the portfolio's kernel problem: **canonical, verifiable identity of
structure**.

## Verified defects (ground truth, with evidence)

| # | Defect | Evidence |
|---|---|---|
| D1 | `query --kind calls` returns 0 for real functions | `call_chain.rs:68-79` reads `to_id`; every language extractor emits calls via `add_name_edge` (`to_id=""`); only Rust macro invocations get real ids (`rust.rs:366`). Confirmed: `calls --name main` → 0 despite ~15 calls. |
| D2 | Qualified calls never resolve in `deps`/`rdeps`/MCP `impact` | Callee text stored verbatim (`rust.rs:392`): `db::connect` never matches node `name="connect"`. Confirmed: `deps --name main` → 0. |
| D3 | Project-wide name collisions | No module/file scoping: `rdeps --name walk_calls` blended 11 hits from 6 unrelated extractor files. |
| D4 | `circular` structurally inert cross-file | Reads `to_id` for `[calls, imports, references]`: calls' `to_id` empty (D1); `references` never emitted by any extractor (0/6, despite `schema.surql:33` + CLI docs); `imports` Rust-only and same-file by construction (`rust.rs:326`). Confirmed: 0 cycles on self-index. |
| D5 | Edge vocabulary uneven vs. docs | `contains` 5/6 (Go: none), `imports` Rust-only, `implements` Rust-only, `member_of` Go-only, `AMBIGUOUS` confidence never emitted. |
| D6 | Ops/latency | Embedded surrealkv exclusive-locks (2nd process hard-errors); measured 37s wall / 0.8s CPU for one `summary` query on a 301-node store; every CLI call reopens the store. |
| D7 | No safety net | 3 tests total, all ID-hash determinism; zero integration tests; no root README. |

## Design principles (the moat is cheap maintenance)

1. **Extractors stay dumb.** Per-language code remains shallow tree-sitter walks that
   emit *unresolved references*. All intelligence moves into one language-agnostic
   resolver pass. This is the explicit lesson of GitHub archiving stack graphs
   (Sep 2025): per-language semantic depth is the cost curve that kills the category.
2. **Deterministic, staged, honest.** Resolution is a fixed rule cascade. Ties are
   reported as AMBIGUOUS with candidates, never silently blended (D3's failure mode),
   never guessed. Honesty is a product feature; `AMBIGUOUS` finally earns its place in
   the schema.
3. **Resolve at write time, read uniformly.** The resolver materializes `to_id` on the
   edge records themselves. Every query (`calls`, `deps`, `rdeps`, `circular`, MCP)
   then reads one shape. D1 is fixed by making `to_id` real, not by teaching each
   query about name-edges.
4. **Not a compiler.** No type inference, no overload resolution, no dynamic dispatch,
   no LSP. Structural + heuristic with disclosed confidence. (See Non-goals.)

## Data model changes

### `code_node`
- Add `qualified_name: string`. Language-mechanical module path + containment chain +
  name, e.g. `graph::dependencies::resolve_deps` (Rust), `pkg/server.Handler.serve`
  normalized to `pkg::server::Handler::serve`. Computed from file path + `contains`
  chain. Indexed on `(project_id, qualified_name)`.
- Canonical separator: `::` internally; each extractor supplies its natural separators
  (`::`, `.`, `/`), normalized once at node-build time.

### `code_edge`
- Keep the flat SCHEMALESS table (the RELATE dodge in `index/mod.rs:336` stands: no
  reason to churn it in v1).
- Resolver materializes: `to_id` (bound target), `confidence: RESOLVED | AMBIGUOUS |
  UNRESOLVED`, `resolved_by: r1..r6` (provenance rule tag), `resolution_gen: int`,
  `candidates: [node_id]` (AMBIGUOUS only).
- `to_name` is retained verbatim forever. It is the re-resolution input and audit
  trail.

### Derived file graph (new, cheap, language-agnostic)
- `file_ref` edges: `from_file → to_file`, derived from every RESOLVED cross-file
  edge during the resolver pass. This gives module/file-level import structure, and
  therefore cross-file cycle detection, for **all 6 languages with zero per-language
  import extractors** (D4's fix without the maintenance trap).

## The resolver cascade (deterministic; first rule that yields exactly one → RESOLVED)

Input: an edge with `to_id=""`, its `to_name` (normalized to `::`), `to_type`, source
file, and source node.

- **R1** exact `qualified_name` match.
- **R2** qualified-suffix match: candidate's `qualified_name` ends with `::`+`to_name`
  (so `db::connect` binds `myapp::db::connect`).
- **R3** same-file bare-name match.
- **R4** import-informed match: where import facts exist (Rust `use` today), restrict
  candidates to imported paths. (Slot exists in v1; only Rust feeds it.)
- **R5** project-unique bare name: exactly one node in the project has this
  `(name, to_type)` → bind with `resolved_by=r5`.
- **R6** terminal: >1 survivor → `AMBIGUOUS` + `candidates`; 0 survivors →
  `UNRESOLVED` (external/stdlib call: expected, not an error).

Traversal default: `calls`/`deps`/`rdeps`/`circular`/`impact` walk RESOLVED edges only;
`--include-ambiguous` opts in (and MCP responses label them). Determinism property:
identical store contents → identical bindings, independent of insertion order.

## Incremental re-resolution (D-incremental)

Indexing already diffs by content hash (`incremental.rs`). Extend:
1. File X changed → re-resolve all edges **from** X.
2. Compute symbol delta of X (names added/removed) → re-resolve edges anywhere whose
   `to_name` tail matches a delta symbol. Requires a reverse index on
   `(project_id, name)` over nodes (exists: names are queryable; add index if profiling
   says so).
3. Bump `resolution_gen` per pass; stale-gen edges are re-resolvable but never served
   as RESOLVED... (gen mismatch → treated as UNRESOLVED until re-resolved).

## Phases

### R0: Identity groundwork (blocking)
- `qualified_name` on all nodes, all 6 languages; separator normalization.
- Fix Go `contains` (D5's worst gap: Go currently emits none, breaking Go
  qualified names).
- Schema migration for new edge fields + indexes.
- **Accept:** self-index shows correct `qualified_name` for a hand-checked sample per
  language fixture; Go nodes have containment.

### R1: Resolver pass
- Rule cascade R1–R6 as a post-extraction pass (full-project first; incremental in R4).
- **Accept:** on self-index, ≥80% of intra-project call edges RESOLVED, 0 false
  bindings on the fixture manifest (below), collisions come back AMBIGUOUS with
  correct candidate sets.

### R2: Query rewrites
- `calls`, `deps`, `rdeps`, `circular`, MCP `impact`/`architecture` consume
  materialized `to_id` + confidence uniformly; add `--include-ambiguous`.
- `circular` runs over resolved `calls` + derived `file_ref` graph.
- **Accept:** D1 regression dead (`calls --name main` ≥ 10 correct results);
  D2 dead (qualified fixture call resolves); D3 dead (collision fixture: zero blended
  results; ambiguity reported as such); D4 dead (planted cross-file cycle detected in
  every language fixture).

### R3: Fixture + regression suite (the safety net, D7)
- `tests/fixtures/<lang>/` minimal project per language + one polyglot repo, each with
  `expected.yaml`: known calls (bare + qualified), a same-name collision, a cross-file
  cycle, an external/UNRESOLVED call.
- Integration tests: index → resolve → assert manifest. Regression cases lifted
  verbatim from the four confirmed defects. Determinism property test (shuffle
  insertion order → identical bindings).
- **The gate kill-test, operationalized:** a rename/refactor fixture pair (before/
  after) where `impact` must flag the breakage, the exact scenario red-team said a
  skeptical engineer will try in 2 hours.
- **Accept:** suite runs in `cargo test`; every defect D1–D4 has a failing-before /
  passing-after test.

### R4: Incremental re-resolution
- The two-sided re-resolve above wired into the existing hash-diff indexer.
- **Accept:** fixture test. Touch one file, assert only affected edges re-resolved
  (gen counter) and results still match manifest.

### R5: Measure ops honestly (D6; measurement, not migration)
- Instrument and document store-open time (where do the 37 seconds go?); document the
  single-process lock as a known limitation; bless `codegraph serve` as the
  amortized path; record serve-mode p50/p95 on the polyglot fixture.
- **Accept:** numbers in the README with repro commands; serve-mode p50 < 100ms on
  fixture queries or the miss is documented with cause. Storage engine swap (incl.
  the Turso question) is **explicitly v2**. A bake-off doc may cite these numbers.

### Also in v1 (small, honest)
- Root README with a per-language **coverage matrix** (which edge kinds each language
  actually emits). No "language-agnostic" claims the matrix doesn't back (D5).

## Non-goals (v1): scope discipline is the strategy
Type inference; overload/trait/dynamic-dispatch resolution; per-language import
extractors beyond existing Rust `use` (the derived `file_ref` graph covers cycles);
LSP/compiler integration; cross-repo resolution; `references` edge emission;
`implements` expansion beyond Rust; any storage-engine change; any renaming.

## Definition of done (binary)
1. All R0–R5 acceptance criteria pass in CI (`cargo test`).
2. The four confirmed defects each have a regression test that failed before the fix.
3. `rdeps` on a name that exists in N unrelated modules returns per-symbol results or
   AMBIGUOUS, never a blend.
4. Cross-file cycle detected in all 6 language fixtures via `file_ref`.
5. README coverage matrix + measured perf numbers published.
6. Self-index smoke: `calls --name main` correct; `deps` resolves qualified calls;
   re-index after a one-file edit stays consistent.

## Sizing (solo, honest)
R0 ~2-3d · R1 ~3-4d · R2 ~2d · R3 ~3-4d · R4 ~2d · R5 ~1d → **~2.5-3 weeks** solo,
compressible with builder/validator agent pairs per phase (R0 blocks; R1→R2→R3 chain;
R4/R5 parallel after R2).
