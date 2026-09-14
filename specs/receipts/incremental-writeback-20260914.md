# Incremental write-back and query ordering (lane L7, 2026-09-14)

Base: branch `frontier`, commit family around `1a5c6e0`.
Toolchain: `rustc 1.97.1`, `cargo 1.97.1`. Host darwin 25.5.0, aarch64.
Isolated build throughout: `CARGO_TARGET_DIR=<scratch>/l7-perf-target`, so
every number below comes from one binary that differs only by the change
under test. Machine load is reported per run, because this box was under
heavy concurrent load all night and wall time alone is not trustworthy;
instructions retired is the load-independent number and is what the claims
rest on.

---

## 1. The defect

L9 root-caused it (`specs/receipts/index-profile-20260914.md` §8): bare
`codegraph index`, the invocation the README and `codegraph init` both
recommend, takes `resolve_incremental`, whose write-back was an `UPDATE ...
WHERE` per edge. `--force` takes `resolve_project`, which bulk-inserts.

Reproduced here before changing anything. cobra, 36 Go files, 4,458
name-edges:

```
$ uptime
 4:21  up 1 day, 17:50, load averages: 30.75 28.92 29.79
$ cd <scratch>/perf/cobra-before && codegraph init && /usr/bin/time -l codegraph index
[codegraph] done in 42.3s (discovery 0.0s, change detection 0.0s, parse+store 1.2s, resolve 40.6s, fingerprint 0.5s, registry 0.0s)
       43.16 real        24.06 user         0.91 sys
        195523006829  instructions retired
```

resolve 40.6s, 195.5B instructions. Matches L9's 195.2B.

## 2. The stated mechanism was wrong, and the first fix made it worse

L9 attributed the cost to SQL parsing: 250 concatenated `UPDATE` statements
per chunk means 250 parses and 250 query plans, against 1 for a bulk
`INSERT`. The prescribed fix was to iterate a bound array server-side so a
chunk becomes one statement.

That was implemented first, as a SurrealQL `FOR $u IN $updates { UPDATE ...
}`, one parsed statement per chunk. Measured:

```
$ uptime
 4:24  up 1 day, 17:53, load averages: 11.62 20.25 25.88
[codegraph] done in 32.2s (discovery 0.0s, change detection 0.0s, parse+store 0.8s, resolve 30.9s, fingerprint 0.4s, registry 0.0s)
       32.61 real        29.98 user         0.37 sys
        306228659917  instructions retired
```

**306.2B instructions, against 195.5B before: 1.57x worse.** Wall time fell
from 43.2s to 32.6s, but load also fell from 30.75 to 11.62, so the wall
figure is noise and the instruction count is the real result. Collapsing
4,458 parses into 18 made the work go up, which means parsing was never the
dominant cost.

The arithmetic that explains the real mechanism: a conditional `UPDATE ...
WHERE` has to *find* its rows, so N updates over an N-edge table is
quadratic. 4,458 x 4,458 is 19.9M row visits; at roughly 10k instructions
per visit that is about 200B, which matches the 195.5B measured. Under the
parsing story, 4,458 parses would each have to cost ~44M instructions,
which is not plausible for one short statement. The `FOR` loop kept the
per-row WHERE and added loop and field-access overhead on top, which is
exactly the direction it moved.

Recording this because the fix that follows is not the fix that was
specified, and the reason is a measurement that contradicted the brief.

## 3. The fix

`resolve_incremental` now writes back the way `--force` does: delete, then
bulk insert. Deletion is keyed on the **source node**, one statement with a
bound array, so there is no per-row WHERE anywhere in the path.

Deleting by `from_id` sweeps up edges that share a source node but were not
re-resolved this pass. Those siblings are reinserted from their stored
values verbatim, `resolution_gen` included, which is why selectivity still
means what it says: an edge nothing touched keeps its old generation, and
`touch_one_file_re_resolves_only_the_affected_edges` still asserts exactly
that. To make that possible, `load_all_edges` now projects the whole stored
binding rather than just the lookup key.

`write_file_refs` also went from one `CREATE` per cross-file pair (60 for
cobra, 451 for zstd) to a chunked bulk `INSERT`.

`write_updates` and `EdgeUpdate` are deleted; nothing calls them.

## 4. After

```
$ uptime
 4:42  up 1 day, 18:11, load averages: 12.54 17.23 18.75
$ cd <scratch>/perf/cobra-final && codegraph init && /usr/bin/time -l codegraph index
[codegraph] done in 2.2s (discovery 0.0s, change detection 0.0s, parse+store 0.8s, resolve 0.8s, fingerprint 0.5s, registry 0.0s)
        2.68 real         1.58 user         0.12 sys
         19244398211  instructions retired
```

Every row below is the `/usr/bin/time -l codegraph index` transcript quoted
in §1, §2 and §4 of this receipt; no number here is derived or estimated.

| | resolve | instructions retired | load at run |
|---|---|---|---|
| before | 40.6s | 195,523,006,829 | 30.75 |
| `FOR` loop (rejected) | 30.9s | 306,228,659,917 | 11.62 |
| after | 0.8s | 19,244,398,211 | 12.54 |

**10.2x fewer instructions, resolve 40.6s to 0.8s**, from the three
transcripts above. The after figure sits next to the `--force` path's own
17.1B instructions and 0.9s resolve, measured by L9 in
`index-profile-20260914.md` §8.1 with the command quoted there, which is the
expected landing place now that both paths do the same thing.

## 5. Same rows, not just faster

Field-for-field, on the same repo, both from a fresh store:

```
$ codegraph index --force          $ codegraph index
  Nodes:       652                   Nodes:       652
  Edges:       4588                  Edges:       4588
  Edges:       4458 considered       Edges:       4458 considered
  Resolved:    935 (21.0%)           Resolved:    935 (21.0%)
  Ambiguous:   137 (3.1%)            Ambiguous:   137 (3.1%)
  Unresolved:  3386 (76.0%)          Unresolved:  3386 (76.0%)
  File refs:   60                    File refs:   60
```

`incremental_write_back_matches_the_force_path_field_for_field` asserts the
same thing per row across four fixtures (rust, polyglot, go,
rename-refactor/after), comparing `to_id`, `confidence`, `resolved_by`,
`candidates`, `attempted_rules`, `resolution_outcome` and the derived
`file_ref` rows. `file_refs_are_written_once_with_the_expected_shape` checks
the bulk insert does not duplicate a pair.

---

## 6. Ordering sweep of `src/graph/`

Every `sort_by`/`sort_by_key` and every limit in the module, audited.

| Site | Was | Now |
|---|---|---|
| `coupling.rs` score sort + `truncate` | `sort_by_key(Reverse(ca+ce))`, no tie-break | total order, then truncate |
| `coupling.rs` `name_to_file` | **"first match wins" over storage order** | lowest file path wins |
| `call_chain.rs` roots | storage order | (file, qualified name, id) |
| `call_chain.rs` `entries` | `sort_by_key(depth)` | depth then caller/callee file and name |
| `search.rs` both queries | `LIMIT` with **no `ORDER BY`** | `ORDER BY file_path, name, node_id` before the limit |
| `explain.rs` chain key sorts | `(from_id, to_name)` | plus `to_type`, `edge_type` |
| `hub_nodes.rs`, `dependencies.rs` | fixed earlier tonight | unchanged |
| `circular.rs`, `clones.rs` | already total orders | unchanged, verified |

Two of these are worse than the reported symptom and are worth reading
twice:

**`coupling.rs`'s `name_to_file` was not an ordering bug, it was a
correctness one.** The comment said "first match wins on ambiguity", and the
first match meant whichever row storage happened to return first. For a name
defined in more than one file (`connect`, in both `api/app.py` and
`worker/main.go` in the polyglot fixture) that flipped which file received
the afferent counts, so the **metric changed between runs**, not merely the
row order. The extended test caught it by producing a top-3 that was missing
a file scoring 2 while including one scoring 1, which no tie-break could
explain. The documented approximation is unchanged; only the choice of
winner is now a property of the source (lowest file path) rather than of
storage.

**`search.rs` had `LIMIT` with no `ORDER BY`.** Which matches come back at
all depended on storage order. That is the same defect as the truncate
cases, in SQL rather than in Rust, and it is user-visible on `codegraph
query --kind search`.

`two_fresh_indexes_produce_byte_identical_query_output` now indexes the
polyglot fixture four times into four isolated stores and asserts a single
distinct serialization for `rdeps`, `hubs`, `coupling`, `circular`,
`call_chain` and `search`. Coupling is queried with `limit = 3`
deliberately: the defect is not only tie order but that a tie straddling the
cut changes which rows return at all, and a limit larger than the result set
could never show it.

## 7. Done

```
$ cargo test --release
passed=340 failed=0   (18 targets, 0 build errors)

$ cargo build --release
0 warnings
```

## 8. Follow-ups

- `hub_nodes` shares the `(name, node_type)` aggregation caveat that
  `coupling` documents: symbols sharing a name and type share an in-degree.
  That is deterministic and documented, so it was left alone, but it is the
  same approximation and worth revisiting together.
- `verify_chain` still loads the whole graph per call; see
  `explain-v1-20260914.md`.

---

## 9. Dead-code allows removed (follow-up commit)

Nine `#[allow(dead_code)]` annotations added earlier tonight are gone. They
existed for one reason: `main.rs` used to re-declare `mod graph;` and `mod
index;` instead of using the library crate, so the binary compiled a second
copy of the module tree in which anything reachable only through the library
API, `graph::explain`, or the test suite looked dead. Another lane fixed
that root cause (`736f3e4`, "use the library crate instead of re-declaring
the module tree"), which made every one of them redundant.

Removed:

- `src/graph/explain.rs`: the module-level `#![allow(dead_code)]` and the
  paragraph of module docs that justified it.
- `src/graph/dependencies.rs`: three on `ReversePath`,
  `reverse_dependency_paths`, `is_stale_candidate_for_explain`.
- `src/index/resolve.rs`: five on `AttemptOutcome::as_tag`, `NoTrace`,
  `candidate_pool`, `has_import_facts`, `resolve_all`, plus the explanatory
  note block on `resolve_one`.

The pre-existing allow on `GraphEdge` (`src/graph/mod.rs`) is not L7's and
was left alone.

Evidence that they were masking nothing:

```
$ cargo build --release 2>&1 | grep -cE '^warning|^error'
0

$ cargo test --release
passed=340 failed=0   (18 targets, 0 build errors)
```

A suppression that can be deleted without a single warning appearing is a
suppression that was describing a build quirk rather than hiding unused
code, which is what the comments claimed and is now checked rather than
asserted.
