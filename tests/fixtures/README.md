# Fixtures for the resolution-layer test suite (R3a)

Ground truth for R0 (`qualified_name` computation) and R1 (the resolver
cascade), encoded as source fixtures + `expected.yaml` manifests. Written
against `specs/resolution-layer-v1.md`. Read that first if you haven't;
this file only documents the fixtures themselves and the interpretive
choices made where the spec doesn't fully pin things down.

This is data only. No Rust test code lives here (that's R3b's job, see the
"For R3b" section at the end).

## Layout

```
tests/fixtures/
  rust/                 one fixture project per language, each with:
  typescript/             - its own minimal project layout (Cargo.toml,
  python/                  package.json, go.mod, pom.xml, ... as idiomatic)
  go/                      - an expected.yaml manifest (schema below)
  java/
  c-cpp/                 mixes C (legacy.c) and C++ (namespaces) — one
                         extractor (c_cpp.rs) covers both
  polyglot/              Python + Go + TypeScript in one project_id;
                         tests the boundary BETWEEN languages, not the
                         cascade within one
  rename-refactor/
    before/              two independent, independently-indexable fixture
    after/                 projects, each with its own expected.yaml
    expected.yaml         + a top-level delta/kill-test assertion
```

Every fixture assumes indexing at tier `Balanced` or `Full`. `calls` edges
are only extracted when `ctx.tier != IndexingTier::Fast` (every extractor
checks this before walking a function body).

## The five required cases, and what each proves

Per the R3a task, every per-language fixture (not polyglot or
rename-refactor, which have their own shape) deliberately contains:

| case | what it is | defect | typically resolved by |
|---|---|---|---|
| a | bare same-file call | D1 (`calls` never returned real functions) | r3 |
| b | module/namespace-qualified cross-file call | D2 (qualified calls never resolved) | r1 or r2 |
| c | two same-named functions in unrelated modules, called ambiguously from a third file | D3 (project-wide collisions blended) | r6 → AMBIGUOUS |
| d | cross-file call cycle (mutual recursion, split across two files) | D4 (`circular` structurally inert cross-file) | varies (r1 or r5) |
| e | call to an external/stdlib symbol | none (new expected behavior, not a regression) | r6 → UNRESOLVED |

Beyond the required five, several fixtures carry **bonus** cases (tagged
`bonus-*` in their manifests) that round out cascade-rule coverage. R4 is
Rust-only in v1 per the spec, so it only appears in `rust/`; a clean,
import-free R5 example only appears in `c-cpp/legacy.c` (see "Contract
interpretations" for why). `go/` additionally carries a **case f**: a
cross-file receiver-method (`member_of`) resolution, added 2026-07-10
after R2's validator flagged that this edge kind (Go's cross-file
receiver fallback, now walked by `deps`/`rdeps`/`impact` alongside
`calls`) had no permanent fixture coverage. Unlike every other case in
this suite it's not a `calls` edge (`edge_type: member_of`, `to_type:
struct`). See the schema section below. The full rule-coverage matrix is
at the bottom of this file.

## `expected.yaml` schema

### Per-project fixtures (`rust/`, `typescript/`, `python/`, `go/`, `java/`,
### `c-cpp/`, `polyglot/`, `rename-refactor/{before,after}`)

Top level:

- `fixture`: short id (matches the directory name).
- `language`: extractor exercised (`rust`, `typescript`, `python`, `go`,
  `java`, `c-cpp`, `polyglot`).
- `root`: fixture path relative to the repo root.
- `description`: free text.
- `nodes`: ground truth for `qualified_name` (R0's accept criterion: "self-
  index shows correct qualified_name for a hand-checked sample per language
  fixture", this list *is* that sample). Each entry:
  - `file`: path relative to the fixture root.
  - `name`: the node's bare name (what the extractor already puts in
    `CodeNode.name`).
  - `node_type`: e.g. `function`, `module`.
  - `qualified_name`: the expected value of the new field R0 adds.
- `edges`: ground truth for the resolver cascade (R1's accept criterion).
  Each entry:
  - `case`: one of `a`..`e` (see table above), `d-cycle-<name>` for the two
    halves of a mutual-recursion cycle, `bonus-<rule>-<detail>` for extra
    cascade coverage beyond the required five, or a fixture-specific letter
    for a coverage addendum outside a-e (currently just Go's `f`, see
    below).
  - `defect`: `D1`/`D2`/`D3`/`D4` if this edge is a named regression test
    for one of the spec's four verified defects, else `null`.
  - `edge_type`: `calls` for every case in this suite **except** Go's case
    f, which is `member_of` (a struct's cross-file receiver-method edge,
    added 2026-07-10, see the coverage matrix note).
  - `from`: `qualified_name` of the source node (the function/method whose
    body contains the call).
  - `from_file`: path of the source node's file, relative to the fixture
    root (redundant with `from` given the qualified_name convention below,
    but saves the R3b agent from having to re-derive it).
  - `to_name`: the callee's **exact source text**, verbatim as the
    extractor's `add_name_edge` would capture it (dots/double-colons
    exactly as written; never normalized). This is what ends up in the raw
    `CodeEdge.to_name` before resolution.
  - `to_type`: `function` for every case in this suite **except** Go's
    case f, where it's `struct` (a `member_of` edge's target is always a
    struct/receiver type, never a function).
  - `confidence`: `RESOLVED` | `AMBIGUOUS` | `UNRESOLVED`, the edge's
    expected final `confidence` after the resolver pass.
  - `resolved_by`: `r1`..`r6`. For `RESOLVED` edges, the specific rule
    (`r1`-`r5`) that fires *first* in cascade order. For `AMBIGUOUS` **and**
    `UNRESOLVED` edges, always `r6`. The spec frames both outcomes as "R6
    terminal" (>1 survivor vs. 0 survivors), so there's no separate
    "r6-ambiguous"/"r6-unresolved" split; `confidence` already disambiguates.
  - `target`: `qualified_name` of the resolved node, when `confidence:
    RESOLVED`. `null` otherwise.
  - `candidates`: list of `qualified_name`s. Populated (2+ entries) only
    when `confidence: AMBIGUOUS`. `[]` (explicit empty list) when
    `confidence: UNRESOLVED` (zero survivors). `null` when `confidence:
    RESOLVED` (not applicable, `target` already names the one candidate).
  - `note`: optional free text for non-obvious reasoning. Present
    wherever it adds real clarity, omitted otherwise.
- `cycles`: ground truth for the derived `file_ref` cross-file cycles
  (D4's fix). Each entry:
  - `files`: the file paths (relative to the fixture root) participating
    in one detected cycle. Order isn't significant.
  - `via`: which edge_type(s), once RESOLVED, produced this cycle in the
    derived `file_ref` graph. Always `calls` in this suite.

### The `rename-refactor/` pair: extra top-level schema

`rename-refactor/before/expected.yaml` and `.../after/expected.yaml` each
follow the per-project schema above exactly (they're two ordinary,
independently-indexable fixture projects). `rename-refactor/expected.yaml`
(the parent, sibling to `before/` and `after/`) is different. It doesn't
restate either side's ground truth, it encodes the **delta** between them:

- `before` / `after`: relative paths to the two sub-manifests.
- `kill_test.qualified_name_before` / `.qualified_name_after`: the renamed
  symbol's identity on each side.
- `kill_test.callers[]`: one entry per caller of interest:
  - `from` / `from_file`: the caller.
  - `before` / `after`: each `{confidence, resolved_by, target}`, i.e. what
    that exact call site resolves to on each side.
  - `must_flag`: whether a correct `impact`/`deps` implementation (or a
    before/after diff) is required to surface this caller as affected by
    the rename.
  - `note`: why.

## Contract interpretations

Places where `specs/resolution-layer-v1.md` doesn't fully pin down a
mechanical rule, and the reading chosen here (so R3b, and whoever
implements R0/R1, aren't left guessing why a manifest says what it says).

**1. `qualified_name` general formula.** Per the spec: "Computed from file
path + `contains` chain." Read as:
`qualified_name = module_path_segments ++ containment_chain ++ [item_name]`,
all joined with `::`. `containment_chain` is the bare names of enclosing
`contains`-chain entities (inline `mod`, `impl`, classes) between the file
and the item. This part is uncontroversial and already inferrable from
existing `contains` edges. `module_path_segments` (derived from the file's
path) is where each language needs its own rule:

**2. File-basename-inclusive vs. directory-only.** Two families:
  - **File-basename-inclusive** (Rust, Python, TypeScript/JavaScript,
    C/C++): each *file* contributes its own basename (extension stripped)
    as a segment. Rust's own module system works this way by design;
    Python/TS treat each file as its own module too; C/C++ gets the same
    treatment **by default for lack of a better signal**. It has neither
    a language-enforced directory-equals-namespace rule (unlike Go/Java)
    nor Rust's per-file-module semantics, so falling back to "the file is
    the unit" is the least-surprising mechanical choice.
  - **Directory-only** (Go, Java): each file's *directory*, not its
    filename, contributes the segment(s); multiple files in one directory
    share the same prefix. Chosen because both languages have a real
    language/tooling-level directory-equals-package convention (Go: one
    package per directory, enforced by the toolchain; Java: package
    declarations conventionally (and under Maven/Gradle, effectively
    always) mirror the directory tree).

  This split is why `tests/fixtures/go/evenodd/{even,odd}.go` (two files,
  one package/directory) share the `evenodd::` prefix, while
  `tests/fixtures/rust/src/db/connection.rs` gets its own `connection`
  segment on top of `db`'s.

**3. Elision.**
  - Rust: the leading `src` path segment is dropped (matches the spec's own
    worked example, `graph::dependencies::resolve_deps` for this very
    repo's `src/graph/dependencies.rs`). **Additionally**, the crate-root
    file (`main.rs`/`lib.rs`) contributes *no* segment at all. Its items
    sit directly at the crate root (`run`, not `main::run`), since in real
    Rust, `src/main.rs`/`src/lib.rs` aren't a module named "main"/"lib",
    they *are* the crate root. This fixture suite avoids `mod.rs`-style
    module roots entirely (uses 2018+-edition sibling-file style
    exclusively) specifically to avoid a second, analogous elision case.
  - **Java: NO elision (orchestrator ruling 2026-07-10).** An earlier draft
    of this file dropped the leading `src/main/java/` Maven/Gradle
    standard-layout prefix, on the theory that it's build-tooling
    boilerplate analogous to Rust's `src/`. That was wrong, and has been
    corrected: R0's verified implementation keeps `src/main/java/` as
    ordinary mechanical path segments (its own cross-check: `Alpha.java` →
    `...::src::main::java::com::example::alpha::Alpha`, file basename
    dropped as expected since Java is directory-only per item 2 above, but
    nothing else elided). Ruling: qualified_name is **strictly mechanical**
    in v1. No per-language, convention-aware stripping beyond what's
    forced by item 2's file-vs-directory split. The spec's own
    "language-mechanical" wording and the "extractors/resolver stay
    dumb, no per-language special-casing" design principle both favor
    this; guessing at conventions (Maven layout, in this case) is exactly
    the maintenance trap the whole spec is designed to avoid. `java/
    expected.yaml` has been updated to match (every qualified_name now
    includes the `src::main::java::` segments).
  - Rust's `src/` + crate-root elision above predates this ruling and
    wasn't itself in question (it's the spec's own worked example, not an
    invented convention), and the crate-root-gets-no-segment part (the one
    piece of it that was *this fixture suite's own* inference beyond the
    one example the spec gives, not a quote from it) is now **confirmed by
    test, 2026-07-10**: `crate_root_files_produce_bare_qualified_names`
    (`src/index/extractors/rust.rs`) pins a top-level fn in `src/main.rs`
    and `src/lib.rs` to a bare `"foo"` qualified_name, no `main::`/`lib::`
    segment. `rust/expected.yaml` needed no revision.
  - No other language gets elision of any kind. Python/TypeScript/Go/C++
    keep their full relative path/directory as given.

**4. R1/R2 require a qualified (multi-segment) `to_name`; R3/R4/R5 are
bare-name-only.** The spec's R2 example (`db::connect` binding
`myapp::db::connect`) is itself multi-segment. Read strictly, a
single-segment `to_name` would *also* trivially satisfy R2's "ends with
`::`+to_name" test against any node whose last segment matches (since
`"X::foo"` always ends with `"::foo"` whenever the bare name matches),
which would make R3/R4/R5 redundant dead code. This suite assumes R1 and R2
only ever fire for a `to_name` that itself contains a separator; a bare
identifier is handled exclusively by R3 → R4 → R5 → R6.

**5. R4 ("import-informed") is Rust-only in v1**, explicit in the spec's
own parenthetical ("Slot exists in v1; only Rust feeds it"). Consequence:
in every other language, a bare call reached only through an import (named
import, `from x import y`, static import, dot-import, …) that doesn't
happen to be same-file (R3) falls through the always-empty R4 straight to
R5. This is why the `even`/`odd` mutual-recursion cycle resolves via **r1**
in Rust/Go/C++ (all three use a qualified/namespaced call at the actual
call site) but via **r5** in TypeScript/Python/Java (all three use a
bare-after-import call site, see each fixture's cycle files for why that's
the idiomatic form in that language).

**6. Ambiguity (case c) needs a language feature that makes a genuinely
bare, unqualified call to a colliding name syntactically valid to *write*.**
Plain "two functions, same bare name, no import at all" doesn't compile in
5 of these 6 languages (only Python tolerates a truly nameless bare
reference via `import *` shadowing). Each fixture uses the closest real
mechanism, chosen to be valid to write and only wrong at the point of
actual (ambiguous) *use*, i.e., something codegraph's non-compiling
resolver can't lean on a compiler to catch:
  - Rust: `use alpha::*; use beta::*;` (glob imports). `rustc` rejects the
    ambiguous *use*, not the imports, with E0659.
  - Go: dot-imports (`import . "…/alpha"`), the only Go import form that
    produces an unqualified name at all; `go build` rejects the collision
    as "redeclared in this block."
  - Java: colliding `import static`. `javac` rejects as "reference … is
    ambiguous."
  - TypeScript/JavaScript: plain script-mode files (no top-level
    import/export at all, hence global scope). Verified with `tsc`
    directly (see "Validation performed" below): it flags this at the
    *declaration* sites as TS2393 "Duplicate function implementation",
    not at the call site, closer in shape to Go's "redeclared in this
    block" than to Rust/Java/C++'s call-site ambiguity errors. Either
    way, it's valid syntax that a real toolchain only rejects via
    type-checking, which codegraph never runs.
  - Python: `from alpha import *` then `from beta import *`, not even a
    compile/runtime error; Python silently lets the second shadow the
    first. The most realistic of the six.
  - C++: `using namespace alpha; using namespace beta;`. Errors only at
    the point of an actually-ambiguous call, not at the directives.

**7. Cross-language candidate scoping (`polyglot/`, case c).** Not
explicitly stated by the spec's cascade rules, but necessary for sane
multi-language behavior: candidates for every rule (R1-R6, but R5 is where
it's actually exercisable, see below) must be implicitly scoped to nodes
reachable from the calling edge's own language. None of these 6 languages
can call across a language boundary at the syntax level, so a same-named
collision between e.g. a Python function and a Go function must never
surface as project-wide ambiguity. `polyglot/expected.yaml`'s case c
(`api/consumer.py` calling `connect`, which also exists in
`worker/main.go`) is the regression test for this. See its `note` for the
exact wrong-answer a language-oblivious implementation would produce.

**8. Known precision gaps these fixtures flag but do *not* assert must be
fixed** (out of v1 scope per the spec's non-goals, or simply not spec'd):
  - **Go: no same-package, cross-file tier.** `evenodd/{even,odd}.go` are
    same package, different files; R3 (same-file) doesn't reach across
    files, so this relies on R5 (project-unique bare name), which happens
    to be correct here but would incorrectly go AMBIGUOUS if another
    package also had a uniquely-named-within-itself function of the same
    name. A same-package tier is a reasonable v2 addition, not built here.
  - **C/C++: the extractor doesn't track namespace containment at all**
    (no `namespace_definition` handling in `c_cpp.rs`, confirmed by
    reading it; only `function_definition`/`struct_specifier`/
    `enum_specifier`/`class_specifier`/`preproc_include`/`type_definition`
    are handled, everything else just falls through to the generic child
    recursion). This fixture's files are deliberately *named* to match
    their namespace (`db.cpp` ↔ `namespace db`, etc.) so the
    file-basename-inclusive rule alone produces the right
    `qualified_name` without depending on namespace extraction. If/when
    namespace containment is added, the algorithm will need to avoid
    double-counting when a file's basename already equals its top-level
    namespace's name.
  - **Java: case b's rule was corrected 2026-07-10 once the extractor fix
    actually landed (resolved_by only, to_name/target unchanged from the
    original submission).** `java.rs`'s `method_invocation` handler
    originally captured *only* the `name` field:
    ```rust
    if let Some(name_node) = node.child_by_field_name("name") {
        let callee = ctx.node_text(name_node).to_string();
        ctx.add_name_edge(caller_id, &callee, "function", "calls", "INFERRED");
    }
    ```
    silently discarding the `object` field (the receiver/qualifier) that
    Rust/TypeScript/Python/Go/C++ all already preserve via their generic
    "whole function-expression text" capture, meaning every Java call
    would have produced a bare `to_name` regardless of qualification, and
    D2 could never have resolved for Java at all. This was reported as GAP
    2 and is now fixed: `java.rs:214-223` calls
    `ctx.node_text(object_node)`, capturing the object field's full text
    verbatim, exactly like every other language's extractor. Case b's call
    (genuinely written fully-qualified in `Main.java` as
    `com.example.db.Db.connect()`) therefore produces the full
    `to_name = "com.example.db.Db.connect"` (five segments), which is a
    **suffix** match (r2) against the eight-segment qualified_name
    (`...::db::Db::connect`; the leading `src::main::java::` is
    build-layout boilerplate no Java source ever writes), not an exact
    match, so r2, not the r1 originally submitted. (A same-day relayed
    report briefly claimed the fix captures only the *immediate* qualifier,
    i.e. `Db.connect` rather than the full object text. That was a
    mechanism misdescription, not what `java.rs:214-223` actually does; it
    produced the right rule for the wrong reason and was reverted once
    R3b's string-strict harness and a direct read of the fix caught the
    mismatch. Full blow-by-blow in `java/expected.yaml` case b's note.)
    Case e needed no change throughout (`System.getenv`'s single-segment
    qualifier is identical either way).

## Coverage matrix (which rule/defect each fixture exercises)

| | a (D1) | b (D2) | c (D3) | d (D4) | e | f (member_of) | bonus |
|---|---|---|---|---|---|---|---|
| rust | r3 | r1 | r6-AMBIG | r1 / r1 | r6-UNRES | none | r4×2, r2 |
| typescript | r3 | r2 | r6-AMBIG | r5 / r5 | r6-UNRES | none | none |
| python | r3 | r1 | r6-AMBIG | r5 / r5 | r6-UNRES | none | none |
| go | r3 | r1 | r6-AMBIG | r5 / r5 | r6-UNRES | r5 | none |
| java | r3 | r2 | r6-AMBIG | r5 / r5 | r6-UNRES | none | none |
| c-cpp | r3 | r1 | r6-AMBIG | r1 / r1 | r6-UNRES | none | r5 (legacy.c) |
| polyglot | r3 (go) | r2 (ts) | r5, cross-language-safe (D3-adjacent) | r5 / r5 (ts) | r6-UNRES (ts) | none | none |

Java's column was corrected 2026-07-10 (case b: r1 → r2) once the `java.rs`
extractor fix landed and was diffed against this manifest. See "Contract
interpretations" for why r2, not r1, is the rule that actually fires. Go's
`f` column was added the same day (`member_of`, not `calls`, see the
schema section) once R2's validator flagged the missing coverage.

Every one of R1-R6 is exercised at least twice across the suite; D1-D4 are
each exercised in all six per-language fixtures plus (for D1/D2/D4)
polyglot; the rename-refactor pair is the standalone D2-adjacent
regression/kill-test described above, not part of this matrix.

## Validation performed

Every fixture was actually built/run with its real toolchain, not just
read back. "valid, idiomatic" in the quality bar is a checked claim here,
not an assumption:

- **Rust**: `cargo build` on `rust/`. The only error is the intended
  E0659 ("`helper` is ambiguous") from `gamma.rs`'s case-c glob imports. A
  scratch copy with that one call site disambiguated (`crate::alpha::helper()`)
  builds clean.
- **Go**: `go build ./...` and `go vet ./...` on `go/`. The only error is
  the intended "Helper redeclared in this block" from `gamma.go`'s
  dot-imports. A scratch copy with that file's dot-imports replaced by a
  named import + qualified call builds, vets, **and runs** clean, printing
  `starting up / alpha helper / IsEven(4) = true / serving ` (the trailing
  space is `Handler.addr`'s unset zero value: expected, `main()` never
  sets it), confirming case f's `handler` package (added 2026-07-10)
  builds and links correctly alongside everything else. `go build
  ./handler/...` and `go vet ./handler/...` also pass in isolation.
- **Java**: `javac` across every `.java` file in `java/`. The only error is
  the intended "reference to helper is ambiguous" from `Gamma.java`'s
  colliding static imports. A scratch copy with that call disambiguated
  (`Alpha.helper()`) compiles clean *and runs*, printing `isEven(4) = true`,
  confirming the mutual-recursion cycle is logically correct, not just
  syntactically valid.
- **C/C++**: `g++ -std=c++17 -Wall -Wextra` (and `gcc -std=c11` for
  `legacy.c`) across every file in `c-cpp/`, then linked as one program.
  The only error is the intended "call to 'helper' is ambiguous" from
  `gamma.cpp`'s using-directives. A scratch copy with that resolved
  (`alpha::helper()`) builds *and links and runs end-to-end*: `starting up
  / alpha helper / legacy helper / is_even(4) = 1`, confirming both the
  legacy-C R5 bonus wiring and the cycle's arithmetic.
- **TypeScript**: `tsc --noEmit` on `typescript/`. The only errors are two
  TS2393 "Duplicate function implementation" diagnostics on `alpha.ts`/
  `beta.ts`. See "Contract interpretations" item 6 for why that's the
  right mechanism for TS specifically (declaration-level, not call-site).
- **Python**: `python3 -m py_compile` / `compileall` across every `.py`
  file in `python/`. Clean, no errors (Python's case-c mechanism, wildcard
  shadowing, isn't a compile-time error by design, so a clean compile here
  is the *expected* result, not evidence the ambiguity is broken).
- **polyglot/**: reuses the same per-language patterns already validated
  above (no new language mechanics), so it wasn't independently recompiled
  beyond the schema/cross-reference checks below.
- **rename-refactor/before/**: `cargo build && cargo run`. Clean, prints
  `helper` twice (once from each caller).
- **rename-refactor/after/**: `cargo build` **fails**:
  `error[E0425]: cannot find function `helper` in module `target`` at
  `stale_caller.rs:10`, exactly where the kill-test says it should. This is
  expected, not a bug: `after/` is deliberately the mid-refactor state
  where one caller was never updated, so real `rustc` independently
  confirms the exact staleness this fixture exists to catch. codegraph
  never runs `cargo build` (it tree-sitter-parses source text, which
  stays syntactically valid either way), so this doesn't block indexing;
  it's the reason a structural tool that works without a successful
  compile is valuable for exactly this scenario (real refactors often
  don't compile cleanly at every intermediate commit). Don't be alarmed if
  you try to `cargo build` this directory and it fails. That's the point.
- **Every `expected.yaml`** (10 files): parsed with `pyyaml`, then checked
  programmatically against the schema documented above. Every `nodes[]`/
  `edges[]` entry has exactly the documented fields (no missing, no
  extra); `confidence`/`resolved_by`/`target`/`candidates` are mutually
  consistent per the rules in "`expected.yaml` schema" (e.g. `RESOLVED` ⇒
  non-null `target` + null `candidates` + `resolved_by` ∈ r1-r5; no
  duplicate `case` ids within a fixture); every `from`/`target`/`candidates`
  qualified_name actually appears in that manifest's own `nodes[]`; every
  `file`/`from_file`/`cycles[].files` path exists on disk under the
  fixture's `root`. All clean.
- Hand cross-checked (pre-completion checklist item 3) a spread of entries
  across languages against the cascade text directly: the R1-vs-R2 split
  in `rust/expected.yaml` (`bonus-r2` vs. case `b`), the R4-only-in-Rust
  consequence that makes `typescript`/`python`/`java`'s cycle cases `r5`
  instead of `r1`, and `polyglot`'s cross-language case c, each traced
  rule-by-rule against §"The resolver cascade" before being written down.

## For R3b (test-wiring)

Not this task's job, noted only so the schema above is read in context:
each `expected.yaml`'s `edges[]` entry is meant to become one assertion of
the shape "index `root`, resolve, find the edge from `from`/`from_file`
whose raw `to_name` equals the given value, assert
`confidence`/`resolved_by`/`target`/`candidates` match." `nodes[]` entries
become "find the node at `file`+`name`+`node_type`, assert its computed
`qualified_name` matches." `cycles[]` entries become "assert the derived
`file_ref` graph contains a cycle across exactly this file set." The
`rename-refactor` pair additionally wants: index `before/`, capture results;
index `after/` (simulating a re-index after the file changes: R4's
territory, but the *shape* of the assertion doesn't need incremental
re-resolution to be meaningful, a fresh full index of `after/` alone
suffices to check the kill-test); assert each `kill_test.callers[]` entry's
`before`/`after` transition holds. None of this is implemented here. No
Rust test code was added under `tests/fixtures/**`, per this task's scope.

**Representation note (orchestrator ruling 2026-07-10):** the DB contract
per the spec text is `candidates` UNSET/`None` for `UNRESOLVED` (the field
is only ever populated for `AMBIGUOUS`). This manifest suite writes
`candidates: []` for `UNRESOLVED` instead of omitting the key (equivalent
test-data notation, not a distinct value) because YAML has no bare "unset"
that isn't just "key absent," and an explicit `[]` reads more clearly in a
hand-authored fixture than a missing key would. **Assertion harnesses must
treat `[] ≡ absent`**: don't assert `candidates == []` literally against
the real DB/API for an `UNRESOLVED` edge; assert it's empty-or-unset.
