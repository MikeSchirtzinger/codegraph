# codegraph

codegraph is a language-agnostic codebase graph indexer: it tree-sitter-parses
a repo across 6 languages (Rust, Go, Java, Python, TypeScript/JavaScript, C/C++)
into a `code_node`/`code_edge` graph in SurrealDB, runs a deterministic
name-resolution cascade over every call/implements/member-of reference so that
`to_id` bindings are real node ids rather than unresolved text, and exposes the
result over a CLI (`codegraph query ...`) and an MCP server (`codegraph serve`)
so agents and tools can ask "what calls this," "what depends on this," "is
this file part of a cycle," and get answers backed by an explicit
RESOLVED/AMBIGUOUS/UNRESOLVED confidence rather than a silent guess. That same
resolver binds a roadmap file (`.codegraph/planes.yaml`) into the graph as
hyperedges over code, so "what planned work already touches this file" is one
query against the same index, and a structural finding can carry a
machine-checkable evidence chain a second program re-derives instead of
trusting. It is not a compiler: no type inference, no overload resolution, no
dynamic dispatch.
See [Known limitations](#known-limitations) below. The design and the defects it fixes
are documented in `specs/resolution-layer-v1.md`. This file documents the
result: what's actually implemented, what each language actually emits, and
what it actually costs to run.

## Quickstart

```bash
cargo build --release        # or: cargo install --path .
```

### The first run on a repository nobody has indexed

Four commands, no flags:

```bash
cd /path/to/repo
codegraph init            # scaffold .codegraph/, repair .gitignore, name the next command
codegraph doctor          # eleven checks, in the order a confused user asks the questions
codegraph index           # defaults to ".", derives the project id, reports every phase
codegraph query --kind hubs
```

The full transcript of exactly that sequence against a fresh clone of
`spf13/cobra`, with the real output of all four commands, is in
`specs/receipts/run-anywhere-20260914.md` §11.

**Where the project id comes from**, in order, on every subcommand and reported
by `doctor`: the `--project-id` flag, then `CODEGRAPH_PROJECT_ID`, then
`project_id` in `.codegraph/config.toml`, then the sanitized name of the repo
root. The flag is now optional rather than required; passing it explicitly
behaves exactly as it always did.

`init` writes `.codegraph/config.toml`, scaffolds `.codegraph/planes.yaml` when
absent, and repairs `.gitignore`. It is idempotent, and it never replaces an
existing planes file, with or without `--force`: that file is a hand-maintained
roadmap and a three-line template would destroy work no other copy holds. The
gitignore repair rewrites a whole-directory `.codegraph/` rule as
`.codegraph/*` plus negations for the two tracked files, because a
whole-directory rule stops git descending into the directory at all, and a
negation cannot re-include a file whose parent was excluded. Without the
rewrite, a repository carrying the obvious rule silently cannot commit the two
files `init` just told it to commit. Proven against `git check-ignore` rather
than against the text of the file, since only git can say whether an ignore rule
bites (`specs/receipts/run-anywhere-20260914.md` §5).

`doctor` runs eleven checks: `version`, `repo-root`, `project-id`,
`config-file`, `planes-file`, `store-url`, `store-open`, `schema`,
`project-indexed`, `languages`, `next`. Exit is non-zero when any check fails.
An absent schema, a missing `config.toml` and a missing `planes.yaml` are
warnings rather than failures: a store that has never been indexed has no schema
by construction, and `index` applies the DDL before it writes, so reporting it
red would make `init` then `doctor` red on every new repository.
`project-indexed` refuses to guess, and says the project cannot have been
indexed into a store with no schema rather than reporting a node count it never
managed to read. `languages` runs the real `index::discover_source_files` rather
than a second scan of its own, so a file missing from doctor's list is missing
from the index for the same reason, and the strategy line says which reason.

### Index a codebase

```bash
codegraph index                       # the current directory
codegraph index /path/to/repo         # anywhere else
```

**Discovery respects `.gitignore`.** When the walk root is inside a git
repository and `git` is on `PATH`, the candidate file list comes from
`git ls-files --cached --others --exclude-standard`, which is gitignore
semantics by construction: every level's `.gitignore`, `.git/info/exclude`, and
the user's global excludes file. Otherwise the old directory walk runs. Which
one ran is printed and logged. Two deliberate narrowings: the built-in skip list
stays applied on the git path too, because a repository that commits its
`vendor/` tree (normal in Go) or its `node_modules` is tracked by git and that is
not first-party code; and an empty git answer falls back to the walk, with the
reason logged, since empty is what both "no source files here" and "this
directory is itself gitignored" look like. Turning gitignore support on can
therefore only ever narrow what is indexed, never widen it. Zero new
dependencies: delegating to `git` means codegraph's answer to "why is this file
not indexed" is the answer `git check-ignore` gives, which a user can verify
without us.

**Progress is printed per phase**: discovery, change detection, parse and store,
resolve, fingerprint, registry, then a total carrying the split. A terminal gets
one line rewritten in place; a pipe or a file gets appended lines at a tenth of
the cadence, because that is what an agent, a CI job and `2> log` all are.
`CODEGRAPH_PROGRESS` overrides the detection with `tty`, `plain`, `off` or
`auto`. The cadence rule is a pure function (`index::progress_tick_due`), so it
is tested without a clock and without a terminal.

**Tiers.** `--tier fast|balanced|full` controls how much is extracted:
definitions only, plus call edges, plus everything
(`src/index/mod.rs`'s `IndexingTier`). `full` is the default and what every
number in this README was measured at. `--tier` resolves flag, then `tier` in
`.codegraph/config.toml`, then `full`.

> **`balanced` and `full` are measured identical for every language except
> Rust.** Every extractor gates call and reference extraction behind a single
> `if ctx.tier != IndexingTier::Fast` check; only `rust.rs` carries an
> additional `Full`-only gate, for macro-invocation nodes. On zstd (279 C files)
> the two tiers produce the same edge count and the same
> resolved/ambiguous/unresolved split, and their instructions retired match
> within noise: 21.34s and 91,929,957,608 instructions at `balanced`, 20.86s
> and 91,894,114,394 at `full`. `fast` is the only tier that saves anything
> there (11.76s), and what it saves is the resolve phase specifically, because
> `fast` emits no name-edges to resolve. It does not save the store phase or
> the old-graph load, both of which scale with file and node count rather than
> with extraction, which is why `fast` is 56% of `full`'s wall time rather than
> some much smaller fraction.
> Measured in `specs/receipts/index-profile-20260914.md` §4.1.

`--force` bypasses the SHA-256 content-hash incremental cache
(`src/index/incremental.rs`) and re-parses every file. It used to be the
difference between a fast and a very slow resolver write-back as well; both
paths now share one write shape and agree field for field, but every indexing
figure in [Performance](#performance) was still measured with it. See
[Known limitations](#known-limitations) #9.

**Errors say what to do about them.** `index` pointed at a file rather than a
directory used to write a row with an empty `file_path` and exit 0; it now names
the containing directory and exits 1. `calls`, `deps`, `rdeps` and `search`
refuse to run without `--name`, rather than printing "No symbol named '' found"
and exiting 0, which taught a caller something false about their codebase. A
store held open by another process is reported in product terms, with the
driver's own sentence kept as the cause rather than discarded:

```
Error: the code graph store at surrealkv://<path> is open in another process, and it allows one writer at a time.
Either close the other codegraph process (an MCP "codegraph serve" left running is the usual one), or point this command at a different store with --db-url.
```

**Storage defaults to embedded.** `surrealkv://.codegraph/graph.db`, anchored at
the repo root rather than at the working directory, so `cd src && codegraph
query` reads the store it wrote instead of silently creating an empty second one.
No server to run. Pass `--db-url ws://host:port` (or set `SURREALDB_URL`) to
point at a real SurrealDB server instead; a url passed explicitly is never
rewritten.

Re-indexing is incremental by default: unchanged files (by content hash) are
skipped, and the resolver pass always re-runs project-wide afterward (see
[Known limitations](#known-limitations) on what "project-wide" costs).

### Query the graph

```bash
codegraph query --kind summary
codegraph query --kind search   --name connect
codegraph query --kind calls    --name main --depth 3
codegraph query --kind deps     --name connect
codegraph query --kind rdeps    --name connect --include-ambiguous
codegraph query --kind rdeps    --name connect --explain
codegraph query --kind circular
codegraph query --kind hubs
codegraph stats
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

`--explain` attaches a machine-checkable evidence chain to every finding. It is
honored by `rdeps` and ignored by the other kinds. With `--json` the chains land
in an `explanations` field; without it they render under an
`=== Evidence (n chain(s)) ===` heading, so the flag is never silently inert.
See [Evidence chains](#evidence-chains).

### Serve over MCP (the amortized path, see Performance)

```bash
codegraph serve
```

Starts an MCP server on stdio (`rmcp`, newline-delimited JSON-RPC). Point any
MCP client at it (Claude Code, an agent harness, `claude mcp add`). Thirteen
tools, all backed by the same resolved graph the CLI queries:

| Tool | Answers |
|---|---|
| `codegraph_search` | does a symbol with this name exist in the indexed project, and where is it defined |
| `codegraph_impact` | if I change this symbol, what else is affected, and what will the index not vouch for. Takes `explain` and `explain_limit` |
| `codegraph_verify_chain` | is this evidence chain actually true of the index right now |
| `codegraph_architecture` | what is in this project, and what does everything hang off. Takes `file_filter` |
| `codegraph_quality` | where is this codebase structurally risky: coupling, cycles, or hubs |
| `codegraph_clones` | which symbols have the same shape as each other, whatever they are called |
| `codegraph_plan_touching` | what planned work already covers this file or symbol |
| `codegraph_plan_list` | what is on the roadmap right now |
| `codegraph_plan_show` | what exactly does this work item cover, what is it waiting on, how far does it reach |
| `codegraph_plan_collisions` | are two pieces of planned work about to touch the same code |
| `codegraph_plan_stale` | which plans point at code that is no longer there |
| `codegraph_plan_blast` | if this planned work lands, what else is downstream of it |
| `codegraph_plan_sync` | the roadmap file changed, so make the graph match it. The only tool here that writes |

`codegraph_impact`'s `explain_limit` defaults to 10 and orders S and A chains
first, so a truncated prefix keeps the gating findings; the total is always
reported. With `explain` off, the response body is the unchanged pre-explain
function, so the old output cannot drift.

One `serve` process opens the store exactly once and answers every subsequent
request against that open connection. See [Performance](#performance) for what
that is worth in milliseconds. Tool-by-tool evidence, including the
fail-before measurement for `file_filter` (four filters, four identical
sha256s before, four distinct after):
`specs/receipts/mcp-surface-20260914.md`.

## Evidence chains

A finding on its own is an assertion: this reference is stale, the resolver
refused to guess here, this symbol has twelve dependents. A chain is the
derivation behind it, decomposed into links a second program can re-check
against the graph without trusting whatever produced them. `--explain` on
`query --kind rdeps` attaches one per finding, and `codegraph_verify_chain`
checks one.

Three shapes ship (`src/graph/explain.rs`, spec `specs/explain-v1.md`, receipt
`specs/receipts/explain-v1-20260914.md`):

| Shape | Finding | What the chain derives |
|---|---|---|
| **S** | stale reference | the symbol entered the query literally, no live definition of that name exists in the project, this edge still names it, and the cascade reached these rules and bound none |
| **A** | ambiguous refusal | the candidate pool held these node ids, no rule narrowed it to one, and the discriminator that would have is named |
| **D** | dependent | a resolved reverse-dependency path, hop by hop, every hop an edge that exists |

Two properties hold by construction rather than by convention. **Every step is a
typed `Fact` with a checker, and there is no free-text step**, because there is
nowhere to put one: prose for human display is derived from the facts at print
time (`Fact::describe`), never stored beside them. **Verification re-derives
rather than comparing against a copy**: `verify_chain` re-runs the real cascade
(`resolve::resolve_one_traced`) and recomputes candidate pools and
live-definition sets from the node set, so a chain that agrees with a tampered
recording still fails. Five tamper cases are tested, each asserting both the
rejection and which step caught it, and every chain produced over the fixture
corpus verifies clean with all three shapes present, so that test cannot pass
vacuously on a corpus that produced no chains. The rule-attribution the chains
read was pinned by a fixture oracle that failed 102 times over 51 cases before
the change and 0 after. The first link of both symbol-anchored
shapes pins how the symbol entered the query, as an explicit literal argument or
as a `deleted_symbol` record, and neither variant admits a similarity match: a
chain whose every later link is exact but whose first step was fuzzy derives a
clean-looking path from a possibly wrong origin, and nothing downstream can
detect that.

**`no_candidates` and `no_rule_matched` are different unresolved outcomes, and
an audit has to keep them apart.** `no_candidates` means nothing in the project
answers to that name any more, which is what an incomplete rename looks like.
`no_rule_matched` means a definition exists and no rule admitted it, which is
what an external call like `std::env::args` looks like. Reporting both as
"unresolved" hands an audit client a pile of correct external references dressed
as breakage. Both outcomes are now persisted on the edge, alongside the list of
rules the cascade actually attempted, by both the full and the incremental
resolver pass.

**Chain shape N, the neutrality certificate, did not ship.** Its verification
step is re-canonicalization, which lives in `src/canon.rs` and
`src/index/fingerprint.rs`, and both were frozen for this work. A chain could
have been assembled that reports the fingerprint pair and then "verifies" by
re-reading the row it was built from, but that re-checks nothing, and shipping it
beside three shapes that genuinely re-derive would weaken what "machine-checkable"
means everywhere else the word is used here. The fingerprint-based neutrality
verdict in [Structural fingerprints](#structural-fingerprints-clones-and-refactor-neutrality)
is real; the re-derivable chain behind it is not built.

`EXPLAIN_VERSION` is 2, and the verifier refuses a chain from any other version
rather than checking it against rules it was not built under. The wire format
tags each fact with `kind`; version 1 tagged it `fact`, which collided with the
field name one level up, so a consumer reaching for `step["fact"]["node_id"]`
and one reaching for `step["node_id"]` both looked right and serde ignored the
wrong one in silence. Every query ordering that feeds a chain is a total order
now (`hub_nodes` by degree, file, name, then node id, and the same treatment
applied to reverse-dependency groups, stale-reference lists, `coupling`,
`search`, `call_chain` and the chain key sorts themselves), because a bare
degree sort let the same repo indexed twice report a different top-N, and
`coupling` truncating straight after an untied sort changed which files came
back at all. The instability was SurrealDB's row-return order differing
between independently built stores, not node id generation, which is
content-derived and byte-identical across fresh indexes. The determinism test
indexes one fixture four times into four isolated stores and asserts a single
serialization for six query kinds.

## Planes: the roadmap as hyperedges over the code

A planned change is not a pair of nodes, it is a set: one work item touches many
files and symbols at once. That is a hyperedge, and the resolver that binds a
call site to a node id binds a plan's touches with the same
RESOLVED / AMBIGUOUS / UNRESOLVED vocabulary. So an agent about to edit
`src/canon.rs` can ask what planned work already covers it and get an answer
grounded in the same machinery as every other codegraph answer.

`.codegraph/planes.yaml` is the source of truth. The graph is an index over it,
the same relationship codegraph already has with source code: throw the store
away, run `plan sync`, get it back. A tracked file diffs in a PR, survives a
database rebuild, can be edited by a human without a tool, and can be changed by
an agent through the normal review path.

```yaml
version: 1
project: codegraph
planes:
  - id: run-anywhere
    title: Run codegraph on any codebase
    status: active          # planned | active | done | abandoned
    horizon: now            # now | next | later
    summary: One command, sensible defaults, honest errors.
    items:
      - id: RA-1
        title: Default the project id instead of requiring it
        kind: feature       # feature | fix | perf | refactor | docs | research
        status: planned
        touches:            # the hyperedge: a SET of code entities
          - file: src/cli.rs
          - symbol: Commands::Index
          - glob: "src/index/**"
        depends_on: [RA-0]
        spec: specs/resolution-layer-v1.md
        notes: Free-form. Anything an agent picking this up needs to know.
```

Nine subcommands. Every one takes `--json` and emits a typed struct, because the
primary consumer is an agent, not a terminal:

| Command | Answers |
|---|---|
| `plan sync` | ingest the file, resolve every touch, report the confidence split |
| `plan list` | planes and items, filtered by `--plane`, `--status`, `--horizon` |
| `plan show <id>` | one item: its touches with confidence, its dependencies, how far it reaches |
| `plan touching <path or symbol>` | inverse incidence: what planned work covers this |
| `plan collisions` | active items whose touch sets intersect, which is two agents about to collide |
| `plan stale` | items whose touches no longer bind to live code |
| `plan blast <id>` | reverse dependencies over the union of a touch set |
| `plan lint` | schema, dependency cycles, dangling references. Reads the file and nothing else, so it works on a repo that was never indexed |
| `plan brief` | compact, agent-pasteable markdown of the landscape and the roadmap |

**One entry is not one row.** A `glob:` matching four files is four incidences
on the hyperedge, which is what keeps collision detection and blast radius a
plain set intersection. Both counts are reported everywhere and neither is
derivable from the other: `selectors` counts entries as written, `touches`
counts incidences.

**Touch resolution is not the r1 to r6 cascade, deliberately.** R3 prefers a
definition in the calling file, R4 narrows by that file's import facts, R5 by
its language family. A plan has no calling file: `- symbol: sync` is written by
a human in a YAML document, not captured at a call site, so those three rules
have no input, and running them would mean inventing a context. What is reused,
by calling rather than copying, is the context-free part
(`normalize_separators`, `bare_name`), so a plan and a call site agree on what
`a.b.C` means.

A `file:` or `glob:` touch binds to the on-disk path and carries an `indexed`
flag, because a path that exists but has no grammar is a sound plan the graph
cannot draw, not a stale one. `plan stale` therefore flags only the three things
that are actually stale: a path that is not on disk, a `symbol:` that no indexed
definition answers to, and a glob that matched nothing. This repository's own
roadmap is what forced that distinction. Its first sync reported 23 unresolved
touches, and every one of them was a `.md`, `.yaml` or `.surql` file that exists
on disk and has no grammar. Telling a reader to re-index would have been wrong
advice 23 times out of 23, and counting sound plans as stale work makes
`plan stale` unreadable on the repository it was built for. The grammar
question is answered by calling `index::parser::extension_to_language`, not by a
second copy of that table, so a grammar added there changes this answer on the
same commit. Details and the dogfood run:
`specs/receipts/planes-core-20260914.md`.

Measured on this repository's own roadmap at **`d95bad6` plus the uncommitted
working tree at 04:18 on 2026-09-14**, from a release build in an isolated
`CARGO_TARGET_DIR` against a scratch index of the whole tree. Lane L4's two-file
MCP follow-up was still uncommitted when this ran, which is why the node count
is one above the landscape figure lane L3 recorded. The same numbers taken at
the board's final commit are in
`specs/receipts/o-planes-validation-20260914.md`:

```
$ codegraph plan lint
  6 planes, 32 items, 79 touches
  No problems found.

$ codegraph plan sync
  Index:       140 files, 1982 symbols
  Worktree:    336 files, found by git ls-files, so .gitignore is respected
  Written:     6 planes, 32 items, 307 touch rows from 79 selectors
  Selectors:   79 resolved, 0 ambiguous, 0 unresolved
  Touch rows:  307 resolved, 0 ambiguous, 0 unresolved
  Not indexed: 110 of those resolved rows bound to a path the code graph holds nothing for

$ codegraph plan stale
=== Stale plans (0 items, 0 actionable touches) ===
Every touch in the roadmap still binds to live code.

$ codegraph plan collisions
=== Collisions among active work (1 pairs, 3 active items) ===

  RA-7 and RA-8 share 1 node(s)
    RA-7 Multi-repo corpus validation for the run-anywhere onboarding
    RA-8 README, the seeded planes.yaml, and the doc corrections
    via README.md via RA-7 file: README.md and RA-8 file: README.md

$ codegraph plan touching src/graph/explain.rs
=== Planned work touching 'src/graph/explain.rs' (3 items) ===
  CI-6 [feature/planned] Chain shape N, the neutrality certificate, once canon unfreezes
  RA-10 [feature/done] Evidence chains, verify_chain, and --explain on query rdeps
  RF-6 [perf/planned] Scope verify_chain's graph load to the chain it is checking
```

That 110 is the `indexed` flag earning its place: those rows are specs,
receipts, docs and YAML, every one of them on disk and none of them something
the code graph can reach. 79 selectors expanding to 307 rows is the glob
arithmetic in the paragraph above. The collision is real rather than
illustrative: two work items on the roadmap that wrote this README both touch
it, which is exactly the answer an agent about to edit a file wants before it
starts. The `touching` output is trimmed here to one line per item; each item
also names the selector that matched and the node it bound to.

**`codegraph context` appends the roadmap.** Any agent already reading
`.codegraph/context.md` inherits the planes with no further integration work.

## Landscape

`codegraph landscape` renders the current codebase as a hypergraph of subsystems
with the roadmap overlaid on it. The partition rule is stated in every
rendering's header, so no reader has to reverse engineer the boxes:

> Subsystems are directory prefixes. Every file starts in its top level
> directory. Any subsystem holding more than 25% of the project's files is split
> one level deeper, to a cap of 3 path segments. A subsystem left with fewer
> than 2 files folds into its parent when the parent is also a subsystem. Files
> at the repo root are grouped as ".".

25% was measured, not chosen. At the 35% first draft, `src` stayed one box at 44
of 140 files (31.4%) while the test fixture trees got eight boxes between them:
the map upside down, with the whole product in one rectangle. At 25% `src`
splits into `src`, `src/graph`, `src/index`, `src/mcp` and `src/plan`, and the
inter-subsystem dependency count goes from 2 to 19
(`specs/receipts/landscape-20260914.md` §2).

Each subsystem is a hyperedge over its files, and every file lands in exactly
one, which is what makes an incidence between two subsystems a fact about two
disjoint sets rather than an artifact of double counting. Edges carry two
numbers, reported side by side and never summed: `channels` counts distinct
(from file, to file) dependency pairs, `references` counts the resolved name
edges behind them with multiplicity. Adding them would double count the same
facts; reporting only one would hide either how wide the coupling is or how
heavy it is.

```bash
codegraph landscape --format text        # default
codegraph landscape --format json
codegraph landscape --format mermaid
codegraph landscape --format dot
codegraph landscape --format mermaid --output arch.mmd
codegraph plan brief                     # ranked by connectedness, sized for a paste
```

Measured at the same tree state, from the same isolated build and scratch index
as the planes numbers above:

```
$ codegraph landscape --format text
# Landscape: codegraph

140 files, 17 subsystems, 1982 nodes, 21 inter-subsystem dependencies.
```

A project that was never indexed renders rather than failing.

A touch the map cannot place is separated by typed cause rather than blurred
into one bucket: `outside_indexed_tree` (on disk, not indexed), `not_on_disk` (a
stale plan), and `no_definition` (a `symbol:` no indexed definition answers to).
Only the last two are a staleness signal, and only those two reach the brief.
Twenty lines of "your README is not a source file" would crowd out the part an
agent editing code needs.

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
# file) finds 66 groups / 341 symbols at the default threshold; the largest
# group is 26 resolver unit tests sharing one harness shape:
codegraph index . --project-id self
codegraph clones --project-id self --min-edges 3          # --json supported
```

That count tracks the tree, so it moves whenever code is added: it read 33/133
when this section was written, 34/134 at commit `8b8b82d` (whose own clippy
type aliases added a group), 36/143 once
`tests/gate_specificity.rs` and the stale-scan unit tests landed, and **66/341
at `d95bad6` plus the uncommitted working tree at 04:18** (140 files, 1,982
nodes, 9,094 edges, `--tier full --force` into a scratch store, from the same
isolated release build). That last step
is much larger than the ones before it and was **not root-caused**: the obvious
candidate is that the 2026-09-14 board added several thousand lines across
`src/` and `tests/`, and the largest group grew from 18 to 26 members of the
same resolver-test shape, but that was not isolated from any other cause. It is
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
"incomplete rename" kill-test blind spot (commit `e4572d6` is the fix's own
record; this line pointed at a `docs/records/` file that was never written).
Closed 2026-07-14; proof: `tests/deletion_tracking.rs` end-to-end,
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
| TypeScript/JavaScript | `function`, `class`, `interface`, `type_alias`, `object`, `module`, `import` | `typescript.rs` |
| C/C++ | `function`, `struct`, `enum`, `class`, `type_alias`, `import` | `c_cpp.rs` |

TypeScript's `object` is a module-level const bound to an object literal,
which idiomatic TypeScript uses to ship a whole module as one exported
value. Its `module` is synthetic, one per file, named after the file, and
created only for a file whose top level runs code: it owns the references
made by module-scope statements and by callbacks with no enclosing named
definition, which is where all of a test file's code lives. A `module` node
is a container rather than a definition, so it is never a candidate for a
`calls` capture and never satisfies a `- symbol:` roadmap touch; name the
file instead.

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
   37,000ms claim (D6) still does not reproduce. The "tens of
   milliseconds" figure that originally replaced it regressed 12 to 17x
   between 2026-07-10 and 2026-07-30 (`scripts/bench/store_open_cost.py`,
   two independent runs agreeing within ~20%), and that regression is now
   root-caused and fixed: `codegraph query` had called `db::init_schema`
   since commit `6b487e4` (2026-07-10), re-running the full schema DDL, 72
   `DEFINE` statements in `src/schema.surql` plus 47 in
   `src/schema_plan.surql`, on every invocation. Fixed 2026-09-14 in
   `284078b`, which records a schema hash on the store and skips the DDL
   when it matches (`specs/receipts/store-cost-20260914.md`). Indexing's
   own "high wall time, low CPU" signature, previously
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

   Re-measured 2026-09-14 (`specs/receipts/index-profile-20260914.md` §3.1):
   `index`'s wall time minus the sum of its own phases is a flat 0.6 to 2.0s
   on every repo regardless of size (cobra 2.0s, flask 1.1s, this repo 0.6s),
   and the mechanism is named: process start for a 69 MB binary with dylib
   linking, `db::connect`, and `db::init_schema` re-executing 20-odd
   idempotent `DEFINE` statements before `index_project` starts timing. That
   is a fixed tax per invocation, invisible in any "seconds per file" framing
   because it does not shrink per file, and on a four-file repo it dominates
   outright. **Fixed the same day** (`284078b`): the store now records a
   SHA-256 of the schema document and skips the DDL when it matches. Since
   `db::init_schema` is called by both `index` and `query`, this closes the
   query-side regression in the paragraph above as the same bug. Measured
   on an isolated, SHA-pinned build, five interleaved repetitions under
   concurrent load, medians: `codegraph query --kind summary` warm went
   1956.2ms to 224.2ms on this repo's `src/`, 1215.4ms to 617.9ms on `cobra`,
   1071.4ms to 553.0ms on `flask` (`specs/receipts/store-cost-20260914.md`
   §5, §7).
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
9. **`index` with no flags took a slow resolver write-back path. Fixed
   2026-09-14 (`3a517b2`), and the mechanism first published for it was
   wrong.** `--force` selects `resolve::resolve_project`, which deletes and
   then bulk-inserts. Its absence selects `resolve::resolve_incremental`,
   which on a first-ever index still considers every edge, ran the identical
   cascade in memory, and then wrote back through `write_updates`, one
   conditional `UPDATE ... WHERE ...` per edge. Measured on cobra, 4,458
   name-edges, same binary and fresh store either way: resolve phase 40.6s and
   195,523,006,829 instructions retired, against 0.9s with `--force`
   (`specs/receipts/index-profile-20260914.md` §8.4).

   The cause was published as SQL parsing, 4,458 statements parsed instead of
   18. That was wrong, and the first fix built on it made things worse:
   collapsing the parses into one server-side loop over a bound array moved
   instructions retired to 306.2B, 1.57x the wrong way. A conditional `UPDATE`
   has to *find* its rows, so N updates over an N-edge table is quadratic;
   4,458 squared is 19.9M row visits, which at roughly 10k instructions each
   is the 200B actually observed. The fix that landed is the shape `--force`
   already had: delete keyed on the source node with one bound array, then
   bulk insert, with no per-row `WHERE` left anywhere on the path.
   `write_updates` and `EdgeUpdate` are deleted; `write_file_refs` went from
   one `CREATE` per cross-file pair to a chunked bulk insert, which closes the
   other round-trip loop §4.2 of the profile receipt found. After: **0.8s and
   19,244,398,211 instructions, 10.2x fewer**, with both paths producing
   identical output on cobra (652 nodes, 4,588 edges, 935 resolved, 137
   ambiguous, 3,386 unresolved, 60 file refs) and a test asserting that field
   for field across four fixtures. Receipt:
   `specs/receipts/incremental-writeback-20260914.md`.

   The [Performance](#performance) tables below still all predate this and
   were all measured with `--force`, so they are not invalidated by it. They
   are also not re-measured against the new path.
10. **Chain shape N, the neutrality certificate, is not built.** Three
    evidence-chain shapes ship and re-derive on verification (S, A, D); a
    machine-checkable certificate that a change altered nothing does not, for
    the reason in [Evidence chains](#evidence-chains). The fingerprint-based
    neutrality verdict is real and is a different artifact from a chain.
11. **`codegraph_verify_chain` loads the whole project graph per call.**
    `ExplainGraph::load` reads every node, every name-edge and every deletion
    record for the project before checking one chain, so verifying n chains
    over MCP pays that load n times. Correct, and linear in the project rather
    than in the chain. Not yet measured against a large store, and not scoped
    to the chain's own node set.

## Performance

> **Read this before quoting anything below.** Every indexing figure in this
> section was measured with `--force`. Until `3a517b2` that mattered a great
> deal, because `codegraph index` with no flags took a different and much
> slower resolver write-back ([Known limitations](#known-limitations) #9).
> Both paths now share the delete-then-bulk-insert shape and agree field for
> field, but nothing in this section has been re-measured since, so read every
> figure here as a `--force` figure. Two further rules came out of
> the 2026-09-14 measurement work and hold for anything added here: a number
> from a `CARGO_TARGET_DIR` shared with concurrent builds is not quotable,
> because lanes clobber each other's artifacts, and only instructions retired
> stayed stable under this machine's load (1-minute load 22 to 50 moved
> per-phase milliseconds by 75% while instructions retired moved 0.5%).

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

The medium row is this repo's own `src/`, which keeps growing: by
2026-09-14 it was 44 files / 1,387 nodes / 6,817 edges, up from the 28 /
503 / 3,020 above, so a later medium-row figure is not a clean
apples-to-apples against this table without accounting for that growth
(`specs/receipts/store-cost-20260914.md` §7.2).

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
`idx_ce_from`/`idx_ce_to` was credited with fixing. **Root-caused and fixed
2026-09-14** (`specs/receipts/store-cost-20260914.md`): since commit
`6b487e4` (2026-07-10), `codegraph query` called `db::init_schema`, which
re-ran the full schema DDL, 72 `DEFINE` statements in `src/schema.surql`
plus 47 in `src/schema_plan.surql`, on every invocation, and that DDL fell
entirely in `phase_rest`. Fixed in `284078b`, which records a SHA-256 of
each schema document on the store and skips the DDL when it already
matches. Measured on an isolated, SHA-pinned build, five interleaved
repetitions under concurrent load, medians: `codegraph query --kind
summary` warm went 1956.2ms to 224.2ms on this repo's `src/`, 1215.4ms to
617.9ms on `cobra`, 1071.4ms to 553.0ms on `flask`. The medium store's
absolute figures are still not a clean comparison to the July table above;
see the corpus-growth note there.

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
