# run-anywhere receipt

Objective: codegraph runs on any codebase, cleanly, with no tribal
knowledge. Nine deliverables, D1 through D9, from a ranked gap list.

Baseline for every before/after number below is commit `3d84be8`, built in a
dedicated worktree (`git worktree add <scratchpad>/l1-before 3d84be8`) with
its own `CARGO_TARGET_DIR`. `git stash` was not used: three other lanes hold
uncommitted work in this tree.

The change set was also built and tested in a second isolated worktree
(`<scratchpad>/l1-work`, `3d84be8` plus only the files this lane owns),
because lane L3 was mid-edit on `src/context.rs` in the shared tree and left
the binary crate temporarily unbuildable there. Every number below therefore
describes this lane's commit on its own, which is the thing being reviewed.

## 0. Definition of done

| Gate | Before | After |
|---|---|---|
| `cargo build --release` | exit 0, 0 warnings | exit 0, 0 warnings |
| `cargo test --release` | 304 passed, 0 failed | 323 passed, 0 failed |
| New tests | n/a | 19 (15 in `tests/run_anywhere.rs`, 4 unit tests in `src/init.rs`) |

```
$ cargo build --release 2>&1 | grep -c '^warning'
0
$ cargo test --release       # summed across all 15 targets
323 passed, 0 failed
```

The 19 new tests are the whole delta: the lib unit-test target went 127 to
131 (the four in `src/init.rs`) and one new integration target contributes 15.

### 0b. The commit as it actually landed

Lane L4 committed while this lane was working, so this lane's commit
`e4932ef` sits on top of `7e496c5` rather than on `3d84be8`. The committed
tree was therefore verified on its own, in a third worktree:

```
$ git worktree add <scratchpad>/l1-head e4932ef
$ cd <scratchpad>/l1-head && cargo test --release
16 targets, 359 passed, 0 failed, 0 warnings
```

The two numbers measure different things and both are needed. 304 to 323 is
this lane's delta, isolated from every other lane. 359 is the tree that exists
at `e4932ef`, which also carries L4's MCP work.

One process note for whoever reviews the night's commits. The git index is
shared across this working tree, so `git add <paths>` followed by a bare `git
commit` can capture another lane's files if they stage theirs in between. It
happened once here: a first commit captured L2's staged work as well as this
lane's. It was undone with `git reset --soft HEAD~1`, which touches nothing in
the working tree, and recommitted as `e4932ef` with an explicit pathspec
(`git commit -F <msg> -- <paths>`). L2 lost nothing; their files were still
staged afterward. The pathspec form is the fix, and it was passed to the
orchestrator for the remaining lanes.

No new Cargo dependency. Board hard constraint 2 is untouched, and
`Cargo.toml` was not edited at all: the TTY check is `std::io::IsTerminal`
(stable since 1.70, this toolchain is 1.97.1), gitignore semantics come from
the `git` binary rather than the `ignore` crate, and `tempfile`, `tokio` and
`serde_json` were already present.

`src/canon.rs` and `src/index/fingerprint.rs` were not opened. Hard
constraint 1 holds.

## 1. Fail-before, measured

Every new integration test was run against the pre-change tree. Two of the
fifteen call library API that does not exist at `3d84be8`
(`index::progress_tick_due`, `db::is_store_locked`), so a baseline-expressible
variant was used: the cadence test was removed outright, and the
`is_store_locked` assertion was rewritten as the same substring check
inline. The remaining fourteen compile against `3d84be8` and fail there.

```
$ cd <scratchpad>/l1-before && cargo test --release --test run_anywhere
test result: FAILED. 0 passed; 14 failed; 0 ignored
```

Selected failure reasons, verbatim:

```
a_jsx_component_reaches_the_graph
  the JSX component must be extracted, got: ["loadRows"]
a_store_held_by_another_process_says_so_in_product_terms
  the message must say what happened, got:
explain_attaches_evidence_chains_and_is_absent_without_the_flag
  error: unexpected argument '--explain' found
a_name_taking_query_without_a_name_is_an_error_not_an_empty_answer
  --kind calls with no --name must not exit 0
indexing_a_file_says_to_pass_its_directory
  indexing a file must not succeed as an empty index
doctor_reports_every_check_and_is_honest_about_an_unindexed_project
  doctor omitted the version check
```

After the change, the full file passes:

```
$ cargo test --release --test run_anywhere
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 2. D1: progress

`index` emitted three log lines for a whole run. It now emits a phase line
per phase, a rate-limited per-file line during parse and store, and a total
with the per-phase split.

Phases reported: discovery, change detection, parse+store, resolve,
fingerprint, registry. `file_ref` derivation is **not** a separately timed
phase: both `resolve::resolve_project` and `resolve::resolve_incremental`
build it internally before they return, and `src/index/resolve.rs` belongs to
lane L7. Its size is reported as `File refs:` in the run summary instead.

Style is chosen once per run. A terminal gets one line rewritten in place; a
pipe or a file gets appended lines at a tenth of the cadence, because that is
what an agent, a CI job and `2> log` all are. `CODEGRAPH_PROGRESS` overrides
the detection with `tty`, `plain`, `off` or `auto`.

The cadence rule is a pure function, `index::progress_tick_due`, so it is
measured without a clock or a terminal. Repro:
`cargo test --release --test run_anywhere the_progress_cadence`.

| Style | Ticks when |
|---|---|
| `InPlace` | 25 files have gone by and 100 ms have passed, or 2 s have passed |
| `Plain` | 500 files have gone by, or 10 s have passed |
| `Off` | never |

The 100 ms floor on the in-place path is the one addition to the board's
rule: 25 files can go by in a millisecond on a warm cache, and a terminal
repainted a thousand times a second is worse than no progress at all.

The existing `tracing` lines are unchanged.

## 3. D2: ignore rules

Zero new dependencies, exactly the design the board preferred. When the walk
root is inside a git repository and `git` is on `PATH`, the candidate list
comes from `git ls-files --cached --others --exclude-standard -z`, which is
gitignore semantics by construction: every level's `.gitignore`,
`.git/info/exclude`, and the user's global excludes file, with tracked files
always in and ignored files always out. Otherwise the old walk runs. Which
one ran is logged and printed.

Two deliberate departures from the letter of the brief, both narrowing:

1. **The built-in skip list stays applied on the git path too.** A repository
   that commits its `vendor/` tree (normal in Go) or its `node_modules` is
   tracked by git, and dropping the list would have pulled that in as
   first-party code. Keeping it means turning gitignore support on can only
   ever narrow what is indexed, never widen it, which is what board hard
   constraint 7 asks for.
2. **An empty git answer falls back to the walk**, with the reason logged.
   Empty is ambiguous: it is what a repository with no source files looks
   like, and it is also what indexing a directory that is itself gitignored
   looks like. The fallback costs one walk and cannot produce fewer files.

Measured, on a two-file repository whose `.gitignore` holds `generated/`:

```
$ <pre-change binary> index <repo> --project-id probe --db-url ...
  Files:       2 scanned, 2 indexed, 0 unchanged, 0 skipped

$ <post-change binary> index <repo> --project-id probe --db-url ...
  Discovery:   git ls-files, so .gitignore is respected
  Files:       1 scanned, 1 indexed, 0 unchanged, 0 skipped
```

Three tests: the ignored file is absent from the graph and the tracked one is
present (`a_gitignored_directory_is_not_indexed`), a directory with no
repository still indexes through the walk with the skip list intact
(`a_directory_with_no_repository_still_indexes_through_the_walk`), and an
untracked file is indexed without being committed first
(`an_untracked_file_is_indexed_without_being_committed_first`).

## 4. D3: tier notice

`full` remains the default. On every run, before any work:

```
[codegraph] tier full. Lighter tiers exist: --tier fast indexes definitions only, --tier balanced adds call edges
```

Elapsed time is D1's closing line plus an `Elapsed:` row in the summary.

`--tier` became optional, resolving flag, then `tier` in
`.codegraph/config.toml`, then `full`. With no config file present the
behavior is byte-identical to before.

## 5. D4: init and doctor

Both signatures are P0's, unchanged: `init::run(&InitOptions) -> InitReport`
and `doctor::run(&DoctorOptions) -> DoctorReport`. Fields were added, which
the contract allows; `src/main.rs` still only serializes or Displays them.

**init** creates `.codegraph/`, writes `config.toml`, scaffolds
`planes.yaml` when absent, repairs `.gitignore`, and prints the next command.
It is idempotent: a second run reports "Nothing to do."

`planes.yaml` is never overwritten, with or without `--force`. It is a
hand-maintained roadmap document, replacing one with a three-line template
destroys work no other copy holds, and nothing about init needs to. `--force`
still replaces `config.toml`.

Every key `config.toml` carries is read back by `src/config.rs`:
`project_id` (rung three of P0's resolution order), `tier` (D3), and `db_url`
(section 8 below). A config file whose keys do nothing is worse than no
config file, because editing it looks like it worked.

The scaffolded planes file validates with zero violations, pinned by
`init::tests::starter_planes_file_is_valid`, which calls
`plan::schema::validate` directly.

The `.gitignore` repair is the part with a real failure mode behind it. A
whole-directory rule stops git descending into the directory at all, and a
negation cannot re-include a file whose parent directory was excluded, so a
repository carrying the obvious `.codegraph/` rule silently cannot commit the
two files init just told it to commit. The rule is rewritten as
`.codegraph/*` plus the two negations, which keeps the store ignored while
leaving the directory traversable. Where there is no `.codegraph` rule the
three lines are appended. Where there is no `.gitignore` at all, none is
created: that is a bigger decision than init is entitled to make, and the two
files are already committable without one.

Proven against git itself rather than against the text of the file, since
only git can say whether an ignore rule bites:

```
$ git check-ignore -q .codegraph/planes.yaml   # after init
exit 1 (not ignored)
```

**This repository's own `.gitignore` is fixed in this commit.** Line 2 was
`.codegraph/`.

```
$ for f in .codegraph/planes.yaml .codegraph/config.toml .codegraph/graph.db; do
    git check-ignore -q "$f" && echo "IGNORED  $f" || echo "committable  $f"; done
committable  .codegraph/planes.yaml
committable  .codegraph/config.toml
IGNORED  .codegraph/graph.db
```

L8's `.codegraph/planes.yaml` is therefore now committable. It is left
untracked: that file belongs to L8 and then L6.

**doctor** runs eleven checks in the order a confused user asks the
questions: `version`, `repo-root`, `project-id`, `config-file`,
`planes-file`, `store-url`, `store-open`, `schema`, `project-indexed`,
`languages`, `next`. Exit code is non-zero when any check fails, which
`doctor_exits_non_zero_when_a_check_fails` pins using a planes file with an
empty touch set.

Two judgment calls, both toward not crying wolf on a first run:

- **An absent schema is a warning, not a failure.** A store that has never
  been indexed has no schema by construction, and `index` applies the DDL
  before it writes. Reporting it red would make `init` then `doctor` red on
  every new repository, which is the experience this lane exists to remove.
- **A missing `config.toml` or `planes.yaml` is a warning**, for the same
  reason: neither is required for any other command to work.

`project-indexed` refuses to guess. When the schema probe failed, it says the
project cannot have been indexed into a store with no schema rather than
reporting a node count it never managed to read. `count_rows` propagates
every query error instead of folding it into a zero, because "the query
failed" and "the project has no nodes" are different diagnoses and that
function's whole job is telling them apart.

`languages` runs the real `index::discover_source_files` rather than a second
scan of its own, so a file missing from doctor's list is missing from the
index for the same reason, and the strategy line says which reason.

The store lock is diagnosed rather than crashed on: doctor takes a url, not a
connection, and a lock failure comes back as a failed check with a remedy.

## 6. D5: `--name` where it is needed

`calls`, `deps`, `rdeps` and `search` now refuse to run without `--name`.
Every other kind is untouched, pinned by
`query_kinds_that_take_no_name_are_unchanged` over summary, hubs, coupling
and circular.

```
$ <pre-change>  codegraph query --kind rdeps --project-id probe --db-url ...
=== Reverse Dependencies of '' (depth 3, 0 found) ===
No symbol named '' found.
exit 0

$ <post-change> codegraph query --kind rdeps --project-id probe --db-url ...
Error: --kind rdeps needs a symbol to work on. Pass one with --name:
  codegraph query --kind rdeps --name <symbol>
exit 1
```

The old output is worse than an error: a caller reading "No symbol named ''
found" with a zero exit code learns something false about their codebase.

## 7. D6: the lock message

```
$ <pre-change>
Error: Failed to connect to SurrealDB at surrealkv://<path>
Caused by:
    There was a problem with the datastore: Other error: Database at <path>/LOCK is already locked by another process

$ <post-change>
Error: the code graph store at surrealkv://<path> is open in another process, and it allows one writer at a time.
Either close the other codegraph process (an MCP "codegraph serve" left running is the usual one), or point this command at a different store with --db-url.
Caused by:
    There was a problem with the datastore: Other error: Database at <path>/LOCK is already locked by another process
```

The driver's sentence is kept as the cause rather than discarded. Detection
matches `"is already locked"`, the stable middle of that sentence, so a
reworded prefix or a changed path rendering does not silently turn the
diagnosis off. If the match ever goes stale the raw string comes back, which
is what shipped before: the failure mode is an absent diagnosis, never a
wrong one.

Tested with two live connections to one temp store
(`a_store_held_by_another_process_says_so_in_product_terms`).

## 8. D7: file versus directory, and the default path

`index <file>` did not fail quietly. It wrote a corrupted row:

```
$ <pre-change> codegraph index <repo>/src/keep.rs --project-id filecheck --db-url ...
  Files:       1 scanned, 1 indexed, 0 unchanged, 0 skipped
  Nodes:       1
$ <pre-change> codegraph query --kind search --name kept_function --project-id filecheck --json
[{"node_id":"6b550fe0...","name":"kept_function","node_type":"function","file_path":"","language":"rust","start_line":1}]
```

`file_path` is empty, because the walk root and the file are the same path
and `strip_prefix` leaves nothing. Now:

```
$ <post-change> codegraph index <repo>/src/keep.rs ...
Error: <repo>/src/keep.rs is a file, and codegraph indexes directories. Point it at the directory that contains the file instead:
  codegraph index <repo>/src
```

The check lives in `index::index_project`, so library callers get it too.

`index` with no path now defaults to `.`.

**One change beyond the letter of the D list, and the reason for it.** The
embedded store default was `surrealkv://.codegraph/graph.db`, relative to the
working directory, so `cd src && codegraph query` created a second empty
store at `src/.codegraph/graph.db` and reported an empty graph with nothing
on screen to say why. `config::db_url` now anchors a relative surrealkv path
at the repo root. This is in scope because D4 requires doctor to report the
store path, and a store path that changes depending on which directory you
are standing in is not a diagnosis. Backward compatibility holds: every
invocation in the README is run from the repo root and resolves to the same
path it always did, an explicit `--db-url` or `SURREALDB_URL` is passed
through untouched including a relative one, remote urls are never rewritten,
and `db::connect(None)` is unchanged for library callers.

## 9. D8: JSX

`src/index/parser.rs` routes `.jsx` to `LANGUAGE_TSX`, the same grammar
`.tsx` already used. `.mjs` and `.cjs` stay on the plain TypeScript grammar,
with the reason in a comment: they are ordinary JavaScript modules,
JavaScript is a subset of TypeScript, and neither extension is
conventionally JSX-bearing.

Fixture `tests/fixtures/run_anywhere/jsx/` holds one `.jsx` file with two
functions (one whose body is a JSX element) and one plain `.mjs` module.
Before the change, the whole `.jsx` file yields nothing:

```
the JSX component must be extracted, got: ["loadRows"]
```

`loadRows` is the `.mjs` function. Both `.jsx` functions were lost, because
the plain grammar parses `<div>` as a comparison and turns the body into an
error node. After the change all three are extracted.

This is not a parse error the old code reported: `files_skipped` was 0 and no
error was recorded. The symbols simply were not there.

## 10. D9: `--explain`

Wired per L7's design: `--explain` on `query`, honored by `rdeps`, routed
through `facade::explain_chains_for_symbol`, which is the function L7 built
for this caller (it reuses the open connection rather than opening a second
one against the same embedded store).

With `--json` the chains land in `explanations`. Without `--json` they are
rendered through `Chain::render()` under an `=== Evidence (n chain(s)) ===`
heading, so the flag is not silently inert in text mode.

`explain_attaches_evidence_chains_and_is_absent_without_the_flag` asserts
three things on the `rename-refactor/after` fixture: the field is absent
without the flag, the field is a non-empty array with it, and stripping
`explanations` off the explained verdict yields output identical to the plain
one. That last assertion is the one that matters: explaining a verdict must
not change it.

## 11. End to end on a real repository

`git clone --depth 1 https://github.com/spf13/cobra`, then four commands with
no flags at all. No `--project-id`, no `--db-url`, no `--tier`.
`RUST_LOG=codegraph=warn` only to keep the transcript readable.

<!-- credo-lint:allow-fenced Verbatim CLI output. The em dashes are printed by the existing hub-query printer in src/main.rs, which lane L6 owns and this lane did not edit. Rewriting them here would make the transcript a paraphrase. -->
```
$ codegraph init
=== codegraph init ===
  Root:        <scratchpad>/firstrun/cobra
  Project:     cobra
  created      <scratchpad>/firstrun/cobra/.codegraph/config.toml
  created      <scratchpad>/firstrun/cobra/.codegraph/planes.yaml
  .gitignore   added .codegraph/* with negations for planes.yaml and config.toml

  Next: codegraph index

$ codegraph doctor
=== codegraph doctor: cobra ===
  Root: <scratchpad>/firstrun/cobra

  [  ok] version: codegraph 0.1.0
  [  ok] repo-root: <scratchpad>/firstrun/cobra (nearest ancestor holding a .git entry)
  [  ok] project-id: cobra from project_id in <scratchpad>/firstrun/cobra/.codegraph/config.toml
  [  ok] config-file: <scratchpad>/firstrun/cobra/.codegraph/config.toml declares project_id "cobra"
  [  ok] planes-file: <scratchpad>/firstrun/cobra/.codegraph/planes.yaml is valid: 1 plane(s), 1 work item(s)
  [  ok] store-url: surrealkv://<scratchpad>/firstrun/cobra/.codegraph/graph.db
  [  ok] store-open: opened for writing
  [warn] schema: the store carries no codegraph schema yet: The table 'code_node' does not exist
         fix: run codegraph index, which applies the schema before it writes
  [warn] project-indexed: project "cobra" cannot have been indexed into a store with no schema
         fix: run codegraph index
  [  ok] languages: 36 file(s) via git ls-files, so .gitignore is respected: go 36
  [  ok] next: codegraph index
exit=0

$ codegraph index
[codegraph] tier full. Lighter tiers exist: --tier fast indexes definitions only, --tier balanced adds call edges
[codegraph] discovery starting at 0.0s
[codegraph]   36 source files found by git ls-files, so .gitignore is respected
[codegraph] change detection starting at 0.0s
[codegraph] parse+store starting at 0.0s
[codegraph]   36/36 files parsed and stored
[codegraph] resolve starting at 1.0s
[codegraph] fingerprint starting at 67.6s
[codegraph] registry starting at 68.1s
[codegraph] done in 68.1s (discovery 0.0s, change detection 0.0s, parse+store 0.9s, resolve 66.7s, fingerprint 0.4s, registry 0.0s)

=== Indexing Complete ===
  Project:     cobra
  Tier:        full
  Discovery:   git ls-files, so .gitignore is respected
  Elapsed:     68.1s
  Files:       36 scanned, 36 indexed, 0 unchanged, 0 skipped
  Nodes:       652
  Edges:       4588

=== Resolution (R1) ===
  Edges:       4458 name-edges considered
  Resolved:    935 (21.0%)
  Ambiguous:   137 (3.1%)
  Unresolved:  3386 (76.0%)
  File refs:   60

=== Fingerprints ===
  Symbols:     610 fingerprinted (generation 1)
  Delta:       0 changed, 610 added, 0 removed vs previous generation

$ codegraph query --kind hubs --limit 5
=== Hub Nodes (top 5) ===

  executeCommand (function) — in:306 out:1 total:307 — command_test.go
  checkStringContains (function) — in:120 out:2 total:122 — command_test.go
  checkStringContains (function) — in:120 out:2 total:122 — doc/cmd_test.go
  TestBashCompletions (function) — in:0 out:91 total:91 — bash_completions_test.go
  getCompletions (function) — in:0 out:79 total:79 — completions.go
```

### 11b. A measurement the phase split hands to lane L9

Every number in this section is measured, and read off the `codegraph index`
transcript in section 11 above. Repro:
`cd <a fresh clone of cobra> && codegraph index`.

An earlier note read cobra's 31.7 s as "indexing cost at 0.88 s/file, 5x the
0.17 s/file measured on a large Rust repository". The phase split says
otherwise: parse and store is
0.9 s for 36 files, and the resolver pass is 66.7 s of the 68.1 s total.
Whatever makes small repositories slow per file, it is not parsing.

Two runs of the same 36 files on this machine gave 45.2 s and 66.7 s for the
resolve phase. The spread is machine load: five lanes were compiling and
testing concurrently. The split within a run is still valid, since both
phases are measured under the same load; the absolute seconds are not
comparable across runs, and nothing here should be quoted as a benchmark.
This is a pointer for `specs/receipts/index-profile-20260914.md`, not a
result.

## 12. Not done, and why

- **Chain shape N, and anything in `src/canon.rs` or
  `src/index/fingerprint.rs`.** Frozen by board hard constraint 1. Not
  opened.
- **`file_ref` as a separately timed phase.** It is a sub-step inside
  `resolve::resolve_project` and `resolve::resolve_incremental`, both in
  lane L7's `src/index/resolve.rs`. Splitting it would have meant editing
  another lane's file. Its size is reported instead of its time.
- **The `ignore` crate.** The zero-dependency design does the job, and
  delegating to `git` means codegraph's answer to "why is this file not
  indexed" is the same answer `git check-ignore` gives, which is the answer a
  user can verify.
- **`README.md`.** L6 owns it. The edits it should make are in this lane's
  report.
- **Running `codegraph init` in this repository.** The board asked only for
  the `.gitignore` fix here. Writing a `.codegraph/config.toml` mid-board
  would put a new untracked file under four concurrent lanes with no
  instruction to expect it.
- **`SurrealDB signin failed ... proceeding without auth` on every embedded
  run.** A warning printed on every single command against an embedded
  store, where auth does not exist and the failure is expected. It is noise
  on a first run and it looks like a problem. `src/db.rs` is this lane's
  file, so this was reachable, but the line predates the board and
  downgrading it is a behavior change nobody asked for. Flagged rather than
  taken.

  **Correction, 2026-09-14, integration pass.** Taken after this receipt was
  written. `736f3e4` attempts signin only for a real server, decided by url
  scheme matched positively, so an embedded open logs nothing and an
  unrecognized scheme still gets its attempt and its warning. Pinned by
  `opening_an_embedded_store_logs_no_warning` (`tests/run_anywhere.rs:554`).
  The entry above is left as written so the lane's reasoning at the time
  stays legible.

## 13. The starter planes file now resolves on the repo it is written into (added 2026-09-14)

Lane L5's corpus after-pass (`specs/receipts/run-anywhere-corpus-20260914.md`
§11) found the defect this section fixes. `init` scaffolded a `planes.yaml`
whose example item touched `glob: "src/**"`, a fixed string. That resolves on
a repository with a top-level `src/` and nowhere else: 4 of the 10 corpus
repositories. On the other 6, among them a Java project whose sources sit at
`retrofit/src/main/java/`, `plan sync` correctly reported
`UNRESOLVED (no_glob_match)`.

The bug is not the resolver, which was right both times. It is that a new
user's very first `plan sync` showed an unresolved touch in a file the tool
had just written for them, which reads as a broken tool rather than as a
placeholder they were meant to replace.

The scaffold is now derived from the tree it is written into:

- `file:` the repository's root README (`README.md`, `README.rst`, `README`,
  first one that exists), otherwise `.codegraph/config.toml`, which `init`
  wrote moments earlier and therefore exists by construction. A `file:` touch
  binds on a path that exists whether or not it is indexed, so this resolves
  on a repository that has never been indexed.
- `glob:` over the top-level directory holding the most files in a language
  codegraph has a grammar for, omitted entirely when every source file sits
  at the root. Ties break alphabetically so the same tree always produces the
  same file.

Discovery reuses `index::discover_source_files` rather than walking the tree
again. That is what makes "a file codegraph can parse" mean the same thing to
the scaffold and to the indexer, and it applies the same ignore rules, so the
scaffolded glob can never name a path the index will refuse to hold.

**Measured, same binary, on a repository whose sources live under `lib/`.**
Repro: the commands below, run in a fresh git repository containing
`README.md`, `lib/keep.rs` and `lib/deep/more.rs`.

```
# the old scaffold's content, verbatim
$ codegraph plan sync --db-url mem:// --json
resolved=0 ambiguous=0 unresolved=1

$ codegraph init
$ grep -E "file:|glob:" .codegraph/planes.yaml
          - file: README.md
          - glob: "lib/**"
$ codegraph plan sync --db-url mem:// --json
resolved=3 ambiguous=0 unresolved=0
```

`mem://` is the whole store here. `plan sync` binds `file:` and `glob:`
touches against the working tree, so it needs no index and no store that
outlives the process, which is what makes this assertion cheap enough to be a
test.

Four tests, all failing before the change. Three unit tests in `src/init.rs`
(`starter_planes_file_is_valid`, now over both a rich tree and a bare one;
`the_scaffold_names_the_directory_the_code_is_actually_in`, which asserts the
busier of two candidate directories wins and that a source file at the root
does not vote; `a_tree_with_no_source_directory_still_scaffolds_a_resolvable_touch`)
and one end to end through the real binary,
`the_scaffolded_planes_file_resolves_on_the_repo_it_describes`, which runs
`init` then `plan sync` and asserts zero unresolved and zero ambiguous.

```
$ cargo build --release       # shared tree
0 errors, 0 warnings
$ cargo test --release
345 passed, 0 failed
$ cargo test --release --test run_anywhere
test result: ok. 17 passed; 0 failed
```
