# index-profile-20260914: where `codegraph index` spends its time, per phase and per language

Lane L9 (profile), measurement only. No `src/` file under `/Users/mike/dev/codegraph` was
modified. One binary was built from the committed tree (`ac272a5`) in an isolated git
worktree, with temporary phase timers added to that worktree's copy of `src/index/mod.rs`
only (diff reproduced in full below). The main checkout was never touched, never built.

## 5-line answer

On a clean, isolated, fresh-store measurement with today's committed code, the two real
costs are **store** (a DB round trip per file, ~15-30ms/file, scales with **file count**)
and **resolve** (the R1-R6 cascade, ~0.15-0.3ms per name-edge, scales with **edge count**).
Neither is per-candidate or per-file-size in any way that blew up: zstd's legacy/ directory
duplicates functions 7x but candidate pools stay ~4-8 wide, not huge. zstd is slow relative
to its file count because it has 6-8x more edges per file than the other three repos
(legacy/ inflates node and edge counts at constant file count), not because C parsing or a
specific phase is pathologically slow. **Store dominates at `--tier fast`** (resolve
collapses to near-zero with no name-edges to resolve); **store and resolve are roughly
tied at `--tier balanced`/`--tier full`**, which are measured *identical* for this C repo
(see §4). **The single biggest lever found tonight is not language or tier: every number
above used `--force`. Omitting `--force` (codegraph's own default, and what every other
measurement tonight actually ran) is 11-50x slower, confirmed by a controlled A/B with a
hardware instruction counter, root-caused to one function. See §8, added after this receipt
first shipped, at the team lead's request, to reconcile three conflicting measurements of
the identical cobra repo.** §0 below is the investigation as it stood before that request;
it is kept rather than rewritten because the ruling-out work in it still stands, it simply
had not yet tested the one variable that turned out to matter.

## 0. Context: these numbers are far below the number this task started from (see §8 for the resolution)

The task brief states `codegraph index` costs 0.17 s/file (Rust, 1573-file repository),
0.88 s/file (Go, cobra), 1.85 s/file (C, zstd, 515s total), all traceable to
`recon-product`'s earlier `smoke-*.log` / `scale-*.log` runs in this session's scratchpad,
using a binary recon-product built with `cargo build --release` (`build-cold.log`:
`` Finished `release` profile [optimized] target(s) in 10m 53s ``).

My runs, same repos, same `--tier full --force`, same `/usr/bin/time -l` wrapper, same
scratchpad, a different but also `--release` binary built from `ac272a5`:

| Repo | Task brief s/file | My measured s/file | Ratio |
|---|---|---|---|
| go-cobra (Go, 36 files) | 0.88 (recon `-solo` run) | 0.119 (4.29s/36) | 7.4x faster |
| c-zstd (C, 279 files, 515s total) | 1.85 | 0.0748 (20.86s/279, **24.7x less wall time**) | 24.7x faster |

Output is byte-identical every single time this session (cobra: 652 nodes / 4588 edges /
935 resolved / 137 ambiguous / 3386 unresolved / 610 fingerprints, in recon's runs, in
L1's environment implicitly, and in all of mine), so this is not a correctness
difference. To rule out "your run just got lucky on an idle machine," I checked the one
metric that is immune to scheduling: macOS `time -l`'s **instructions retired** (a
hardware counter: CPU cycles the process's own code actually executed, not wall time it
waited for a turn):

| Repo | recon-product's instructions retired | mine | Ratio |
|---|---|---|---|
| go-cobra | 181,223,204,590 (`smoke-go-cobra-solo.log`) | 16,357,262,766 | **11.1x more** |
| c-zstd | 2,438,952,927,328 (`smoke-c-zstd.log`) | 91,894,114,394 | **26.5x more** |

That gap cannot be contention (§5 measures contention directly and it tops out around
1.3-1.8x on this machine, not 11-26x) and it cannot be disk speed (instructions retired
is a CPU-execution counter, not an I/O counter). Something in recon-product's binary
genuinely executed 11-26x more instructions to produce the identical output. I spent real
effort trying to root-cause this and could **rule out**:
- Debug vs. release build: both builds' logs confirm `` `release` profile [optimized] ``.
- An explicit profile override: neither the current nor the `ac272a5` `Cargo.toml` has a
  `[profile.release]` section; no `.cargo/config.toml` at repo/`~/dev`/`~` level; no
  `RUSTFLAGS` in the shell env.
- A parent Cargo workspace silently changing settings: no `Cargo.toml` exists above
  `/Users/mike/dev/codegraph`, and my worktree at `/private/tmp/.../l9-wt` cannot see one
  even if it existed, which is itself a candidate explanation in reverse (see below).

At the time this section was first written, I had not yet tested whether recon-product's
run passed `--force`. It almost certainly did not: §8 below found and confirmed the actual
variable (the presence or absence of `--force`, an 11-50x effect on the identical binary,
identical repo, identical everything else), which fits recon-product's numbers as well as
it fits L1's. The build-vs-workspace-vs-RUSTFLAGS ruling-out above still stands as
evidence against those specific mechanisms; it was simply the wrong branch of the search.
**Read §8 first if you only read one more section.** The phase *ratios* in §3-§4 of this
receipt were unaffected either way, since every one of my own measured runs used `--force`
consistently throughout.

## 1. Method

Binary: `git -C /Users/mike/dev/codegraph worktree add <scratch>/l9-wt HEAD` (pinned at
`ac272a5`), `CARGO_TARGET_DIR=<scratch>/l9-target cargo build --release` (cold, 8m33s, exit
0). Temporary `tracing::info!` phase timers added to `<scratch>/l9-wt/src/index/mod.rs`
only; full diff in §7. The existing code had timing at exactly two points (`discovered
source files`, `incremental change detection complete`); everything from the parse/store
loop through resolve, fingerprint, and registry update ran as one unbroken, untimed span
before this session. `RUST_LOG` was left unset; default filter is `codegraph=info`
(`src/main.rs`), which is enough; no `--verbose` needed.

Every repo run: fresh empty store (`rm -rf` + `mkdir` before each), `--tier full --force`
unless noted, one at a time, never concurrent with another `codegraph index` invocation,
wrapped in `/usr/bin/time -l`, `uptime` captured immediately before and after.

Labeling note: every number in every table below is measured, by the command shown in §7,
against the log files it names. A number attributed to the task brief, to `recon-product`,
or to L1 is named as such inline at first use in its section and is never my own
measurement; §0 and §5 compare those against mine explicitly rather than blending them.

## 2. Load-average caveat (read before trusting any single absolute number)

System load climbed steadily through this session as other lanes' builds/tests ramped up:
1-minute load was 22.2 at the first cobra run, 34.5 by flask, 46-50 by the zstd/self runs.
The task brief's own threshold (1-min load > 8 ⇒ wall numbers contaminated) was blown past
before my second run. I do not have a quiet-machine baseline to fall back on tonight, but
I do have a **direct, same-repo, same-binary, same-fresh-store contention control**: I
reran go-cobra 4.5 minutes after the first run (load 22.2 → 24.6-49.8 over the session):

| | run 1 (load ~22) | run 2 (load ~25, 4.5 min later) | delta |
|---|---|---|---|
| store_ms | 855 | 984 | +15% |
| resolve_ms | 858 | 1509 | **+76%** |
| fingerprint_ms | 421 | 731 | **+74%** |
| wrapped real | 4.29s | 4.71s | +10% |
| instructions retired | 16,357,262,766 | 16,431,197,369 | +0.5% (noise floor) |

Instructions retired barely moved (+0.5%, measurement noise) while wall time on the two
CPU/allocation-heavy phases (resolve, fingerprint) moved 74-76%, a clean demonstration
that **contention inflates wall time on this machine by roughly 1.1-1.8x per phase, not by
orders of magnitude**, and that it hits resolve/fingerprint harder than the I/O-bound store
phase. Every absolute ms below should be read with a ±50-80% error bar from contention
alone; the *relative order* of phases (which one is biggest) is far more trustworthy than
any single number, and instructions-retired (last column) is the most trustworthy number
of all where I have it.

## 3. Per-repo phase table (full tier, `--force`, fresh store, one at a time)

All times in ms except where marked. "phase-sum" = discover+detect+parse+load_old+store+
resolve+fingerprint+registry; "wrapped real" is the `/usr/bin/time -l` figure around the
whole CLI invocation (includes process start, DB connect+signin, schema DDL, none of
which `index_project` itself owns).

| Repo | Lang | Files | Nodes | Edges | discover | detect | parse | load_old | store | resolve | fingerprint | registry | phase-sum | wrapped real | user | instr. retired |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| go-cobra (run 1) | Go | 36 | 652 | 4588 | 1 | 0 | 86 | 60 | 855 | 858 | 421 | 7 | 2288 | 4.29s | 1.33s | 16,357,262,766 |
| go-cobra (run 2, control) | Go | 36 | 652 | 4588 | 1 | 0 | 110 | 136 | 984 | 1509 | 731 | 15 | 3486 | 4.71s | 1.69s | 16,431,197,369 |
| py-flask | Python | 83 | 1455 | 3811 | 6 | 0 | 103 | 254 | 2252 | 766 | 134 | 14 | 3529 | 4.65s | 1.44s | 11,146,695,728 |
| ts-zod | TypeScript | 516 | 4872 | 4428 | 11 | 0 | 819 | 6106 | 8455 | 871 | 336 | 7 | 16605 | 17.14s | 7.74s | 73,243,176,976 |
| c-zstd | C | 279 | 6106 | 28094 | 12 | 0 | 986 | 3290 | 7827 | 7418 | 821 | 9 | 20363 | 20.86s | 10.97s | 91,894,114,394 |
| codegraph-self (pinned worktree) | Rust | 119 | 1053 | 4826 | 5 | 0 | 132 | 207 | 1970 | 1264 | 111 | 8 | 3697 | 4.32s | 1.49s | 12,688,399,422 |

`codegraph-self` indexed the **pinned `l9-wt` worktree** (119 files, `ac272a5`), not the
live `/Users/mike/dev/codegraph` checkout. That checkout was dirty with four other lanes'
concurrent edits (`git status` at session start: `M Cargo.toml`, `M src/cli.rs`, `M src/
index/resolve.rs`, `M src/lib.rs`, `M src/main.rs`) and indexing a moving target mid-edit
would not have been a valid measurement. Node/edge counts (1053/4826) differ negligibly
from recon's own `scale-codegraph-self.log` (1052/4813, indexed against the dirty tree),
confirming this substitution is sound.

Derived rates (phase-sum basis, the more meaningful denominator since wrapped-real carries
fixed per-invocation overhead; see §3.1):

| Repo | store ms/file | resolve ms/name-edge |
|---|---:|---:|
| go-cobra | 23.75 | 0.192 |
| py-flask | 27.13 | 0.218 |
| ts-zod | 16.39 | 0.213 |
| c-zstd | 28.05 | 0.264 |

Resolve cost per name-edge is remarkably consistent (0.19-0.26ms) across four unrelated
languages and repo shapes, strong evidence the cascade genuinely is O(edges), not blown
up by zstd's duplicate-symbol candidate pools. Store cost per file is noisier (16-28ms)
but the same order of magnitude everywhere; §2's contention caveat almost certainly
explains most of that spread rather than a real per-language difference.

### 3.1 The fixed per-invocation cost

`wrapped real` minus `phase-sum` is consistently ~0.6-2.0s across every repo regardless of
size (cobra run 1: 4.29s - 2.29s = 2.0s; flask: 4.65s - 3.53s = 1.1s; codegraph-self:
4.32s - 3.70s = 0.6s): process start (69MB binary, dylib linking), `db::connect`
(signin+use_ns+use_db), and `db::init_schema` (72 core plus 47 plan idempotent `DEFINE`
DDL statements; counted in `specs/receipts/store-cost-20260914.md` section 1) all
happen before `index_project` starts timing. This is a genuine fixed tax on every `index`
invocation, independent of repo size, and is invisible in a "s/file" framing since it
doesn't shrink per file; on a 4-file repo it would dominate entirely.

## 4. zstd deep dive

zstd is the outlier by file count normalization (§3's derived rates show it isn't a
per-phase outlier, cost per file/edge is in line with the other three repos). It is slow
in absolute terms because **it has far more edges for the same file count**: 279 files
carry 28,094 edges (100.7 edges/file) versus cobra's 127, flask's 45.9, zod's 8.6. The
`lib/legacy/` directory is 7 near-complete duplicate copies of the codec
(`zstd_v01.c`..`zstd_v07.c`, 2128-4490 lines each); functions like `ZSTD_isError`
(in-degree 463 per the original hub-nodes query), `MEM_readLE32` (in-degree 143-146,
appearing in 5 different files), and `FUZ_rand` (in-degree 146, in 2 files) are each
defined multiple times and called from every version's call sites. This inflates edge
count at constant file count; it does **not** blow up resolver candidate pools the way a
naive read might suggest, because `resolve.rs`'s `Indices.by_bare_key` buckets candidates
by `(bare_name, node_type, language_family)`; even `ZSTD_isError`'s pool tops out around
4-8 (one per legacy version + current), not hundreds. The mechanism is edge *count*, not
edge *fan-out*.

Which phase dominates depends entirely on tier (§4.1). At `full`/`balanced`, store
(7.2-7.8s) and resolve (7.4-8.2s) are within noise of each other, roughly tied for the
largest phase, together ~72-75% of phase-sum. `load_old` (3.3-4.2s, a per-file `SELECT ...
WHERE project_id=$pid AND file_path=$fp` round trip against the composite index
`idx_cn_project_file`) is a real third contributor I had not anticipated going in; see
§4.2.

### 4.1 Tier sweep (zstd only)

| Tier | Files | Nodes | Edges | parse | load_old | store | resolve | fingerprint | wrapped real | user | instr. retired |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| fast | 279 | 6106 | 15 | 857 | 4170 | 5542 | 93 | 562 | 11.76s | 5.22s | 41,134,444,513 |
| balanced | 279 | 6106 | 28094 | 988 | 3377 | 7191 | 8219 | 975 | 21.34s | 11.10s | 91,929,957,608 |
| full | 279 | 6106 | 28094 | 986 | 3290 | 7827 | 7418 | 821 | 20.86s | 10.97s | 91,894,114,394 |

**balanced and full are measured-identical** (edges, resolved/ambiguous/unresolved counts,
instructions retired all match within noise; wrapped real 21.34s vs 20.86s is inside this
machine's contention band from §2). This is not a coincidence. I checked the source
before measuring: every extractor (`c_cpp.rs`, `go.rs`, `python.rs`, `java.rs`,
`typescript.rs`) gates call/reference extraction behind a single `if ctx.tier !=
IndexingTier::Fast` check. Only `rust.rs` has an *additional* `Full`-only gate
(`"macro_invocation" if ctx.tier == IndexingTier::Full`, extracting macro-invocation
nodes). **For every language except Rust, `--tier balanced` buys nothing over `--tier
full` today**: they are the same extraction, so a user choosing "balanced" on a C, Go,
Python, TypeScript, or Java repo pays full's cost for fast's documented feature set. `fast`
is the only tier that actually saves anything on zstd, and what it saves is specifically
the resolve phase (93ms vs 7.4-8.2s), because tier=fast never emits name-edges to
resolve. It does **not** save store or load_old (both file/node-count driven, not
edge-extraction driven), which is why fast's wall time (11.76s) is 56% of full's, not some
much smaller fraction; the two largest phases are tier-independent.

### 4.2 `load_old`: a phase I did not anticipate, and a partial answer to the team lead's round-trip question

Mid-session, team lead (L1) reported measuring "resolver pass = 66.7s of 68.1s" for cobra
and asked whether the resolver does a per-edge or per-candidate store round trip. I
refuted that specific number directly (§5), but while checking it I read every DB call in
`resolve.rs` and found one genuine N-round-trip loop, just not in the cascade: `resolve_
project`'s tail call `write_file_refs` (resolve.rs:1454) issues **one individual `CREATE
code_edge` per cross-file `(from_file, to_file)` pair**, not a batched/chunked insert like
everything else in this file (`insert_resolved_edges` and `write_updates` both batch at
`WRITE_CHUNK_SIZE=250`). Round-trip counts this session: cobra 60, flask 42, zod 52, zstd
451 (`file_refs` in every `indexing complete` log line above). At the ~1.7-12ms/round-trip
range `load_old`'s per-file lookups showed in §3, 451 individual round trips is a genuine,
findable, *real* (if modest, probably well under 2s even at the high end) contributor
inside the resolve phase, and the cleanest actual "N round trips" finding I could confirm
in the code. It is not the mechanism L1 described (candidate-count-driven), and it does
not come close to explaining a 66.7s number on cobra, where it would be 60 round trips.

Separately, `resolve::load_file_symbols` (the `load_old` phase, called once per file
before every store) is also an unbatched per-file round trip by design (`resolve.rs:1330`,
`WHERE project_id = $pid AND file_path = $fp`, matching the composite index `idx_cn_
project_file` exactly), not a bug, since each file's own row can only be looked up
individually, but it is a second per-file round trip on top of store's own per-file bulk
insert, and zod's 516 files paid 6.1s for it (11.8ms/file, the highest of any repo
measured, plausibly load-contamination per §2 rather than a real zod-specific cost, since
it queries by exact `file_path` match and should not depend on how much of the *same*
file's data existed before, which on a `--force` fresh store is always none).

## 5. L1's claim, checked directly

**Update: confirmed and root-caused in §8.** L1's transcript (`specs/receipts/run-anywhere-
20260914.md` §11) never passes `--force`. Everything below was true and still is: the
*force* path (`resolve_project`, what every number elsewhere in this receipt measures) has
no per-edge round trip. L1's run took the *other* path (`resolve_incremental`, `codegraph
index`'s own default), which does effectively become O(edges) in SQL-statement count,
just not through the mechanism originally guessed. Left in place below as the record of
what was checked before that was found.

L1 (team lead) reported, from a binary built at commit `e4932ef`: cobra parse+store = 0.9s,
resolver pass = 66.7s of 68.1s total, and hypothesized a per-edge or per-candidate store
round trip. I could not reproduce this at any point in this session, using `--force`:

- **Code check**: `cascade`/`cascade_rules`/`terminal` (resolve.rs:484-732, the R1-R6 rule
  engine) are plain synchronous functions over in-memory `&[ResolverNode]` and a prebuilt
  `HashMap` (`Indices`, built once via `build_indices`), zero `.await` anywhere in that
  path. `resolve_project`'s only DB I/O is 3 reads (`load_nodes`, `load_unresolved_edges`,
  `next_resolution_gen`), 1 `DELETE`, ~18 chunked `INSERT`s for cobra's 4458 edges at
  `WRITE_CHUNK_SIZE=250`, and 60 individual `write_file_refs` round trips (§4.2), roughly
  82 round trips total, not O(edges).
- **Index check**: `schema.surql` defines `idx_cn_project`, `idx_cn_project_file`,
  `idx_ce_from`/`idx_ce_to`/`idx_ce_toname`; every one `project_id`-prefixed and matching
  the resolver's actual `WHERE` clauses exactly. An unindexed full-table scan across all
  projects ever stored is not the obvious mechanism (though I did not load-test this
  composite-index assumption under an accumulated multi-project store; flagged as
  unverified, not ruled out).
- **My own measurement**: cobra resolve_ms = 858ms and 1509ms across two runs (§3),
  against a wrapped-real total of 4.29-4.71s, not 68.1s.
- **Contention control** (§2): the same repo, same binary, rerun 4.5 minutes later under
  higher load, moved resolve_ms by +76%, real, but nowhere near a 78x gap.
- **Instructions retired** (§0): recon-product's independently-built release binary
  retired 11.1x more CPU instructions than mine for byte-identical cobra output, hard
  evidence that *some* binaries measured tonight did genuinely more work, unrelated to
  scheduling. I could not determine whether L1's `e4932ef` binary is in that same
  bucket, and did not have access to L1's raw invocation or environment to check directly.

I sent this partial finding to team-lead directly mid-session (msg_id
`b7dc00d2-be1a-4371-8f98-da739fec7c97`) rather than holding it for this receipt alone; the
recommendation in it (check the build and the load, re-measure with matching
instrumentation) was reasonable given what was known at the time, but the actual answer
turned out to be simpler and unrelated to build or load: see §8.

## 6. What this suggests (no code changes proposed, that is a separate decision)

The single highest-value fix found tonight is `write_updates` (§8): batch its SQL the same
way its sibling `insert_resolved_edges` already does, and the default (non-`--force`) path
stops being 11-50x slower than `--force` for no semantic reason. Store and resolve are
otherwise the two costs that matter on the `--force` path, and both are already close to
their structurally cheapest shape: store is one bulk INSERT per file (not per node; a
comment at `mod.rs`'s `store_parsed_file` records that this replaced a literal per-node
CREATE loop, "the dominant cost of a full index" in an earlier version, already fixed
once), and `resolve_project`'s cascade is O(edges) with small candidate pools even under
zstd's 7x symbol duplication. The one unbatched loop on the `--force` path that clearly
could be chunked like its siblings is `write_file_refs` (§4.2), cheap to fix, modest
expected payoff there specifically (well under the phase's own noise band at the file
counts measured tonight, though it would scale worse on a much larger monorepo with many
more cross-file pairs). The `balanced`/`full` tier collapse for every non-Rust language
(§4.1) is a documentation/product-surface question, not obviously a code bug. Either
balanced should genuinely extract less for C/Go/Python/TS/Java, or the tier's advertised
distinction should say so. Neither of these is why the task's original 0.17/0.88/1.85
s/file numbers looked the way they did; §8's `write_updates` finding is that reason, is the
far larger effect of anything in this receipt, and is now a confirmed, reproducible bug
report rather than an open question.

## 7. Commands run, verbatim, with output trimmed to the relevant lines

```
$ sysctl vm.swapusage
vm.swapusage: total = 2048.00M  used = 670.38M  free = 1377.62M  (encrypted)

$ uptime
 1:38  up 1 day, 15:07, 1 user, load averages: 4.71 7.41 9.43

$ which samply flamegraph cargo-flamegraph
samply not found / flamegraph not found / cargo-flamegraph not found  (not installed; not
used, per instructions not to install anything)

$ git -C /Users/mike/dev/codegraph worktree add <scratch>/l9-wt HEAD
Preparing worktree (detached HEAD ac272a5)
HEAD is now at ac272a5 docs: house-style sweep of the fixture-corpus README, numbers preserved

$ cd <scratch>/l9-wt && CARGO_TARGET_DIR=<scratch>/l9-target cargo build --release
   Compiling codegraph v0.1.0 (<scratch>/l9-wt)
    Finished `release` profile [optimized] target(s) in 8m 33s
BUILD_EXIT=0

$ <scratch>/l9-target/release/codegraph index <repo> --project-id <tag> --tier full --force \
    --db-url "surrealkv://<scratch>/l9-stores/<tag>/graph.db"
  (one invocation per repo/tier, wrapped in /usr/bin/time -l; full per-run output lives in
  <scratch>/l9-logs/{go-cobra,py-flask,ts-zod,c-zstd-full,c-zstd-balanced,c-zstd-fast,
  codegraph-self}.log; tables in §3/§4.1 are extracted from these, every ms field is the
  literal tracing field value, every "real/user/instructions retired" line is the literal
  /usr/bin/time -l output line)

$ grep -n "DEFINE INDEX" src/schema.surql
DEFINE INDEX OVERWRITE idx_cn_project ON code_node FIELDS project_id;
DEFINE INDEX OVERWRITE idx_cn_project_file ON code_node FIELDS project_id, file_path;
DEFINE INDEX OVERWRITE idx_cn_project_type ON code_node FIELDS project_id, node_type;
DEFINE INDEX OVERWRITE idx_cn_name ON code_node FIELDS name;
DEFINE INDEX OVERWRITE idx_cn_qualified_name ON code_node FIELDS project_id, qualified_name;
DEFINE INDEX OVERWRITE idx_ce_from ON code_edge FIELDS project_id, from_id;
DEFINE INDEX OVERWRITE idx_ce_to ON code_edge FIELDS project_id, to_id;
DEFINE INDEX OVERWRITE idx_ce_toname ON code_edge FIELDS project_id, to_name, to_type;
  (plus file_metadata/deleted_symbol/fingerprint/project_registry indexes, not relevant here)
```

### Instrumentation diff (worktree only, never applied to the main checkout)

```diff
--- a/src/index/mod.rs
+++ b/src/index/mod.rs
@@ use std::sync::Arc;
+use std::time::Instant;
@@ // 1. Discover source files
+    let t_discover = Instant::now();
     let source_files = discover_source_files(&root, config.languages.as_deref());
+    let discover_elapsed = t_discover.elapsed();
     result.files_scanned = source_files.len();
-    tracing::info!(files = source_files.len(), "discovered source files");
+    tracing::info!(files = source_files.len(), discover_ms = discover_elapsed.as_millis(), "L9 PROFILE: discovered source files");
@@ // 2. Determine which files need (re-)indexing
+    let t_detect = Instant::now();
     let changes = if config.force { ... } else { ... };
+    let detect_elapsed = t_detect.elapsed();
@@ tracing::info!( to_index ..., unchanged ..., deleted ...,
+        detect_ms = detect_elapsed.as_millis(),
         "L9 PROFILE: incremental change detection complete" );
@@ // 4. Parse and store each file.
+    let mut phase_parse = std::time::Duration::ZERO;
+    let mut phase_load_old = std::time::Duration::ZERO;
+    let mut phase_store = std::time::Duration::ZERO;
     for path in &to_index {
-        match parser::parse_file(...) {
+        let t_parse = Instant::now();
+        let parse_outcome = parser::parse_file(...);
+        phase_parse += t_parse.elapsed();
+        match parse_outcome {
             Ok(parsed) => {
+                let t_load_old = Instant::now();
                 let old_symbols = resolve::load_file_symbols(...).await?;
+                phase_load_old += t_load_old.elapsed();
                 ...
-                match store_parsed_file(...).await {
+                let t_store = Instant::now();
+                let store_outcome = store_parsed_file(...).await;
+                phase_store += t_store.elapsed();
+                match store_outcome {
                     Ok((nodes, edges)) => { ... }
@@ end of loop
+    tracing::info!(parse_ms = ..., load_old_ms = ..., store_ms = ..., "L9 PROFILE: parse+store loop breakdown");
@@ resolver pass
+    let t_resolve = Instant::now();
     result.resolution = if config.force { ... } else { ... };
+    tracing::info!(resolve_ms = t_resolve.elapsed().as_millis(), "L9 PROFILE: resolver pass");
@@ fingerprint pass
+    let t_fingerprint = Instant::now();
     result.fingerprints = fingerprint::update_fingerprints(...).await...;
+    tracing::info!(fingerprint_ms = ..., "L9 PROFILE: fingerprint pass");
@@ registry update
+    let t_registry = Instant::now();
     update_project_registry(db, config, &result).await?;
+    tracing::info!(registry_ms = t_registry.elapsed().as_millis(), "L9 PROFILE: registry update");
```

(`git -C <scratch>/l9-wt diff --stat`: `src/index/mod.rs | 40 ++++++++++++++++++++++++++++++++++------`,
34 insertions, 6 deletions; full diff available in the worktree before its removal.)

Worktree removed after the last measurement: `git -C /Users/mike/dev/codegraph worktree
remove <scratch>/l9-wt`.

## 8. The `--force` finding: reconciling recon-product, L1, and my own numbers

Team lead asked for a controlled A/B on cobra to reconcile three measurements of the
identical repo: recon-product 31.7s wall, L1 68.1s wall (resolver 66.7s), mine 2.3-4.7s
wall, all nominally the same commit family, at least two of the three confirmed `--release`.
Method: one variable at a time, same binary unless the binary itself is the variable, one
run at a time, `uptime` reported per run. Binary used throughout this section: the shared
`/Volumes/SSD/cargo-target/release/codegraph`, built today from a commit at or after
`e4932ef` (confirmed via `--help`: it has `init`/`doctor`/`plan`/`landscape`, none of which
exist in the `ac272a5` binary the rest of this receipt uses).

### 8.1 Variable 1: binary/commit, flags and store held constant

```
$ uptime
 3:36  up 1 day, 17:05, 1 user, load averages: 21.13 21.27 21.56
$ /Volumes/SSD/cargo-target/release/codegraph index <scratch>/repos/go-cobra \
    --project-id go-cobra-sharedbin-explicit --tier full --force \
    --db-url "surrealkv://<scratch>/ab-test/sharedbin-isolated/graph.db"
[codegraph] done in 2.7s (discovery 0.0s, change detection 0.0s, parse+store 1.1s, resolve 0.9s, fingerprint 0.6s, registry 0.0s)
        3.34 real         1.65 user         0.22 sys
         17089136083  instructions retired
```

A much newer binary, same explicit-flags-and-isolated-store style I used everywhere else in
this receipt: resolve 0.9s, 17.1B instructions retired, matching my `ac272a5` numbers (§3:
858-1509ms, 16.4B instructions) almost exactly. **Binary/commit is ruled out.**

### 8.2 Reproducing L1's exact sequence with that same binary

```
$ uptime
 3:37  up 1 day, 17:06, 1 user, load averages: 20.46 20.95 21.41
$ cd <fresh cobra clone> && /Volumes/SSD/cargo-target/release/codegraph init
$ /Volumes/SSD/cargo-target/release/codegraph doctor
$ /Volumes/SSD/cargo-target/release/codegraph index
[codegraph] done in 58.6s (discovery 0.0s, change detection 0.0s, parse+store 1.3s, resolve 56.4s, fingerprint 0.9s, registry 0.0s)
       59.28 real        25.93 user         1.17 sys
        196299906490  instructions retired
```

L1's exact command sequence (`init`, `doctor`, bare `index`, no flags at all), on the same
binary that was just fast in §8.1: resolve 56.4s, 196.3B instructions retired. This matches
L1's transcript almost exactly (66.7s there, 56.4s here, both far above 0.9s) and matches
recon-product's `instructions retired` for the same repo (181.2B, §0) far more closely than
my own numbers did. Two things changed at once here versus §8.1 (no flags/config-file
defaults, and default store path inside the repo instead of an isolated scratch path), so
this narrows the search but does not yet isolate a single variable.

### 8.3 Isolating store-reuse (the pub-prep-verify-20260730 "store-open" hypothesis)

Team lead flagged `specs/receipts/pub-prep-verify-20260730.md` (store-open cost regressed
12-17x, root cause never diagnosed) as a candidate explanation. Checked directly: that
receipt's repro commands (lines 133, 202, 341, 426) all pass `--force`, so whatever
mechanism this section finds cannot be the same code path as that one; they are separate,
both real, both additive. That receipt's actual measurement was the fixed `connect` +
`schema-init` cost (150.5ms + 394.0ms on a small fixture) before any indexing logic runs,
which is the same thing this receipt's own §3.1 independently measured (0.6-2.0s fixed tax
per invocation, on this machine, tonight) and did not previously connect to that regression.
**§3.1's fixed cost and the July "store-open" regression are plausibly the same,
still-unfixed issue; neither one explains L1's number, which is entirely inside the
`resolve` phase, after connect and schema-init have already finished (see the `[codegraph]
resolve starting at 1.0s` line in every transcript, mine included).**

To test store-reuse directly, not just cite the July receipt: pre-touch an isolated,
non-nested store with a cheap connect (a `--tier fast --force` run), then run the real
`--tier full --force` measurement against that same now-touched store:

```
$ uptime
 3:39  up 1 day, 17:08, 1 user, load averages: 19.00 21.14 21.48
$ codegraph index <scratch>/repos/go-cobra --project-id pretouch-probe --tier fast --force \
    --db-url "surrealkv://<scratch>/ab-test/store-pretouch/graph.db"
        4.96 real         0.53 user         0.15 sys        (the pre-touch itself)
$ codegraph index <scratch>/repos/go-cobra --project-id go-cobra-touched --tier full --force \
    --db-url "surrealkv://<scratch>/ab-test/store-pretouch/graph.db"        (same store)
[codegraph] done in 3.4s (... resolve 1.6s ...)
        4.01 real         1.88 user         0.18 sys
         18336066340  instructions retired
```

Still fast (resolve 1.6s, 18.3B instructions, matching §8.1). **A store that was opened
once before, and a store nested inside the walked repo directory versus a sibling
directory, are both ruled out.**

### 8.4 Isolating `--force`: the answer

One variable left from §8.2's two: config-file-driven flags versus `--force` itself. Same
binary, same explicit-flags style and same isolated (non-nested, never-touched-before)
store as the fast §8.1 run, changing only one thing, dropping `--force`:

```
$ uptime
 3:40  up 1 day, 17:09, 1 user, load averages: 17.24 20.56 21.27
$ /Volumes/SSD/cargo-target/release/codegraph index <scratch>/repos/go-cobra \
    --project-id go-cobra-noforce --tier full \
    --db-url "surrealkv://<scratch>/ab-test/store-noforce/graph.db"
[codegraph] done in 48.5s (discovery 0.0s, change detection 0.0s, parse+store 3.6s, resolve 44.4s, fingerprint 0.4s, registry 0.0s)
       55.50 real        24.02 user         0.89 sys
        195232025946  instructions retired
```

resolve 44.4s, 195.2B instructions retired: matches §8.2's 196.3B (L1's exact reproduction)
to within 0.6%, on a store that was never touched before and lives outside the repo
directory. **This is the whole effect.** `--force` versus its absence, alone, holding
everything else constant, is an **11.4x instructions-retired swing** (17.1B to 195.2B) and
a **~49x wall-time swing on the resolve phase specifically** (0.9s to 44.4s). Both
`8.1`-vs-`8.4` and the `resolve_ms` figures clear the >5x bar asked for; instructions
retired is the more trustworthy of the two, per this receipt's own §2 contention findings.

Both commands, side by side, are the two blocks above; the only difference between them is
the `--force` flag.

### 8.5 Root cause in the code

`--force` selects `resolve::resolve_project` (§3-§5 of this receipt: in-memory cascade,
zero per-edge `.await`, ~18 chunked `INSERT`s total for cobra). Its absence selects
`resolve::resolve_incremental` (`resolve.rs:1067`), which on a first-ever index still has
to consider every edge (`changed_files` is every file, so `affected` ends up the same 4458
edges), runs the identical cascade in memory, then writes back through `write_updates`
(`resolve.rs:1391`, already quoted in full in this receipt's instrumentation-adjacent
reading) instead of `insert_resolved_edges`. The difference is entirely in that write-back:

- `insert_resolved_edges` (the `--force` path): one `INSERT INTO code_edge $rows` per
  250-row chunk, `rows` bound as a single array parameter. For cobra: about 18 SQL
  statements parsed and planned, total, for the whole resolver pass.
- `write_updates` (the non-`--force` path): for each 250-row chunk, it builds **one SQL
  string containing 250 separate `UPDATE code_edge SET ... WHERE project_id = $pid AND
  from_id = $fromN AND to_name = $tnameN AND to_type = $ttypeN AND edge_type = $etypeN ...;`
  statements**, each with its own 10 bound parameters, concatenated with `\n`. For cobra:
  about 4458 individually-parsed, individually-planned SQL statements, not 18. The chunking
  only batches how many round trips happen (still ~18); it does not batch how much SQL
  parsing and query planning the engine does inside each round trip.

That is a CPU-bound, per-statement cost, which is exactly what the instructions-retired
evidence shows (not I/O, not contention: §2 already established this machine's contention
band tops out around 1.1-1.8x, and §0 already ruled out debug builds, profile overrides,
and workspace settings as causes of an instruction-count gap this large). It also explains
why `parse+store` was unaffected in every test above (that phase does not touch
`write_updates` at all) and why `fingerprint` moved only slightly (`update_fingerprints`
also chunks its writes as a single bound array per chunk, like the `--force` path, not like
`write_updates`).

This also almost certainly explains recon-product's original 31.7s (§0): I did not have
their raw command, but the magnitude (31.7s wall, 19.7s user, 181.2B instructions retired
for cobra) sits between this section's fast (§8.1: 17.1B) and slow (§8.4: 195.2B) numbers
in exactly the direction a `--force`-less or differently-contended run would, and is far
closer to the slow end. `zstd`'s recon-product number (2439B instructions, §0) likely
reflects the same mechanism at 28,094 edges instead of 4,458, which would multiply the
per-statement parsing cost proportionally further; I did not re-run zstd without `--force`
to confirm this (the `--force` run alone already takes 20.86s at tonight's load, and a
non-`--force` run at roughly the same multiplier observed here would run several minutes),
so this specific extrapolation is **not measured, only projected from the confirmed cobra
mechanism**, and should be labeled that way if quoted further.

#### Correction, 2026-09-14, later the same night

The mechanism sentence above (SQL parsing and planning, 4,458 individually-parsed
statements versus 18) is **wrong**, refuted by a direct test, not by argument. L7 built
exactly the fix that sentence implies (a server-side `FOR $u IN $updates` loop over one
bound array, collapsing the parse count to 18 same as `insert_resolved_edges`) and
measured it on cobra: instructions retired went **up**, 195.5B to 306.2B, 1.57x worse, not
better. Parsing was never the cost.

The mechanism that fits the number: each `UPDATE code_edge SET ... WHERE project_id = $pid
AND from_id = $fromN AND to_name = $tnameN AND to_type = $ttypeN AND edge_type = $etypeN`
has to *find* its row before it can write it, and nothing in `write_updates`'s WHERE
clause is a covering index lookup on its own (`idx_ce_from` covers `project_id, from_id`,
but `to_name`/`to_type`/`edge_type` still need a residual scan among that `from_id`'s
edges). N such conditional updates against an N-edge table is quadratic in the worst case:
4,458 edges implies roughly 4,458² ≈ 19.9M row visits, and at an estimated ~10k
instructions per visit that lands within a few percent of the measured 195.5B. This was
not independently re-derived here; it is L7's diagnosis, reported because it is the one
that survived a direct test and mine did not.

The fix that landed (`3a517b2`, `specs/receipts/incremental-writeback-20260914.md`) is not
"batch the UPDATE statements" but "stop using conditional UPDATE at all": delete the
affected rows by their known source-node keys, then bulk-insert the resolved rows with one
bound array per chunk, the same shape `--force`'s `insert_resolved_edges` already used.
Measured there: cobra's `resolve_incremental` went from 40.6s / 195.5B instructions to
0.8s / 19.2B, output field-identical to the `--force` path.

What stands, unaffected by this correction: the A/B in §8.1-§8.4 (the `--force` variable
itself, and the localization to `write_updates` specifically, both confirmed by a direct
swap of exactly that one variable) is what made L7's fix possible to find, and remains
correct. Only the explanation of *why* `write_updates` was slow was wrong. Left in place
above rather than edited, per the request that prompted this correction.

### 8.6 What this means for the product

`codegraph index` **with no flags, the exact invocation the README and `codegraph init`'s
own "Next: codegraph index" prompt both recommend, and the one every first-time user will
actually type, is not on the fast path measured everywhere else in this receipt.** The
first index of a repository (and every subsequent one, since only a `--force` run ever
takes the bulk-insert path; every ordinary incremental re-index goes through
`resolve_incremental` by construction) pays the `write_updates` cost, not the
`insert_resolved_edges` cost. On cobra that is a 44-56s resolve phase instead of well under
a second. On a repository the size of zstd, or larger, the same mechanism should be
expected to dominate total wall time by a wide margin. This is the actionable fix `specs/
receipts/run-anywhere-20260914.md`'s §11b pointed here for, and it is a small, mechanical,
low-risk change: give `write_updates` the same one-bound-array-per-chunk shape
`insert_resolved_edges` and `update_fingerprints` already use, rather than building
per-row SQL text. That is a fix proposal, not a fix; per this lane's scope, no code was
changed.

## 9. ac272a5 vs HEAD, same repo, same flags: did tonight's commits regress the resolver

Team lead's follow-up, prompted by an L5 measurement of gin (Go, 99 files) at ~73.5M
instructions retired per edge versus this receipt's isolated ac272a5 cobra at ~3.6M per
edge, roughly 20x. Two candidate explanations: a real regression in tonight's seven
commits (`9d719ea` through `7e8827e`), or invalid per-edge normalization across two
structurally different repos. One test separates them: run both isolated binaries on the
identical repo.

Binaries: `ac272a5` (this receipt's original `l9-target`, rebuilt for nothing, reused
as-is) and `HEAD` at `7e8827e` (`fix(explain): unambiguous chain wire format, published
schemas, stable order`), built the same way as every other binary in this receipt
(`git worktree add <scratch>/l9-head-wt HEAD`, `CARGO_TARGET_DIR=<scratch>/l9-head-target
cargo build --release`, cold, 17m38s, exit 0). `7e8827e` predates `3a517b2` (confirmed:
`git merge-base --is-ancestor 3a517b2 7e8827e` fails), so it does not include §8.5's
correction-section fix; it is `--force`-vs-bare exactly as `--force` and bare behaved for
every other measurement in this receipt.

```
$ uptime
 4:48  up 1 day, 18:17, 1 user, load averages: 7.11 10.57 14.89
$ <l9-target>/release/codegraph index <scratch>/repos/go-cobra --project-id cmp-ac272a5-force \
    --tier full --force --db-url "surrealkv://<scratch>/ab-test2/ac272a5-force/graph.db"
        4.58 real         1.76 user         0.18 sys
         16516367494  instructions retired

$ uptime
 4:48  up 1 day, 18:17, 1 user, load averages: 7.66 10.63 14.89
$ <l9-head-target>/release/codegraph index <scratch>/repos/go-cobra --project-id cmp-head-force \
    --tier full --force --db-url "surrealkv://<scratch>/ab-test2/head-force/graph.db"
[codegraph] done in 4.0s (discovery 0.0s, change detection 0.0s, parse+store 2.0s, resolve 1.4s, fingerprint 0.7s, registry 0.0s)
        6.45 real         1.87 user         0.19 sys
         17046052420  instructions retired
```

ac272a5: 16,516,367,494 instructions retired. HEAD (`7e8827e`): 17,046,052,420. **Ratio
1.032, a 3.2% difference**, inside this receipt's own established noise floor (§2's
same-binary contention control moved instructions retired by 0.5%; two separate ac272a5
runs earlier in this receipt, §3 and this section, span 16.36B-16.52B on their own, about
1% spread). **Tonight's seven commits did not regress `resolve_project`'s `--force` path
on cobra.** L5's ~20x per-edge gap is not a binary regression: `src/canon.rs` and `src/
index/fingerprint.rs` both show zero diff between `ac272a5` and `HEAD` (`git diff --stat`),
so the fingerprint/canon pass is byte-identical code in both binaries, and its cost is
neighborhood-symmetry-dependent (individualization-refinement search scales with how much
color-refinement alone can distinguish a neighborhood, not linearly with edge count), so
per-edge normalization across two structurally different repos (cobra's legacy-free,
comparatively flat call graph versus gin's, unmeasured by this lane) is invalid on its
face. This receipt does not have gin's own phase split to confirm which specific phase
carries its cost; that is L5's data to report, not reproduced here.

Baseline for the fix (bare `codegraph index`, `HEAD` at `7e8827e`, pre-`3a517b2`):

```
$ uptime
 4:49  up 1 day, 18:18, 1 user, load averages: 12.68 11.50 15.03
$ <l9-head-target>/release/codegraph index <scratch>/repos/go-cobra --project-id cmp-head-noforce \
    --tier full --db-url "surrealkv://<scratch>/ab-test2/head-noforce/graph.db"
[codegraph] done in 59.2s (discovery 0.0s, change detection 0.1s, parse+store 1.6s, resolve 56.7s, fingerprint 0.8s, registry 0.0s)
       60.02 real        26.97 user         1.21 sys
        195782312483  instructions retired
```

resolve 56.7s, 195.8B instructions retired: consistent with every other pre-fix
measurement of this same code path in this receipt (§8.2: 196.3B, §8.4: 195.2B), confirms
the bug is present unchanged from `ac272a5` through `7e8827e` (nothing in the intervening
seven commits touched `write_updates`), and is the exact number `3a517b2`'s own receipt
measures its fix against (195.5B before, 19.2B after, per team lead).

Worktree removed after this section's measurements:
`git -C /Users/mike/dev/codegraph worktree remove --force <scratch>/l9-head-wt`.
