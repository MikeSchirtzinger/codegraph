# codegraph

codegraph is a language-agnostic codebase graph indexer: it tree-sitter-parses
a repo across 6 languages (Rust, Go, Java, Python, TypeScript/JavaScript, C/C++)
into a `code_node`/`code_edge` graph in SurrealDB, runs a deterministic
name-resolution cascade over every call/implements/member-of reference so that
`to_id` bindings are real node ids rather than unresolved text, and exposes the
result over a CLI (`codegraph query ...`) and an MCP server (`codegraph serve`)
so agents and tools can ask "what calls this," "what depends on this," "is
this file part of a cycle," and get answers backed by an explicit
RESOLVED/AMBIGUOUS/UNRESOLVED confidence rather than a silent guess. It is not
a compiler: no type inference, no overload resolution, no dynamic dispatch.
See [Known limitations](#known-limitations) below. The design and the defects it fixes
are documented in `specs/resolution-layer-v1.md`. This file documents the
result: what's actually implemented, what each language actually emits, and
what it actually costs to run.

## Quickstart

```bash
cargo build --release
```

### Index a codebase

```bash
codegraph index /path/to/repo --project-id myproject
```

- Defaults to an embedded SurrealDB (`surrealkv://.codegraph/graph.db`) in the
  current directory: no server to run. Pass `--db-url ws://host:port` (or set
  `SURREALDB_URL`) to point at a real SurrealDB server instead.
- `--tier fast|balanced|full` controls how much is extracted (functions/classes
  only; +calls; +everything, see `src/index/mod.rs`'s `IndexingTier`).
  `full` is the default and what every number in this README was measured at.
- `--force` bypasses the SHA-256 content-hash incremental cache
  (`src/index/incremental.rs`) and re-parses every file.
- Re-indexing is incremental by default: unchanged files (by content hash) are
  skipped, and the R1 resolver pass always re-runs project-wide afterward
  (see [Known limitations](#known-limitations) on what "project-wide" costs).

### Query the graph

```bash
codegraph query --project-id myproject --kind summary
codegraph query --project-id myproject --kind search   --name connect
codegraph query --project-id myproject --kind calls     --name main --depth 3
codegraph query --project-id myproject --kind deps      --name connect
codegraph query --project-id myproject --kind rdeps     --name connect --include-ambiguous
codegraph query --project-id myproject --kind circular
codegraph query --project-id myproject --kind hubs
codegraph query --project-id myproject --kind coupling
codegraph stats  --project-id myproject
```

| `--kind` | What it does | Source |
|---|---|---|
| `summary` | Node/language/edge-type counts | `graph::search::project_summary` |
| `search` | Substring match on entity name, optional node-type filter | `graph::search::search_nodes` |
| `calls` | Recursive call-chain trace from a function name | `graph::call_chain` |
| `deps` | Forward BFS over resolved `calls`/`member_of`/`implements` edges | `graph::dependencies::get_dependencies` |
| `rdeps` | Reverse BFS (impact analysis), plus project-wide stale-reference detection | `graph::dependencies::get_reverse_dependencies` |
| `circular` | Cross-file cycles over resolved name-edges + the derived `file_ref` graph | `graph::circular` |
| `hubs` | Highest-degree nodes (architectural hotspots) | `graph::hub_nodes` |
| `coupling` | Afferent/efferent coupling + instability per file | `graph::coupling` |

`deps`/`rdeps`/`calls`/`circular` all default to **RESOLVED edges only**. Pass
`--include-ambiguous` to also surface AMBIGUOUS edges (labeled with their
candidate sets, never silently blended, see
[The resolver cascade](#the-resolver-cascade-r1)). Unresolved references
relevant to the query (e.g. a stale caller of a renamed symbol) are always
shown regardless of that flag. See `graph::dependencies`'s module doc for why.

### Serve over MCP (the amortized path, see Performance)

```bash
codegraph serve --project-id myproject
```

Starts an MCP server on stdio (`rmcp`, newline-delimited JSON-RPC). Point any
MCP client at it (Claude Code, an agent harness, `claude mcp add`, etc.). Four
tools, all backed by the same resolved graph the CLI queries:

| Tool | Equivalent to |
|---|---|
| `codegraph_search` | `query --kind search` |
| `codegraph_impact` | `query --kind rdeps` (reverse deps / "what breaks if I change this") |
| `codegraph_architecture` | `query --kind summary` + `query --kind hubs` combined |
| `codegraph_quality` | `query --kind coupling\|circular\|hubs` (via an `analysis` param) |

One `serve` process opens the store exactly once and answers every
subsequent request against that open connection. See
[Performance](#performance) for what that's worth in milliseconds.

## Structural fingerprints, clones, and refactor-neutrality

Every index run (step 5b, after resolution) hashes each definition-bearing
symbol's rooted 1-hop typed neighborhood: edge types and node kinds under a
complete I-R canonicalizer (`src/canon.rs`, ported from the bend-multiway
spike and differential-tested against an O(n!) oracle), never names. The
result is a persisted `fingerprint` row keyed `(qualified_name, file_path)`,
with exactly one prior generation retained (`src/index/fingerprint.rs`). A
pure rename preserves every hash. An added call moves one. Two capabilities
ride on that:

```bash
# Structural clones: symbols whose neighborhood certificates are identical,
# names ignored. Self-indexing THIS repo (fresh store, every tracked source
# file) finds 36 groups / 143 symbols at the default threshold; the largest
# group is 18 resolver unit tests sharing one harness shape:
codegraph index . --project-id self
codegraph clones --project-id self --min-edges 3          # --json supported
```

That count tracks the tree, so it moves whenever code is added: it read 33/133
when this section was written, 34/134 at commit `8b8b82d` (whose own clippy
type aliases added a group), and 36/143 once
`tests/gate_specificity.rs` and the stale-scan unit tests landed. It is
*not* moved by the extraction capture filter described under
[The resolver cascade (R1)](#the-resolver-cascade-r1): measured on the identical
tree, before and after, 34 groups / 134 symbols both times. An UNRESOLVED
edge was never a fingerprint input, so dropping 759 of them changes no hash.

```bash
# Refactor-neutrality: after an incremental re-index, classify each touched
# symbol preserved/changed/added/removed vs the previous generation. A pure
# rename (definition + callers updated) PROVES neutral: the renamed symbol
# pairs by identical fingerprint and reports renamed_from:
codegraph index <root> --project-id demo --db-url surrealkv://…/graph.db
# …pure-rename a definition + its callers on disk, then re-index (no --force)…
codegraph index <root> --project-id demo --db-url surrealkv://…/graph.db
cargo run --example neutrality_delta -- surrealkv://…/graph.db demo src/renamed.rs src/caller.rs
# → "structure_neutral: true", the renamed symbol preserved with renamed_from.
```

The facade entry is `facade::structural_delta_for_paths` (additive; the
existing `StructuralVerdict` contract is untouched). AMBIGUOUS/UNRESOLVED
edges never enter a fingerprint (external noise must not churn hashes), and
ambiguous rename pairings refuse to guess (the resolver's posture). `--force`
wipes fingerprint history along with deletion history (a force run
re-baselines; the first delta afterward reports everything `added`, same
documented degradation as `deleted_symbol`). Proof:
`tests/structural_fingerprints.rs`, `tests/clone_detection.rs`, and the
`canon`/`fingerprint`/`facade` unit suites.

## The resolver cascade (R1)

Every `calls`/`member_of`/`implements` edge (`graph::NAME_EDGE_TYPES`,
`src/graph/mod.rs`) starts life with `to_id = ""` and a `to_name` captured
verbatim from source. The resolver (`src/index/resolve.rs`) runs a fixed rule
cascade, first rule that narrows the candidate pool to exactly one wins:

| Rule | Matches | Scope |
|---|---|---|
| R1 | exact `qualified_name` match | qualified `to_name` only |
| R2 | qualified-suffix match (`db::connect` binds `myapp::db::connect`) | qualified `to_name` only |
| R3 | same-file bare-name match | bare `to_name` only |
| R4 | import-informed (`use` facts narrow the candidate pool) | bare `to_name`, **Rust-only in v1** |
| R5 | project-unique bare name, scoped to the edge's own language family | bare `to_name` only |
| R6 | terminal: >1 survivor → `AMBIGUOUS` + candidates; 0 → `UNRESOLVED` | either |

A qualified (multi-segment) `to_name` is R1/R2's exclusive territory: it
never falls back to bare-tail matching (R3–R5) if both miss. See the doc
comment on `resolve_one` for two production false-positives (`HashMap::new`,
`cursor.node()`) this rule exists to prevent. "Language family" is a closed
two-entry table (`{c, cpp}` share a linker namespace; `{javascript,
typescript}` share a JS runtime). Everything else is a singleton, which is
what keeps e.g. Python's `connect` and Go's `connect` from colliding in a
polyglot project (`tests/fixtures/polyglot` case c).

**Confidence semantics**, read uniformly by every query (`calls`, `deps`,
`rdeps`, `circular`, MCP `impact`):

- **RESOLVED**: real `to_id`, traversed by default.
- **AMBIGUOUS**: `candidates: [node_id]` populated, never traversed further,
  surfaced only with `--include-ambiguous`. Never silently blended into a
  RESOLVED result. This is the direct fix for D3 (project-wide name
  collisions).
- **UNRESOLVED**: no `to_id`, no candidates. Either an external/stdlib call
  (expected, not an error) or a stale reference to a renamed/removed symbol.
  A `rdeps`/`impact` query additionally scans project-wide for UNRESOLVED
  edges that still name the queried symbol. That is the only way a rename
  that missed a call site is ever surfaced, since an UNRESOLVED edge has
  zero candidates and can't be attributed to any node id otherwise.

  That scan applies **`resolve_one`'s own matching rule**, not a bare-tail
  text match: a bare capture matches by bare name within the cascade's
  candidate-pool key (name + `to_type` + language family), a *qualified*
  capture must be exact-or-`::`-suffix compatible with a live definition's
  `qualified_name`, and only when the queried name has no live definition
  left (the renamed-away case the scan exists for) does the bare tail
  alone decide. Anything looser reports the resolver's own external noise as
  breakage: a plain bare-tail match rejected an **unmodified**
  `src/graph/dependencies.rs` with 38 stale references, 37 of them the
  tree-sitter method call `cursor.node()` colliding with that module's
  `fn node()` test helper. Measured over a fresh self-index, every source
  file queried on its own (`examples/gate_verdict.rs`): **1,015 stale
  references across 37 files → 0 across every file**, with the rename
  kill-tests unchanged. `tests/gate_specificity.rs` holds both halves:
  clean tree ⇒ clean verdict for every file, and the same tree still
  rejected after one incomplete rename.

  Extraction drops any capture containing a newline for the same reason: a
  name is a single-line token in all six languages, so a multi-line
  `to_name` is the raw source text of a wrapped receiver chain
  (`items\n.iter()\n.find`), never a symbol: 759 of them in a self-index,
  all `calls`/UNRESOLVED, whose normalized bare tails (`find`, `bind`,
  `context`) collide freely with real project names. Dropping them lowers a
  self-index's name-edge count by ~14% and its UNRESOLVED count from 4,219
  to 3,460; no RESOLVED or AMBIGUOUS edge, `file_ref`, or fingerprint is
  affected (fingerprints only ever admit materialized `to_id`s, so an
  UNRESOLVED edge was never an input).

The gate-facing paths entry (`facade::impact_verdict_for_paths`) reaches that
same stale scan through **deletion tracking**: every incremental re-index
records the definitions that disappeared from each re-indexed (or vanished)
file in the `deleted_symbol` table (healed when the same `(qualified_name,
file_path)` is defined again, wiped by `--force`) and the facade unions those
names into the symbols it queries for a touched file. Without this, a plan
touching the file whose symbol was renamed away could never query the old name
(the file no longer defines it), and the gate passed silently: the
"incomplete rename" kill-test blind spot (`docs/records/e4572d6.md` is the
fix's own record). Closed 2026-07-14; proof: `tests/deletion_tracking.rs` end-to-end,
`examples/gate_verdict.rs` to reproduce from a shell. Rows are query targets,
never verdicts. A rejection still requires a live UNRESOLVED edge on that
name, which is what keeps retained rows false-positive-free.

## Per-language coverage matrix

No language gets credit for an edge or node kind it doesn't actually emit.
This section is cross-checked directly against each extractor's source
(file:line cited per row), not the aspirational list in `schema.surql`'s
comments.

### Edge types

| `edge_type` | rust | go | java | python | ts/js | c/cpp | Goes through R1? |
|---|:---:|:---:|:---:|:---:|:---:|:---:|---|
| `contains` | yes | yes | yes | yes | yes | yes | No: real `to_id` at extraction time (`EXTRACTED`) |
| `imports` | yes | no | no | no | no | no | No: `EXTRACTED`, and Rust-only |
| `calls` | yes | yes | yes | yes | yes | yes | Yes, the name-edge form (see note below) |
| `implements` | yes | no | no | no | no | no | Yes: Rust-only (`impl Trait for Type`) |
| `member_of` | no | yes | no | no | no | no | Yes: Go-only (method → struct, reverse of `contains`) |
| `references` | no | no | no | no | no | no | Never emitted by any of the 6 (confirmed by grep across every extractor). Explicit non-goal |
| `file_ref` | derived for all 6, uniformly, by `index::resolve::write_file_refs` (not emitted by any per-language extractor) | | | | | | Already RESOLVED-only by construction (only ever derived from RESOLVED cross-file bindings) |

Sources: `src/index/extractors/{rust,go,java,python,typescript,c_cpp}.rs`
(every `add_edge`/`add_name_edge` call site, verified exhaustively: see
`rust.rs:108,296,326,366,394`; `go.rs:167,174,310`; `java.rs:59,134,224`;
`python.rs:119,152,193`; `typescript.rs:70,135,302`; `c_cpp.rs:62,234`).

Two things the table above hides that are worth calling out explicitly:

- **Rust's `calls` edges are two different things.** Line 394's is a real
  name-edge (`to_id=""`, `INFERRED`, goes through the resolver like every
  other language's). Line 366's is different: `extract_macro_invocation`
  creates a synthetic `macro_call` node *per macro invocation site* (every
  `println!`, `vec![]`, …) and points a real-`to_id` `calls` edge
  (`EXTRACTED`) at that synthetic node. It never touches the resolver and it
  does **not** represent a call to the macro's actual definition. It's a
  structural "a macro was invoked here" marker, not identity resolution. This
  is the literal mechanism behind D1's old "only Rust macro invocations get
  real ids" observation.
- **Import nodes are edge-less in 5 of 6 languages.** All six extractors
  create `import`-typed nodes, but only Rust ever wires one into an edge
  (`rust.rs:326`, and only when the `use` has an enclosing container: a
  crate-root `use` with no enclosing block gets no edge either). Go, Java,
  Python, TypeScript, and C/C++ import nodes exist as structural metadata
  with no edge_type connecting them to anything. This is *why* R4
  (import-informed resolution) is Rust-only in v1, not just a policy choice
  independent of the data. There is nothing for R4 to read in the other five
  languages today.

### Node types

| Language | Node types | Source |
|---|---|---|
| Rust | `function`, `struct`, `enum`, `trait`, `impl`, `module`, `import`, `macro_call` | `rust.rs` |
| Go | `function`, `struct`, `interface`, `type_alias`, `import` | `go.rs` |
| Java | `class`, `interface`, `function`, `enum`, `import` | `java.rs` |
| Python | `function`, `class`, `import` | `python.rs` |
| TypeScript/JavaScript | `function`, `class`, `interface`, `type_alias`, `import` | `typescript.rs` |
| C/C++ | `function`, `struct`, `enum`, `class`, `type_alias`, `import` | `c_cpp.rs` |

C/C++'s `class` node type is reachable only from `.cpp` files in practice.
The C tree-sitter grammar has no `class_specifier` node kind, so a `.c` file
can never produce one even though `c_cpp.rs` handles both languages with one
extractor (`src/index/parser.rs` routes both `"c"` and `"cpp"` to
`extractors::c_cpp::extract`).

One more cross-language data point (from `tests/fixtures/README.md`'s own
coverage matrix, cross-checked against `resolve.rs`): because R4 is
Rust-only, the same logical case (a call reached only through an import)
resolves via **r1** in Rust/Go/C++ (qualified call syntax at the call site)
but via **r5** in TypeScript/Python/Java (bare-after-import call site). Same
defect fixed, different rule number, because R4's import slot has nothing to
read outside Rust.

## Known limitations

Stated as plainly as the defects that motivated this spec were:

1. **Nested-container `qualified_name` gaps.** Some deeply-nested containment
   shapes (e.g. multiple levels of inline Rust `mod {}` blocks) aren't
   covered by the fixture suite's containment tests and may not produce a
   fully correct `qualified_name` chain in every case. Open follow-up, not
   yet closed as of this measurement.
2. **Single-process store lock.** Embedded surrealkv holds a datastore-level
   lock on its file. We independently reconfirmed this: two `codegraph index
   --force` processes launched *simultaneously* against the same store. One
   succeeds, the other hard-errors `Database at .../LOCK is already locked by
   another process`. This is real and reproducible under true concurrent-open
   conditions. It is **not**, however, a simple "any second process touching
   an open store always fails": a `codegraph serve` process left running,
   then a separate `codegraph query` invoked ~1.5s later against the same
   store, succeeded without error in our testing. The failure looks
   race/timing-dependent rather than an always-on exclusive hold for the
   whole lifetime of a connection. We did not characterize the exact window
   further (out of this task's scope). The operationally safe rule is still
   **one process per store at a time**. `codegraph serve` is how you get many
   logical queries against one open store without re-opening it.
3. **Store-open cost and indexing throughput.** See
   [Performance](#performance) for the full measurement. The *query-side*
   37,000ms claim (D6) still does not reproduce, but the "tens of
   milliseconds" figure that originally replaced it has since regressed
   12–17x, root cause not yet diagnosed (re-measured 2026-07-30 with
   `scripts/bench/store_open_cost.py`, two independent runs agreeing within
   ~20%). Indexing's own "high wall time, low CPU" signature, previously
   traced to unbatched per-record writes, **is fixed** (`cg-batch`, commit
   `2cf8aeb`). The scale abort that stood here until 2026-08-08
   (deterministic stack overflow on Rust corpora above ~700–750 files at
   `balanced`/`full` tier) **is also fixed**: `canon::refine` returned
   colours unrenormalized, so `canon_search`'s individualization doubling
   compounded, wrapped `usize` at depth 64, and un-individualized the
   search; the recursion turned that non-termination into the abort. The
   search is now an explicit-stack DFS over always-renormalized colourings,
   certificates unchanged (the pinned fixture hash and the brute-force
   differential in `src/canon.rs`'s tests both hold). Verified: a
   1,525-file Rust monorepo that aborted 9/9 now indexes in 181s, exit 0.
   The store-open regression is the one open perf item.
4. **No type inference, no overload/trait/dynamic-dispatch resolution, no
   LSP/compiler integration.** Structural + heuristic matching with disclosed
   confidence, by design (`specs/resolution-layer-v1.md`'s Non-goals). Not a
   gap to be closed, a boundary the project intentionally doesn't cross.
5. **C/C++ namespace containment is absent.** `c_cpp.rs`'s top-level dispatch
   (`src/index/extractors/c_cpp.rs:17-26`) handles `function_definition`,
   `struct_specifier`, `enum_specifier`, `class_specifier`,
   `preproc_include`, `type_definition`. There is no `namespace_definition`
   arm. The C/C++ fixture compensates by naming files to match their
   namespace so the file-basename-inclusive `qualified_name` rule produces
   the right answer without needing namespace extraction
   (`tests/fixtures/README.md` §8).
6. **Go same-package, cross-file resolution has no dedicated tier.** R3 (same
   *file*) doesn't reach across files in the same package/directory, so two
   files in one Go package rely on R5 (project-unique bare name). That is
   correct today, but would go AMBIGUOUS if a second package also happened to
   have a uniquely-named-within-itself function of the same name. A
   same-package tier is a reasonable v2 addition, not built here.
7. **`references` edges are never emitted** by any of the 6 extractors,
   confirmed directly against source (grepped every `add_edge`/`add_name_edge`
   call site). Explicit non-goal, not an oversight.
8. **R3 (fixture/regression test wiring) and R4 (incremental re-resolution)
   were in progress in this same working session** as this measurement was
   taken (see the project's task board). The numbers below exercise the
   shipped R0–R2 resolver/query layer directly via the CLI binary, not via
   `cargo test` assertions.

## Performance

Measured at working-tree state **2026-07-10, pre-commit**, commit
`c3658028` plus uncommitted changes to `Cargo.{toml,lock}`,
`src/{cli,main}.rs`, `src/graph/**`, `src/index/{mod,parser}.rs`,
`src/index/extractors/**`, `src/mcp/server.rs`, `src/schema.surql` (R1–R4
resolver/query work landing in parallel this session: see the R2 commits
`5c8bf23`/`7ab8abd` in particular, which added the `idx_ce_from`/`idx_ce_to`
indexes referenced below). Every number has a re-runnable driver in
`scripts/bench/`; run `./scripts/bench/run_all.sh` from a clean shell to
reproduce all of it, or any script individually (each prints its own repro
command as its first line of output). All scripts create their own scratch
`surrealkv` stores under `mktemp` and clean up after themselves. See
[Known limitations](#known-limitations) #2 for why they never share a store
with each other or with any other process.

`./scripts/bench/run_all.sh` was run twice end-to-end while writing this
section (once standalone, once immediately after a second `cargo build
--release`, on a machine with several other agents' builds/tests running
concurrently in sibling worktrees). The two runs agree on every conclusion
below and on most figures to within ~10-30%. The slower operations (full
`index`, standalone `resolve`) show the widest run-to-run spread, consistent
with real CPU/IO contention from those concurrent processes rather than
noise in the method. The tables below report the first run's numbers.
Treat every figure as "this order of magnitude, reproduced twice," not as a
precise-to-the-millisecond constant.

### 1. Store-open cost: D6's claim, re-measured

`specs/resolution-layer-v1.md` D6: *"measured 37s wall / 0.8s CPU for one
`summary` query on a 301-node store."*

**Repro:** `python3 scripts/bench/store_open_cost.py`

| Store | Cold (first read, new process) | Warm (median of 5) |
|---|---|---|
| tiny (1 file) | 42.5ms | 37.2ms |
| medium (this repo's `src/`, 28 files, 503 nodes) | 67.5ms | 58.5ms |
| large (`tests/fixtures/`, 70 files) | 50.3ms | 40.2ms |
| floor (`codegraph --help`, no DB touch) | n/a | 7.4ms median |

Measured **2026-07-10**. The method: `src/db.rs::connect`'s existing
`tracing::info!("Connected to SurrealDB...")` line (fired at the very end of
opening the embedded engine) is timestamped by the *measuring* process the
instant it arrives on the child's stderr, splitting each call into
`phase_open` (spawn → connect log) and `phase_rest` (connect log → exit).
`phase_open` accounted for essentially all of the wall time at every size
tested (e.g. medium: open≈40-46ms, rest≈14-17ms). We attributed this to
`idx_ce_from`/`idx_ce_to` (added that session, commit `5c8bf23`), an index
that speeds up *queries*, exactly the kind of change that would silently
resolve a query-side complaint without touching indexing at all.

**Re-measured 2026-07-30** (same script,
`scripts/bench/store_open_cost.py`, two independent runs agreeing within
~20%): warm medians
are now **441–991ms**, a **12–17x regression** against the table above,
e.g. the medium store went 58.5ms → 971–992ms warm (854–948ms cold). This is
not machine contention: the script's own no-DB floor row is essentially
unchanged (7.4ms published vs. 8.0–8.3ms measured), while every DB-touching
row regressed double-digit-fold. The phase split localizes it to query
execution, not connecting: for the medium store, `phase_open` is now
90–186ms while `phase_rest` is **758–857ms**. Most of the regression is
*after* the store opens, in the query itself, not the connect that
`idx_ce_from`/`idx_ce_to` was credited with fixing. **Root cause not yet
identified**. This is a known open regression, not a resolved one, flagged
as the highest-value follow-up in the verification receipt.

**D6's original 37,000ms claim itself still does not reproduce at any store
size tested**. Even the regressed numbers above complete in under a
second, cold or warm, so that headline verdict survives even though the
supporting figures it was originally measured against do not. "Cold" here
means "first read in a new process right after the store was written," not
"OS page-cache evicted". This environment has no reliable non-interactive
way to drop the page cache (macOS `purge` needs root; the script tries
`sudo -n purge` and reports plainly when it can't).

### 2. Index+resolve throughput, and where D6's symptom actually moved

**Repro:** `python3 scripts/bench/index_throughput.py`

Measured **2026-07-10**, pre-`cg-batch` (unbatched per-record writes):

| Corpus | Files | Nodes | Edges | Wall | CPU | Wall/CPU | Throughput |
|---|---|---|---|---|---|---|---|
| this repo's `src/` | 28 | 503 | 3,020 | 30.22s | 6.10s | **5.0x** | 0.93 files/s, 17 nodes/s, 100 edges/s |
| `tests/fixtures/` (whole tree) | 70 | 155 | 102 | 2.93s | 0.50s | **5.9x** | 23.9 files/s, 53 nodes/s, 35 edges/s |

Edge counts in the table above also predate the multi-line capture filter
(see [The resolver cascade (R1)](#the-resolver-cascade-r1), which cut
name-edges ~14% for a Rust corpus of this shape) and the `cg-batch` write
path below. Both are already reflected in the 2026-07-30 re-measurement:

**Re-measured 2026-07-30**, post-`cg-batch` (commit `2cf8aeb`; corpora grew
slightly between dates: 28→32 files / 3,523→4,097 records for `src/`; the
fixture tree is bit-identical both times;
`scripts/bench/index_throughput.py` is the driver):

| Corpus | Files | Nodes | Edges | Wall | CPU | files/s | ms/record |
|---|---|---|---|---|---|---|---|
| this repo's `src/` | 32 | 648 | 3,449 | 1.921s | 0.838s | **16.66** | **0.47** |
| `tests/fixtures/` (whole tree) | 70 | 155 | 102 | 1.672s | 0.281s | **41.85** | **6.51** |

**16.66 files/s / 0.47 ms/record for `src/` is an 18x improvement** on the
normalized (ms/record) comparison, the fair one since the corpus grew
between measurements. **Scale ceiling, removed 2026-08-08:** until then,
`codegraph index` at `balanced`/`full` tier deterministically
stack-overflowed and aborted on Rust corpora above **~700–750 files** (root
cause and fix in [Known limitations](#known-limitations) #3). Re-run after
the fix, the same 1,525-file Rust monorepo that aborted 9/9 completes at
`full` tier in **181s wall / 137s CPU** (25,369 symbols fingerprinted,
129,508 name-edges considered), so the throughput above holds at real-repo
scale.

**8.6–11.4ms per record (record = one node or edge) despite a 13.7x
difference in record count was the signature of an unbatched round-trip, not
an algorithm scaling with N**, traced at the time to
`index::store_parsed_file` (`src/index/mod.rs`), which issued one
individually-`.await`ed `CREATE` statement per node and per edge (3,419
sequential round trips for the `src/` store), unlike
`index::resolve::write_updates` (`src/index/resolve.rs`), which already
batched its `UPDATE`s in chunks of `WRITE_CHUNK_SIZE = 250` for the reason
its own doc comment gives ("one already-open db session throughout ...
batched rather than one query per edge"). **Fixed by `cg-batch` (commit
`2cf8aeb`)**, which gave `store_parsed_file` an equivalent batched write
path. Per-record cost now scales linearly
rather than sitting near-constant-but-high: **0.47ms/record at 4,097
records (`src/`) and 0.60ms/record at 96,891 records** (a 700-file Rust
sub-corpus, same 2026-07-30 measurement), a 24x increase in record count
for a 28% increase in per-record cost.

Phase split (connect / schema-DDL / index-body), same stderr-line-watching
method as above, extended with a second watched line
(`"Schema initialized"`):

**2026-07-10** (pre-`cg-batch`):

| Corpus | connect | schema-init | index-body (parse+store+resolve) |
|---|---|---|---|
| `src/` | 67.6ms | 660.0ms | 29,489.9ms |
| fixture tree | 61.9ms | 703.3ms | 2,164.9ms |

**2026-07-30** (post-`cg-batch`; same run of `scripts/bench/index_throughput.py`):

| Corpus | connect | schema-init | index-body (parse+store+resolve) |
|---|---|---|---|
| `src/` | 103.1ms | 372.8ms | 1,445.1ms |
| fixture tree | 150.5ms | 394.0ms | 1,127.9ms |

`index-body` collapsed with the write-batching fix (§2 above); schema-init
did not. It dropped from ~0.66–0.70s to ~0.37–0.39s between the two dates
but the receipt does not attribute that change to `cg-batch` specifically,
and it is now a much larger share of a small index's wall time (24% of the
fixture tree's 2026-07-30 total). Schema DDL re-execution
(`db::init_schema`, run unconditionally by `index` and `resolve`, never by
`query`/`stats`/`serve`/`context`) remains a fixed per-invocation cost
regardless of store size, even though the schema never changes call to
call, and is the next actionable target after the store-open regression in
§1.

**Standalone `codegraph resolve` re-run revises, not confirms, "~2,500
edges <1s":**

| Corpus | Edges reconsidered | Wall | CPU | Wall/CPU | Rate |
|---|---|---|---|---|---|
| `src/` | 2,445 (leftover AMBIGUOUS/UNRESOLVED) | 12.42s | 3.09s | 4.0x | 197 edges/s |
| fixture tree | 44 (leftover AMBIGUOUS/UNRESOLVED) | 1.05s | 0.13s | 7.9x | 42 edges/s |

**Caveat, confirmed by reading `src/index/resolve.rs` directly:** a
standalone rerun's `WHERE ... AND to_id = ''` filter means it only ever
reconsiders the AMBIGUOUS/UNRESOLVED remainder from the prior pass.
RESOLVED edges already carry a real `to_id` and are permanently excluded
from any future rerun by the same design that makes reruns incremental-safe.
There is no external way to force a fresh full-edge-set resolve-only timing
without either re-parsing (which reintroduces the per-record write cost
above) or source instrumentation this task's scope doesn't permit. No
`tracing` call exists inside `resolve.rs` today, so there is no
externally-observable phase boundary between "parsing/storing done" and
"resolving started" within one `index` invocation either. The pure in-memory
cascade (`resolve_all`, HashMap-based, O(n)-ish) is architecturally
incapable of taking multiple seconds of *CPU* at n≈2,400. The 3.09s CPU
figure above almost certainly includes `load_nodes`/`load_unresolved_edges`/
`next_resolution_gen`'s SurrealQL round trips (all real DB queries against a
SCHEMALESS table), not just the cascade itself. **The original "~2,500 edges
<1s" figure does not hold for the full `resolve_project` operation as
measured end-to-end today. It may have referred to the in-memory cascade
alone**, which this measurement cannot isolate from its DB-adapter half
without editing `src/index/resolve.rs` (out of scope here, flagged as a
clean follow-up).

### 3. Serve-mode (MCP) latency vs. CLI-reopen

**Repro:** `python3 scripts/bench/mcp_latency.py` (≥50 requests/tool per the
spec's R5 acceptance criterion; run at 60)

**Spec acceptance criterion: "serve-mode p50 < 100ms on fixture queries."
Met at every tool and every store size tested** (worst case: 33.5ms).

| Target | Tool | serve p50 | serve p95 | CLI-reopen p50 | CLI-reopen p95 | amortization |
|---|---|---|---|---|---|---|
| polyglot fixture | `codegraph_search` | 0.22ms | 0.33ms | 33.02ms | 38.24ms | ~150x |
| polyglot fixture | `codegraph_impact` | 0.38ms | 0.45ms | 33.79ms | 39.35ms | ~89x |
| polyglot fixture | `codegraph_architecture` | 0.85ms | 1.06ms | n/a | n/a | n/a |
| this repo's `src/` | `codegraph_search` | 1.75ms | 2.10ms | 46.58ms | 51.56ms | ~27x |
| this repo's `src/` | `codegraph_impact` (rdeps) | 16.35ms | 16.81ms | 63.00ms | 65.59ms | ~3.9x |
| this repo's `src/` | `codegraph_architecture` | 33.53ms | 34.43ms | n/a | n/a | n/a |

("CLI-reopen" only has two rows per target, `query --kind search` and
`query --kind rdeps`. There's no single CLI query equivalent to the combined
`codegraph_architecture` tool.) The amortization advantage shrinks as the
per-request work grows relative to connect cost. `codegraph_impact` against
the larger `src/` store (16.35ms serve, 2026-07-10 figures) is doing real BFS
+ candidate-set work per request, not just paying/skipping a connect tax.
`codegraph serve` is unambiguously the right choice for repeated queries
against one project, and more so now, not less.

**Re-measured 2026-07-30** (`scripts/bench/mcp_latency.py`, 60
requests/tool): serve-mode's own p50s stayed inside the spec's
<100ms criterion at every tool and every store size. Worst case rose
modestly, 33.53ms → 37.09ms, still comfortably inside the criterion. The
CLI-reopen column is a **known open regression**, the same one documented in
§1 above, independently reproduced by this second script: a one-off CLI
query's store-open-plus-query cost is now **15–23x** slower than the table
above (e.g. `src/` `query rdeps` p50 63.00ms → 1,052.46ms; polyglot `search`
p50 33.02ms → 496.28ms), for a reason **not yet diagnosed**. This is not the
"tens of milliseconds" this section originally reported. One consequence:
the amortization advantage of `serve` over CLI-reopen is now *larger* than
published (~1,700x on the polyglot `search` case vs. the original ~150x),
but for the wrong reason, since the CLI-reopen baseline got more expensive,
not because `serve` got cheaper.

## Storage

An engine swap (including the question of swapping to Turso, Rust's
from-scratch SQLite reimplementation, run embedded/local, not libSQL and not
a hosted service) is **explicitly deferred to v2** per
`specs/resolution-layer-v1.md`'s Non-goals. This task is measurement, not
migration. One piece of guidance for whoever runs that bake-off, grounded in
§2 above: the dominant indexing cost measured in the original (2026-07-10)
numbers was an **application-level** pattern (one unbatched,
individually-awaited `CREATE` per node/edge in `index::store_parsed_file`),
not an intrinsic property of embedded surrealkv, and it **has since been
fixed** (`cg-batch`, commit `2cf8aeb`; see §2's 2026-07-30 re-measurement).
Anyone running a future bake-off should compare engines against codegraph's
*current*, already-batched write path, not the pre-fix numbers this
document's history still shows. A bake-off run against the old unbatched
baseline would misattribute an application-level cost that no longer exists
to "the engine is slow." A fair bake-off should compare engines with a
batched write path on both sides, or it isn't measuring what it thinks it's
measuring.
