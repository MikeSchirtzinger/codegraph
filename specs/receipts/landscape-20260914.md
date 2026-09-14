# landscape-20260914: the subsystem map and the planes overlaid on it

This lane owns `codegraph landscape`, `codegraph plan brief`, the exporters, and the `codegraph
context` integration.

## 1. What shipped

| Thing | Where |
|---|---|
| Subsystem partition, deterministic and explained in the output | `src/landscape.rs` (`Partition`, `PartitionRule`) |
| Typed landscape model, code half and roadmap half | `src/landscape.rs` (`Landscape` and friends) |
| Four renderers, pure functions of the model | `src/plan/export.rs` |
| Agent brief | `src/plan/export.rs::brief` |
| Context file body, landscape and roadmap appended | `src/landscape.rs::context_markdown` |
| Context file writer | `src/context.rs` |
| Tests | `tests/landscape.rs`, `tests/fixtures/planes/landscape-overlay.yaml` |

`run` and `brief` keep the P0 signatures exactly. `LandscapeOptions` keeps
its field set, because `src/main.rs` builds it as a struct literal and
growing it would break a file this lane does not own.

The document builder lives in `src/landscape.rs` and `src/context.rs` is the
writer that puts it on disk. The landscape and its embedding must not be
able to drift into two different partitions of one tree, and keeping the
embedding next to the thing embedded is the cheapest way to guarantee that.

An earlier draft of this receipt gave a different and now wrong reason: that
`src/context.rs` was reachable only from `src/main.rs` and so could not be
tested. That was true when this lane started and stopped being true when
lane L1 landed `736f3e4`, which moved the whole module tree into the library.
The correction matters because it changes what is testable, and §4.4 is the
test that became possible.

## 2. The partition rule

Stated in every rendering's header, so no reader has to reverse engineer the
boxes:

> Subsystems are directory prefixes. Every file starts in its top level
> directory. Any subsystem holding more than 25% of the project's files is
> split one level deeper, to a cap of 3 path segments. A subsystem left with
> fewer than 2 files folds into its parent when the parent is also a
> subsystem. Files at the repo root are grouped as ".".

The 25% threshold was set by measurement, not taste. Both readings come from
the same command on this repo, `codegraph landscape --project-id codegraph
--format text` against the index in §5, run once per threshold. At the 35%
first draft, `src` stayed one box at 44 of 140 files (31.4%) while the test
fixture trees got eight boxes between them: the map upside down, with the
whole product in one rectangle and the fixtures spread out. At 25% `src`
splits into `src`, `src/graph`, `src/index`, `src/mcp`, `src/plan`, and the
inter-subsystem dependency count goes from 2 to 19.

Each subsystem is a hyperedge over its files. Every file lands in exactly
one, which is what makes an incidence between two subsystems a fact about
two disjoint sets rather than an artifact of double counting.

## 3. Two numbers per edge, and why there are two

`channels` counts distinct (from file, to file) dependency pairs between two
subsystems. `references` counts the resolved name edges behind them, with
multiplicity. Both come from the same fact base at two granularities: the
resolved `calls` / `member_of` / `implements` bindings, unioned with the
derived `file_ref` graph the resolver writes from those same bindings.

They are reported side by side and never summed. Adding them would double
count the same facts; reporting only one would hide either how wide the
coupling is or how heavy it is.

## 4. Tests

`cargo test --release --test landscape` on `tests/fixtures/go`, the only
fixture whose files sit in more than one directory and call across those
directories.

```
test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.35s
```

Library unit tests, which include the partition rule and all four renderers:

```
cargo test --release --lib
test result: ok. 182 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.13s
```

Whole suite, in a private `CARGO_TARGET_DIR` for the reason in §7b:

```
cargo test --release      # exit 0
327 passed, 0 failed across 17 targets, tests/landscape.rs 18/18
```

### 4.1 The tests cannot pass on the stub tree

Not "would fail": cannot compile. At the P0 contract commit `3d84be8`, which
is where this lane started and which no later commit changed
`src/landscape.rs` in, the file holds two `bail!` stubs and none of the
items `tests/landscape.rs` imports exist.

```
$ git show HEAD:src/landscape.rs | grep -n "not yet implemented"
132:  "codegraph landscape is not yet implemented (lane L3). ..."
141:  anyhow::bail!("codegraph plan brief is not yet implemented (lane L3). ...")

$ for sym in "pub struct Partition" "build_with_planes" "context_markdown" "PartitionRule"; do
    printf '%-20s %s\n' "$sym" "$(git show HEAD:src/landscape.rs | grep -c "$sym")"; done
pub struct Partition 0
build_with_planes    0
context_markdown     0
PartitionRule        0

$ git cat-file -e HEAD:src/plan/export.rs || echo "no such file at HEAD"
no such file at HEAD
```

### 4.2 The tests have teeth beyond compiling

Three mutations, each confined to files this lane owns, each reverted after
measuring.

| Mutation | Test that caught it |
|---|---|
| Drop the tie-break on `most_coupled_file` | `two_builds_of_one_index_are_byte_identical` |
| Drop an unplaceable touch instead of reporting it | `a_touch_that_names_nothing_is_reported_not_dropped` |
| Stop excluding same-file edges from the channel count | `inter_subsystem_channels_match_an_independent_count` |

```
M1:      test result: FAILED. 17 passed; 1 failed
M2 + M3: test result: FAILED. 16 passed; 2 failed
```

The first of those was not a planted mutation to begin with. The test found
a real non-determinism: `graph::coupling::calculate_file_coupling` returns
ties in whatever order its internal maps produced, so two builds of one
index disagreed on which file was the most coupled in `evenodd` and
`handler`. Fixed by breaking ties on the path as well as the score, and the
same fix applied to hub selection, which had the same latent exposure.

### 4.3 The channel assertion is not the code agreeing with itself

`inter_subsystem_channels_match_an_independent_count` recomputes the whole
channel set inside the test file, from raw `code_node` and `code_edge` rows
and a hand-written file-to-subsystem map, and compares. It never calls
`Partition`. Alongside it, three assertions are derived by hand from the go
fixture's own `expected.yaml`:

- case b (`main.go` calls `db.Connect`, RESOLVED by r1) is the single
  channel from `.` to `db`.
- the case d cycle (`evenodd/even.go` against `evenodd/odd.go`) is internal
  to `evenodd` and is therefore *not* reported as a subsystem cycle.
- case f (`handler::Serve` is a `member_of` `handler::Handler`, cross-file)
  counts as an internal channel of `handler`.

### 4.4 The writer test found a defect in a shipped command

Once `src/context.rs` became a library module, `codegraph context` itself
became testable rather than only the document it renders.
`generate_context_writes_the_whole_document_to_disk` writes the file, renders
the same document a second time, and compares the two byte for byte.

It failed on its first run, before any fix existed for it to confirm. The
only section that differed was **Most Coupled Files**, whose order changed
between two renders of one unchanged index:

```
run 1: evenodd/even.go, gamma.go, evenodd/odd.go, main.go, beta/beta.go, ...
run 2: gamma.go, evenodd/odd.go, evenodd/even.go, beta/beta.go, handler/types.go, ...
```

Root cause, in `src/graph/coupling.rs`:

```rust
results.sort_by_key(|b| std::cmp::Reverse(b.afferent + b.efferent));
results.truncate(limit);
```

The sort key is the score alone, over rows that arrive in hash-map order, so
every tie is ordered by chance. The truncation then happens *inside* the
function, so a tie straddling the cut changes which files appear at all, not
just their order.

The user-visible consequence is that `.codegraph/context.md`, a file meant to
be committed and read by agents, produced a spurious diff on every
regeneration of an unchanged repo.

`src/graph/coupling.rs` belongs to another lane, so this is fixed at the
consumer: `context_markdown` asks for every file, imposes a total order
(score descending, then path), and truncates afterwards. **The root cause is
not fixed.** `codegraph coupling` on the CLI calls the same function with a
limit and has the same exposure. Routed to whoever owns `src/graph/`.

Confirmed end to end on this repo, with the binary built in the private
target directory:

```
$ codegraph context --project-id codegraph --output context-a.md
$ codegraph context --project-id codegraph --output context-b.md
$ diff -q context-a.md context-b.md
(identical)
```

This is the third instance of one bug class in this codebase in one night.
`graph::hub_nodes` had it (fixed by another lane during this shift),
`graph::coupling` has it, and this lane's own `most_coupled_file` selection
had it until `two_builds_of_one_index_are_byte_identical` caught it. Any
`sort_by_key` over rows from a store whose order is not guaranteed, followed
by a truncate, is the same defect. It is worth one sweep rather than three
more discoveries.

## 5. Dogfood

Scratch store, this repo, defaults except the tier and the force flag.

```
$ codegraph index . --project-id codegraph --tier full --force \
    --db-url surrealkv://<scratch>/store2/graph.db
Files:       140 scanned, 140 indexed, 0 unchanged, 0 skipped
Nodes:       1980
Edges:       9071
Elapsed:     12.2s
Resolved:    1967 (22.8%)   Ambiguous: 40 (0.5%)   Unresolved: 6618 (76.7%)
File refs:   168
```

```
$ codegraph landscape --project-id codegraph --format {text,json,mermaid,dot} --output ...
$ codegraph plan brief --project-id codegraph
$ codegraph context --project-id codegraph --output <scratch>/context.md
```

All six commands exit 0, run from the binary built in the private target
directory so the artifact is known to be this lane's code. Output sizes:
markdown 273 lines, mermaid 73, DOT 79, JSON 58,162 bytes, brief 59 lines,
context 423 lines.

Measured on this repo: 140 files, 17 subsystems, 1980 nodes, 21
inter-subsystem dependencies, 4 subsystem pairs that depend on each other in
both directions. Roadmap: 6 planes, 28 items, 40 placed touches, 21
unplaced, all 21 of cause `outside_indexed_tree`.

A project that was never indexed renders rather than failing:

```
$ codegraph landscape --project-id smoke --format text
# Landscape: smoke

0 files, 0 subsystems, 0 nodes, 0 inter-subsystem dependencies.
```

The subsystem table reads the way the architecture actually is:
`src/graph` has instability 0.05 (36 channels arriving, 2 leaving), and
`src/mcp` has 0.80 (2 arriving, 8 leaving). A library and a consumer of it.

### 5.1 Mermaid renders

`mmdc` is not installed here, so the diagram was rendered in a real mermaid
engine instead of eyeballed: mermaid 11 from a CDN, loaded headlessly, the
output inspected in the DOM.

```
{"ok":true,"nodes":27,"edges":34,"clusters":2,"w":1401,"h":2093.09375}
```

27 nodes is 17 subsystems plus 10 overlay work items, 34 edges is 19
dependency edges plus 15 dashed overlay links, 2 clusters is the two active
planes. That matches the source exactly. Screenshot at
`<scratch>/mermaid-render.png`.

### 5.2 Unplaced touches: the distinction that matters

All 21 unplaced touches on this repo are `README.md`, `docs/**`, `specs/**`,
and `.codegraph/planes.yaml`. Those are sound plans the map cannot draw,
because the map is built from the code graph and codegraph indexes source
files. That is a different thing from a stale plan, so the two are separated
by a typed cause rather than blurred into one "unplaced" bucket:

| Cause | Meaning |
|---|---|
| `outside_indexed_tree` | on disk, not indexed |
| `not_on_disk` | not on disk: a stale plan |
| `no_definition` | a `symbol:` touch no indexed definition answers to |

The last two are the staleness signal, and only those two reach the brief.
Twenty lines of "your README is not a source file" would crowd out the part
an agent editing code needs.

## 6. Build and gates

`cargo build --release`: 8 warnings before this lane started, all in
`src/mcp/server.rs` while lane L4 had it mid-edit; 0 after, once L4 landed.
Zero warnings are attributable to this lane at any point. The whole test
build carries one warning, an unused import in `tests/mcp_tools.rs`, which
this lane does not own.

```
$ cargo build --release 2>&1 | grep -c '^warning'
0
```

`cargo clippy --release`: 2 warnings, both in files this lane does not own
(`src/graph/dependencies.rs:479`, `src/plan/ops.rs:1047`). None in
`src/landscape.rs`, `src/plan/export.rs`, or `src/context.rs`.

`credo-lint` on the three documents a person or agent reads:

```
landscape.text   credo-lint: 0 hard, 2 warn
brief.md         credo-lint: 0 hard, 1 warn
context.md       credo-lint: 0 hard, 5 warn
```

One hard violation was found and fixed: the context file opened with
`<!-- AUTO-GENERATED by codegraph. Do not edit. -->`, which trips the
generated-by rule. It now says the same thing without the trailer. The
remaining warnings are the 25% and 3-segment rule parameters, which are
definitions rather than measured claims, one `12 to 17x` figure quoted from
`.codegraph/planes.yaml`, and two false positives on `<!--`.

## 7. Full suite

`cargo test --release` at the time of writing: `tests/landscape.rs` 18/18,
`--lib` 178/178. Two other targets were red, both inside work another lane
had in flight and neither downstream of this one:

- `tests/explain.rs::two_fresh_indexes_produce_byte_identical_query_output`
  (another lane hunting the same bug class from the other end)
- `tests/mcp_tools.rs::verify_chain_rejects_*` (two cases)

Both exercise `graph::explain::verify_chain`, and `src/graph/explain.rs` and
`tests/explain.rs` were both dirty in the working tree at the time. Nothing
under `src/graph/` or `src/mcp/` references `landscape`, `plan::export`, or
`context`:

```
$ grep -rn "landscape\|plan::export\|crate::context" src/graph/ src/mcp/
(no matches)
```

An earlier complete run of the suite, before those lanes' latest edits
landed, was green at 426 passed / 0 failed with `tests/landscape.rs`
included.

## 7b. Every lane on this board shares one CARGO_TARGET_DIR

Worth routing to whoever runs the next board. `CARGO_TARGET_DIR` is set to
`/Volumes/SSD/cargo-target` globally, so every lane, including any running
in its own git worktree at a different commit, writes the same
`libcodegraph.rlib`. Cargo keys that artifact on the package id, which is
identical across checkouts, so one lane's build silently replaces another's.

Observed here twice. `cargo test --release` failed with five "cannot find
function `build_with_planes` in module `landscape`" errors while
`cargo build --release` reported `Finished in 0.24s`, having decided nothing
needed rebuilding, and the source on disk plainly declared every missing
item. `touch src/landscape.rs` and a rebuild took it straight back to 18
passed / 0 failed.

The sharpest version of it: the binary sitting at
`/Volumes/SSD/cargo-target/release/codegraph` reverted to the P0 stub while
this lane was working, with the implementation present and compiling in the
working tree.

```
$ /Volumes/SSD/cargo-target/release/codegraph landscape --project-id x --format text
Error: codegraph landscape is not yet implemented (lane L3). It would render project x as text
```

The consequence is that a green `cargo test` on the shared directory is not
evidence while other lanes are running. Every number in this receipt that
comes from a build was therefore re-measured with a private
`CARGO_TARGET_DIR`, which is the only way to know whose code was linked.

## 8. The seam for lane L2

The overlay reads `.codegraph/planes.yaml` directly with `plan::schema`, so
`landscape` works before `plan sync` has ever run. When L2's `work_touch`
rows exist, their persisted `to_id` and `confidence` replace the lookup in
`landscape::place_touch` and placement becomes a join instead of a search.
Two things change then and nothing above this line does:

1. A `symbol:` touch gets the resolver's own AMBIGUOUS verdict instead of
   this lane's narrowed search, so an item touching a name with several
   definitions can be reported as ambiguous rather than placed in every
   subsystem that holds one.
2. `UnplacedCause::NoDefinition` becomes a stored `UNRESOLVED` confidence,
   which makes `codegraph plan stale` and the landscape's stale list the
   same query rather than two implementations of one idea.

## 9. What is not done

- The landscape and the brief both carry a roadmap section, so
  `.codegraph/context.md` states the roadmap twice at different detail
  levels. The board's section 3.4 asks for exactly that, and it is 420 lines
  rather than 300. Worth a pass once an agent has actually consumed one.
- The partition is over indexed files only, so a repo's documentation tree
  never becomes a box. Drawing `docs/` would mean walking the filesystem,
  which is the indexer's job and not this lane's.
- `landscape::brief` and `codegraph context` both resolve the planes file
  relative to the process working directory. `codegraph landscape` has no
  `--planes-file` flag because that would mean editing `src/cli.rs`, which
  this lane does not own.
