# MCP surface receipt

Date: 2026-09-14. Base commit `acd6170`.
Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo 1.97.1 (c980f4866 2026-06-30)`.
Host: darwin 25.5.0, aarch64.

Every number below cites the command that produced it. The working tree was
shared with lanes P0, L1, L2 and L3 running concurrently; wherever that
affects a measurement it is said so in the line that reports it.

Scope of this receipt is part one of L4: evidence chains on
`codegraph_impact`, the `codegraph_verify_chain` tool, the
`codegraph_architecture.file_filter` fix, and the `codegraph_clones` tool.
The planes tools are part two and are not started.

---

## 0. Baseline, before any change

```
$ cargo build --release 2>&1 | grep -c '^warning'
0

$ cargo test --release 2>&1 | grep -E '^test result' | awk '{p+=$4; f+=$6} END {...}'
passed=304 failed=0
```

304 is the whole suite at the moment this lane started, not this lane's
denominator: other lanes wrote to the same tree throughout. This lane's own
contribution is isolated in §6 by running its test binary alone.

---

## 1. What the MCP surface was missing

Four tools were registered. Read off the wire from the pre-change binary,
over real MCP stdio JSON-RPC:

```
$ codegraph serve --project-id mcp-polyglot --db-url surrealkv://<scratch>/graph.db
  -> {"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
['codegraph_architecture', 'codegraph_impact', 'codegraph_quality', 'codegraph_search']

  -> {"method":"tools/call","params":{"name":"codegraph_clones","arguments":{}}}
{'code': -32602, 'message': 'tool not found'}

  -> {"method":"tools/call","params":{"name":"codegraph_verify_chain","arguments":{"chain":{}}}}
{'code': -32602, 'message': 'tool not found'}
```

So an agent could not reach structural clones at all, and could not reach
the evidence chains lane L7 had just landed. Both are things the CLI could
already do. Six tools are registered now.

---

## 2. D3: `file_filter` was accepted and discarded, measured

`src/mcp/server.rs:228` at `acd6170` read `let _ = p.file_filter; // TODO:
file filtering`. The parameter's own schema doc promised "focus on a
specific file path (substring match)".

Measured against the pre-change binary over MCP stdio, on
`tests/fixtures/polyglot`, four different `file_filter` values including one
that cannot match anything:

```
$ python3 <capture over codegraph serve, tests/fixtures/polyglot, --tier full>
distinct responses across 4 different file_filter values on HEAD: 1
  arch_plain           sha256 4792942cc62dcd27...
  arch_filter_api      sha256 4792942cc62dcd27...   file_filter "api/"
  arch_filter_glob     sha256 4792942cc62dcd27...   file_filter "client/**"
  arch_filter_nomatch  sha256 4792942cc62dcd27...   file_filter "**/does_not_exist/**"
```

One distinct response for four distinct filters, including one matching
nothing. The parameter did nothing.

### 2.1 Fails before, passes after

The three D3 tests were run against a tree whose only difference from the
final one is that `architecture` was reverted to HEAD's two lines
(`let _ = p.file_filter;` then the unfiltered report).

**Before:**

```
$ cargo test --release --test mcp_tools architecture_file_filter

thread 'architecture_file_filter_actually_narrows_the_report' panicked at tests/mcp_tools.rs:535:5:
assertion `left != right` failed: file_filter changed nothing, which is the defect this test exists for
  left: "## Project Structure\n\n**Node types:**\n- function: 10\n- import: 5\n\n**Languages:**\n- typescript: 8\n- go: 4\n- python: 3\n..."
 right: "## Project Structure\n\n**Node types:**\n- function: 10\n- import: 5\n\n**Languages:**\n- typescript: 8\n- go: 4\n- python: 3\n..."

thread 'architecture_file_filter_accepts_a_glob' panicked at tests/mcp_tools.rs:583:5:
a * pattern must be matched as a glob: <the whole unfiltered project report>

failures:
    architecture_file_filter_accepts_a_glob
    architecture_file_filter_actually_narrows_the_report
    architecture_file_filter_that_matches_nothing_says_so

test result: FAILED. 0 passed; 3 failed; 0 ignored; 0 measured; 17 filtered out
```

`left` and `right` are the same string. That is the defect, printed.

**After:**

```
$ cargo test --release --test mcp_tools
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
```

### 2.2 What the filter now means

Two matching modes, because both readings of the parameter are in use. A
value containing `*` or `?` is a glob (`*` stops at a path separator, `**`
crosses it, `?` is one non-separator character, and `a/**/b` also matches
`a/b`); any other value stays a substring, which is what the schema
documented before. Eight unit tests in `src/mcp/server.rs` pin the
semantics, including the cases that separate the two star forms.

Under a filter, a node counts when its own file matches and an edge counts
when the file holding its source matches. That asymmetry is deliberate and
is stated in the response itself, because "what this subtree does" and "what
is done to this subtree" are different questions and a reader cannot tell
which they got from the numbers alone.

Hubs are ranked over the whole project and narrowed afterwards, not narrowed
and then ranked: a function called from everywhere is a hub of the project
even when the filter only shows the file it lives in. The limit is applied
last, so a filtered call still returns up to `limit` rows.

A filter matching nothing says so in words and prints no counts, rather than
printing an empty report that reads like an empty project.

---

## 3. D1: explain on `codegraph_impact`, and byte-identity when it is off

`explain: Option<bool>` and `explain_limit: Option<usize>` were added.
`explain` off or absent appends nothing.

Byte-identity is structural before it is tested: the pre-explain body is its
own function (`CodegraphServer::impact_report`), called unchanged, and the
explain section is appended to its return value. There is no branch inside
the report body to get wrong.

That the bodies really were moved rather than rewritten was checked by
brace-matching each function out of both revisions and comparing the text,
not by reading the diff:

```
$ python3 <extract each fn body from `git show HEAD:src/mcp/server.rs` and from the worktree>
search: body identical to HEAD = True
quality: body identical to HEAD = True
impact_report body identical to HEAD's impact body (minus the params unwrap): True

architecture_report vs HEAD architecture body:
    @@ -1,6 +1,2 @@
     {
    -        let p = params.0;
    -        let hub_limit = p.limit.unwrap_or(15);
    -        let _ = p.file_filter; // TODO: file filtering
    -
             let mut out = String::new();
```

The only lines that left `architecture`'s body are the parameter unwrap and
the discarded filter. `search` and `quality` were not touched at all; their
descriptions changed and their code did not.

It is also tested against bytes captured from the **pre-change binary over
real MCP stdio**, not against a re-run of the current code:

```
$ codegraph index tests/fixtures/rename-refactor/after --tier full --force \
    --project-id mcp-rename_after --db-url surrealkv://<scratch>/graph.db
$ codegraph serve --project-id mcp-rename_after --db-url surrealkv://<scratch>/graph.db
  -> {"method":"tools/call","params":{"name":"codegraph_impact","arguments":{"name":"helper"}}}

"Impact analysis for 'helper': 0 dependent(s) (depth 3):\n\n\n1 unresolved reference(s) still named 'helper' (no live symbol currently has this name; check for an incomplete rename):\n  - use_stale (src/stale_caller.rs) → [UNRESOLVED] 'target::helper' (function)\n"
```

That string is frozen into
`impact_with_explain_off_is_byte_identical_to_the_pre_change_tree`, which
asserts it three ways: `explain` absent, `explain: false`, and the
early-return branch for a name with no dependents.

### 3.1 A caveat that had to be measured rather than assumed

The byte-for-byte case uses a fixture with **one** root symbol. A bare name
matching several live symbols is reported one group per symbol, and that
group order is not stable across re-indexes:

```
$ python3 <probe: 6 calls against one store, then 4 fresh indexes of the same tree>
within one process/store, 6 calls, distinct outputs: 1
  first group line: ['-- api::app::connect (api/app.py) --']
across 4 fresh indexes, distinct outputs: 2
   first group: ['-- api::app::connect (api/app.py) --']
   first group: ['-- worker::connect (worker/main.go) --']
```

Deterministic within a store, not deterministic across re-indexes of the
same source tree. The instability is in the index, not in this tool, and it
predates this lane. It is reported here rather than worked around silently:
it means a `codegraph_impact` answer on an ambiguous bare name is not
reproducible across re-indexes, which matters for anything that diffs two
runs. The multi-group test therefore compares the line multiset against the
pre-change capture and says so in its own comment.

### 3.2 The chains themselves

Chains come from `ExplainGraph::load` plus `explain::explain_symbol` with
`Membership::ExplicitSymbol`, which is what `facade::explain_chains_for_symbol`
does, with the same arguments.

They are not routed through that facade function, for a reason worth
recording: `src/main.rs` declares `mod mcp;` and `mod graph;` but no
`mod facade;`, so the binary target compiles a second copy of
`src/mcp/server.rs` in which `crate::facade` does not resolve, and
`src/main.rs` is another lane's file tonight. Measured rather than assumed,
on the first attempt to write the call:

```
error[E0433]: cannot find `facade` in `crate`
   --> src/mcp/server.rs:588:26
    |
588 |             match crate::facade::explain_chains_for_symbol(&self.db, ...
    |                          ^^^^^^ could not find `facade` in the crate root
```

The drift this could cause is closed by a test rather than by a comment:
`mcp_chains_match_the_facade_exactly` asserts the tool's chains equal
`facade::explain_chains_for_symbol`'s chains exactly, so a later change to
the facade fails a test instead of silently reaching only the CLI.

**Follow-up, gated on another lane.** The root fix is for `src/main.rs` to
stop re-declaring the module tree and use the library crate, which also
stops compiling every one of those modules twice. The same defect broke the
shared build later in the night from `src/context.rs`, which reaches for
`crate::landscape` and `crate::plan`; it was reported to the orchestrator
and routed to the lane that owns `main.rs`. Once that lands, the body of
`explain_section` becomes a single `facade::explain_chains_for_symbol` call
and this workaround comes out. It was checked again before this receipt was
finalized and had not landed yet:

```
$ grep -n '^mod ' src/main.rs
1:mod canon;  2:mod cli;  3:mod context;  4:mod db;
5:mod graph;  6:mod index;  7:mod mcp;     8:mod stats;

$ grep -c 'mod facade' src/main.rs
0
```

`mcp_chains_match_the_facade_exactly` should be kept after the switch rather
than deleted: it stops being a drift guard and becomes a round-trip check
that the chains an agent parses out of the response JSON block are equal to
the ones the library produced.

`explain_limit` (default 10) exists because a hub symbol produces one
dependent chain per resolved path and an MCP response lands directly in an
agent's context window. `explain_symbol` emits stale references first, then
ambiguous refusals, then dependents, so a prefix cut keeps the findings that
gate a change. A truncated response states the total it truncated from.

---

## 4. D2: `codegraph_verify_chain`, tampered across the JSON boundary

Input is the chain object exactly as `codegraph_impact` emits it under
`explain=true`, in the fenced JSON block of the response. A JSON string
holding that object is also accepted, because a client that builds arguments
from a template routinely stringifies a nested object. Anything that is not
a real `Chain` is refused with a message, never defaulted into an empty
chain that would then verify vacuously.

The tamper crosses the JSON boundary in both directions: the chain is
serialized, mutated **as JSON**, decoded back through the same
`serde_json::from_value` call rmcp's extractor makes, and verified.

| Tamper (applied to JSON) | Result | Caught by |
|---|---|---|
| `node_id` swapped for a non-existent id | rejected | the `node_exists` step, named by index |
| `rule` changed to `r99`, never reached | rejected | the `rule_application` step, named by index |
| `candidate_pool.node_ids` emptied | rejected | the `candidate_pool` step, named by index |
| not a chain at all | refused before verification | argument decode |
| chain delivered as a JSON string | accepted, verifies clean | (control) |
| untouched chain | accepted, verifies clean | (control) |

Every failing case asserts **both** that the chain was rejected and which
step rejected it. The two controls are what stop the table from being
satisfied by a verifier that rejects everything.

### 4.1 A near-miss worth recording

The first version of the tamper tests passed while tampering with nothing.
`Fact` is serde-tagged internally under the key `fact`, so a step serializes
as `{"index": n, "fact": {"fact": "node_exists", "node_id": ...}}`; writing
`step["node_id"]` sets an unknown key one level too high, which serde
ignores on the way back in. The chain then verified clean and one assertion
caught it:

```
thread 'verify_chain_rejects_a_forged_rule_and_a_forged_candidate_pool' panicked at tests/mcp_tools.rs:458:9:
an emptied pool was accepted:
ambiguous_refusal: helper
verify: PASS (10 step(s) re-derived against the live index)
```

The tests now go through one `tamper()` helper that **panics when the step
it was asked to mutate does not exist**, and through `chain_with_step()`,
which panics when no chain in the fixture carries that step kind. A tamper
test that quietly finds nothing to tamper with is worse than no test, since
it reads as evidence.

---

## 4b. The whole loop, end to end over a real transport

Not a test harness. A `codegraph serve` process, JSON-RPC over stdio, on
`tests/fixtures/rename-refactor/after`, which is the incomplete-rename
fixture:

```
-> tools/call codegraph_impact {"name":"helper","explain":true}

Impact analysis for 'helper': 0 dependent(s) (depth 3):

1 unresolved reference(s) still named 'helper' (no live symbol currently has this name; check for an incomplete rename):
  - use_stale (src/stale_caller.rs) -> [UNRESOLVED] 'target::helper' (function)

## Evidence chains (1)

stale_reference: helper
  1. the symbol "helper" was queried by name, literally
  2. no live definition of "helper" exists in this project
  3. stale_caller::use_stale (function) is defined at src/stale_caller.rs:9
  4. f52e4b9f07ae853a076c9e391c4ce878 references "target::helper" (function), and the resolver left it UNRESOLVED
  5. the candidate pool for ("helper", function, rust) holds 0 node(s)
  6. r1 ran and admitted nothing
  7. r2 ran and admitted nothing
  8. r2m did not run: its precondition did not hold
  9. r6 ran and admitted nothing
  10. the cascade tried r1, r2, r2m, r6 and ended: no_candidates
  ...
```

followed by the JSON block. Pulling chain 1 straight out of that block and
sending it back:

```
-> tools/call codegraph_verify_chain {"chain": <chain 1, unmodified>}
stale_reference: helper
verify: PASS (10 step(s) re-derived against the live index)
```

Then the same chain with one node id swapped, as JSON:

```
   mutating step 3: node_id f52e4b9f07ae853a076c9e391c4ce878 -> deadbeef...
-> tools/call codegraph_verify_chain {"chain": <tampered>}
stale_reference: helper
verify: FAIL (10 step(s) checked, 1 failed)
  step 3 (node_exists): no node with id "deadbeefdeadbeefdeadbeefdeadbeef"
```

An agent can now get a finding, get its derivation, and check that
derivation itself, without a single step of that loop leaving MCP.

The same session confirms D3 and D4 on the wire. Four filter values that
produced one identical response before now produce four distinct ones:

```
  arch_plain           sha256 40a0f0aaa5ba6ccd...
  arch_filter_api      sha256 aff442117ce4babd...
  arch_filter_glob     sha256 870df7842cd71f39...
  arch_filter_nomatch  sha256 76f746f6447a5562...
distinct: 4        (was 1, see §2)

-> tools/call codegraph_architecture {"file_filter":"api/"}
Scope: file_filter "api/" (substring match), 2 of 7 indexed file(s), 3 node(s).
...
**Languages:**
- python: 3
## Hub Nodes (top 15 within "api/")
- connect (function) ... api/app.py
- run (function) ... api/consumer.py

-> tools/call codegraph_architecture {"file_filter":"**/does_not_exist/**"}
Scope: file_filter "**/does_not_exist/**" (glob match), 0 of 7 indexed file(s), 0 node(s).
Nothing matched, so there is nothing to report at this filter. Indexed paths look like: api/app.py, api/consumer.py, client/src/even.ts.

-> tools/call codegraph_clones {}
No structural clones among 10 fingerprinted symbol(s) at min_edges=3. Lowering min_edges widens the search, at the cost of grouping shapes that are trivially alike.

-> tools/call codegraph_verify_chain {"chain":{}}
Error: the chain argument is not a codegraph evidence chain: missing field `explain_version`. Pass one object from the JSON block codegraph_impact emits under explain=true, unchanged.
```

The polyglot fixture is python under `api/`, typescript under `client/` and
go under `worker/`, so "2 of 7 files, python only, hubs only in api/" is the
right answer rather than merely a different one.

---

## 5. D4: `codegraph_clones`

Mirrors the CLI subcommand: `graph::clones::find_clone_groups`, `min_edges`
defaulting to 3, groups largest first. `clones_tool_reports_what_the_library_groups`
asserts the tool's output carries every group and every member the library
returns for the same project and threshold, so the tool cannot drift from
the CLI.

The empty result distinguishes two cases that would otherwise read alike,
at the cost of one extra query on the empty path only:

- no fingerprint rows at all, so nothing was compared;
- fingerprinted symbols exist, but none group at this threshold, which names
  the threshold and the population it searched.

An agent told "no clones" when the truth is "nothing was compared" would
draw exactly the wrong conclusion.

---

## 6. Definition of done

```
$ cargo build --release 2>&1 | grep -c '^warning'
0     (before, §0)

$ cargo test --release | awk over '^test result'
TOTAL passed=340 failed=0
```

At the commit, measured on the tree as committed:

```
$ cargo build --release 2>&1 | grep -c '^warning'
0

$ cargo test --release | awk over '^test result'
TOTAL passed=427 failed=0

$ cargo test --release --test mcp_tools
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.29s
```

**Warnings: 0 at the commit, and 0 attributable to this lane at any point.**
The tree was not warning-free at every moment overnight, but every warning
seen was in another lane's in-flight file, never `src/mcp/server.rs`,
`tests/mcp_tools.rs` or `tests/common/mod.rs`. Attributed mechanically
rather than by eye:

```
$ grep -A2 '^warning' <suite log> | grep -oE '^\s+--> [^:]+' | sort | uniq -c
  29 src/plan/ops.rs        (lane L2, mid-edit; 0 at the commit)

$ grep -A2 '^warning' <suite log> | grep -E 'mcp/server.rs|mcp_tools.rs|tests/common'
NONE
```

Earlier the same check named `src/init.rs:39` (L1) and `src/config.rs:234`
(P0/L1). None of them this lane's.

**Tests: 427 passed, 0 failed.** That is the whole shared tree and most of
the growth is other lanes: the suite was 304 when this lane started and 340
partway through. This lane's own contribution is 20 integration tests plus 8
unit tests, and the unit tests compile into both the lib and the bin target,
so 36 tests, of which the 20 integration tests are isolated by the
`--test mcp_tools` line above. No number in this receipt is claimed from the
whole-tree total.

**The tree was red twice in between, for other lanes' reasons**, which is
recorded rather than papered over, since it is why some measurements here
carry timestamps. One example:

```
test plan::export::tests::mermaid_draws_only_live_items_of_active_planes ... FAILED
test plan::ops::tests::lint_counts_a_clean_file_without_complaining ... FAILED
test plan::ops::tests::lint_reads_the_file_and_reports_every_rule_it_breaks ... FAILED
test result: FAILED. 175 passed; 3 failed
```

All three are `src/plan/` unit tests belonging to lanes L2 and L3 while they
were editing that module; all three pass at the commit.
`cargo test --release --test mcp_tools` was green on every run, including
that one.

**Concurrency cost.** Other lanes' mid-edit files broke the shared build
four separate times, each with errors in files this lane does not own:
`src/index/mod.rs` (L1), `src/init.rs` plus `src/config.rs` (L1/P0),
`src/landscape.rs` plus `src/doctor.rs` (L3/L1), and `src/context.rs` (L3).
Roughly 50 minutes of wall time waiting, no lost work. The `src/context.rs`
break was reported to the orchestrator because it shares a root cause with
the workaround in §3.2 and will keep recurring: `src/main.rs` re-declares
its own module tree instead of using the library crate, so any file in that
list that reaches for a lib-only module through `crate::` fails to compile
in the bin.

---

## 7. Not done, and one thing left deliberately

**The planes tools are not started.** They are part two of this lane and
wait on lane L2's ops API, per the brief. Nothing was stubbed for them.

**The em dashes in tool output text were left alone.** House style forbids
them, and every tool description written here is clean (asserted by
`every_tool_is_registered_and_described_for_an_agent`, which fails on a
U+2014 or on an en dash used as punctuation). But five output format strings
carry em dashes from before this lane: `src/mcp/server.rs` lines 345, 386,
527, 547 and 746. Two of them, 527 and 547, are inside `impact_report`, and
rewriting them would break exactly the byte-identity D1 requires and this
receipt demonstrates. The new filtered-hub line (828) was written to match
line 746 rather than diverge, so the same tool does not print two different
formats depending on a flag. Cleaning all six is a small, separate change
that should be made deliberately and announced, not folded into this one.

**A live in-process rmcp client was not used, and the tests say so.**
`ToolRouter::call` needs a `ToolCallContext`, which needs a
`RequestContext`, which needs a `Peer`, whose constructor is `pub(crate)`;
rmcp's `client` feature is not enabled and `Cargo.toml` belongs to another
lane tonight. What the tests do instead is exact rather than approximate:
every call asserts the tool is registered under its wire name in the real
`ToolRouter`, then decodes a JSON arguments object through
`serde_json::from_value`, which is the same call rmcp's extractor makes
(`rmcp-1.5.0/src/handler/server/tool.rs:181`), and then invokes the handler.
The frozen byte-identity literals came off a real transport: a `codegraph
serve` process speaking JSON-RPC over stdio.

---

## 8. Latency

`scripts/bench/mcp_latency.py` runs clean and in well under five minutes.
Both runs used a snapshot of the binary under a private `CARGO_TARGET_DIR`,
so a concurrent lane rebuilding `target/release/codegraph` mid-run could not
swap the binary under the measurement.

### 8.0 A first after-run was discarded, and why

The first after-measurement was thrown away rather than reported.

```
$ CARGO_TARGET_DIR=<snapshot> python3 scripts/bench/mcp_latency.py --requests 60
=== target: self-src (src/) ===
    codegraph_architecture n=60   mean= 149.48ms  p50= 120.84ms  p95= 280.53ms
    codegraph_impact       n=60   mean=  63.14ms  p50=  54.12ms  p95= 112.64ms
```

Set beside the pre-change figures in the block further down, that reads as
roughly a doubling on two paths this lane did not change. It was an
artifact:

- a leftover background task ran a **second copy of the same benchmark**
  concurrently, writing into the same log (visible as interleaved output at
  the tail, a second `n=12` run appearing after the first had printed its
  cleanup line);
- a cold `cargo build --release` of this lane's own fallback tree was still
  running;
- another lane was building in its worktree at the same time.

`uptime` at that moment reported `load averages: 20.11 15.90 10.69` on a
machine whose quiet baseline is under 2. Nothing about that number was
measuring this change. The correct reading of a suspicious result is to look
for the confound first, and there were three.

The numbers below are a paired run: the pre-change binary and the
post-change binary benchmarked **back to back in one window**, after waiting
for the one-minute load average to fall below 4, with the load recorded on
either side of each phase. Comparing two runs taken at different machine
loads is not a comparison, which is exactly what the first attempt did.

**Before** (`acd6170`), from the paired run:

```
$ CARGO_TARGET_DIR=<snapshot> python3 scripts/bench/mcp_latency.py --requests 60

=== target: polyglot (tests/fixtures/polyglot) ===
  [serve-mode MCP: one long-lived process, store opened once, amortized]
    codegraph_search       n=60   mean=   0.33ms  p50=   0.31ms  p95=   0.39ms
    codegraph_impact       n=60   mean=   0.61ms  p50=   0.59ms  p95=   0.76ms
    codegraph_architecture n=60   mean=   1.50ms  p50=   1.46ms  p95=   1.77ms
  [CLI-reopen: one fresh process per request, store reopened every time]
    query search           n=15   mean= 392.33ms  p50= 390.33ms  p95= 433.08ms
    query rdeps(impact)    n=15   mean= 390.35ms  p50= 393.83ms  p95= 419.54ms

=== target: self-src (src/) ===
  [serve-mode MCP: one long-lived process, store opened once, amortized]
    codegraph_search       n=60   mean=   3.68ms  p50=   3.69ms  p95=   4.06ms
    codegraph_impact       n=60   mean=  30.73ms  p50=  30.80ms  p95=  31.65ms
    codegraph_architecture n=60   mean=  58.41ms  p50=  58.26ms  p95=  60.02ms
  [CLI-reopen: one fresh process per request, store reopened every time]
    query search           n=15   mean=1060.21ms  p50= 995.21ms  p95=1369.31ms
    query rdeps(impact)    n=15   mean=1114.47ms  p50=1119.56ms  p95=1396.70ms
```

The serve-mode p50 bar the earlier receipts set is met on both targets by
the measurements printed above, from the `mcp_latency.py` command quoted
with them: the slowest p50 in that run is `codegraph_architecture` on
`self-src`. The bench exercises the three pre-existing tools with their
default arguments, so it measures precisely the paths this lane had to leave
unchanged: `codegraph_impact` with no `explain`, and
`codegraph_architecture` with no `file_filter`. It does not measure the new
paths, and no claim is made about them here.

### 8.1 After: the comparison does not resolve on this machine tonight, and here is the proof

**No before-versus-after latency delta is reported, because this machine
cannot currently resolve one.** That is a measurement conclusion, not a
missing measurement, and it is backed by running the benchmark against the
**unchanged pre-change binary** a second time under the load the other lanes
were generating.

The two binaries were alternated, before then after, twice, in one window,
each phase recording the one-minute load average on either side. Running the
**same** binary twice is the control: if it disagrees with itself by as much
as the two binaries disagree, there is no delta to read.

```
$ for round in 1 2; do for phase in before after; do
      CARGO_TARGET_DIR=<snapshot-$phase> python3 scripts/bench/mcp_latency.py --requests 60
  done; done

serve-mode p50, milliseconds:

target     tool                      before r1   after r1  before r2   after r2   before spread
polyglot   codegraph_search               0.34       0.46       0.91       0.29      2.68x
polyglot   codegraph_impact               0.71       0.73       1.64       0.56      2.31x
polyglot   codegraph_architecture         2.53       3.61       4.14       1.37      1.64x
self-src   codegraph_search               8.57       7.11       4.73       5.56      1.81x
self-src   codegraph_impact              89.97      53.48      43.01      51.01      2.09x
self-src   codegraph_architecture       176.75     110.10      83.68      90.37      2.11x

  before round 1: load1 start 17.23 end 23.86
  after  round 1: load1 start 23.86 end 24.97
  before round 2: load1 start 24.97 end 10.80
  after  round 2: load1 start 10.80 end  5.67
```

The last column is the control, and it is the whole answer. **The unchanged
pre-change binary disagrees with itself by 1.64x to 2.68x between its own
two rounds**, on every tool and both targets. Every before-versus-after
difference in the table is smaller than that, and its sign is not even
consistent: `codegraph_architecture` on `self-src` reads "after is 1.6x
faster" in round 1 and "after is 1.08x slower" in round 2.

Widening to the quiet run makes it starker. `codegraph_architecture` on
`self-src`, one binary, no code change, three machine states:

```
  p50  58.26 ms   quiet window
  p50 176.75 ms   round 1, load1 17 to 24
  p50  83.68 ms   round 2, load1 25 to 11
```

A 3.03x spread on identical code. That also settles §8.0 outright: the
pre-change binary under load is **slower** than the figure that discarded
run attributed to the post-change binary. A regression the unmodified code
reproduces more severely than the modified code is not a regression.

Two conclusions, kept separate because they carry different weight:

1. **Measured, and reportable.** In a quiet window the serve-mode p50s are
   the "Before" block above, inside the bar the earlier receipts set, on
   both the polyglot fixture and this repo's own `src/`.
2. **Not measured, and deliberately not guessed.** Whether this lane's
   changes move serve-mode latency. The expectation from the code is that
   they cannot on these three calls: `impact` with `explain` off and
   `architecture` with no `file_filter` run bodies proved textually
   identical to HEAD's in §3, and the new work sits behind branches those
   calls never take. But "the diff says it cannot have changed" is an
   argument, not a measurement, and it is labelled as one here. The clean
   number needs a quiet machine, which is a ten-minute re-run of
   `scripts/bench/mcp_latency.py` once the other lanes are done.

---

# Part two: the roadmap over MCP

Date: 2026-09-14, appended after part one shipped as `7e496c5`. Built on
lane L2's `884cc02` plus its follow-up, lane L7's `7e8827e`, and lane L1's
`main.rs` fix.

## 9. Seven planes tools, and why not eight

| Tool | Parameters | `plan::ops` call |
|---|---|---|
| `codegraph_plan_touching` | `target` | `touching` |
| `codegraph_plan_list` | `plane?`, `status?`, `horizon?` | `list` |
| `codegraph_plan_show` | `item_id` | `show` |
| `codegraph_plan_collisions` | none | `collisions` |
| `codegraph_plan_stale` | none | `stale` |
| `codegraph_plan_blast` | `item_id`, `depth?` (3) | `blast` |
| `codegraph_plan_sync` | `path?` | `sync` |

Thirteen tools are registered now, read off the wire:

```
-> {"method":"tools/list"}
codegraph_architecture, codegraph_clones, codegraph_impact,
codegraph_plan_blast, codegraph_plan_collisions, codegraph_plan_list,
codegraph_plan_show, codegraph_plan_stale, codegraph_plan_sync,
codegraph_plan_touching, codegraph_quality, codegraph_search,
codegraph_verify_chain
```

Every tool calls the `plan::ops` function the matching CLI subcommand calls
and serializes the report it returns, whole and unedited. Nothing about the
roadmap is computed in `src/mcp/server.rs`. That is not only tidiness: L2's
follow-up added `indexed`, `unindexed`, `working_tree_files` and `discovery`
to those reports while this lane was being written, and all four appear in
the dogfood output below without a line changing here.

**`lint` is not exposed, deliberately.** It takes a path and reads the file
without touching the graph, so over MCP it would answer a question about the
caller's filesystem rather than about the project the server is scoped to.
`sync` already validates the same file with the same rules and refuses on
any violation, so the capability is reachable; what is not reachable is
linting an arbitrary path through a server an agent does not control, and
that is the right thing to leave out.

**`plan_sync` takes an optional `path`, which the brief did not ask for.**
It is not optional-by-preference. `ops::sync(db, project_id, planes_path)`
needs a path, `CodegraphServer` holds only a connection and a project id,
and `serve_stdio`'s signature cannot grow a root because `src/main.rs`
calls it. The default is derived the way the CLI derives it, from
`config::repo_root` of the working directory, and the resolved path is
echoed in the report's `source` so a reader never has to guess which file
was read.

## 10. Dogfood: this repository's own roadmap, over real MCP stdio

```
$ codegraph index . --project-id codegraph --tier full --force
  Files:       140 scanned, 140 indexed
  Nodes:       1981      Edges: 9090

$ codegraph serve --project-id codegraph --db-url surrealkv://<scratch>/graph.db

-> tools/call codegraph_plan_sync {}
Ingested codegraph from /Users/mike/dev/codegraph/.codegraph/planes.yaml:
6 plane(s), 28 item(s), 303 touch(es) (303 resolved, 0 ambiguous, 0 unresolved)
against 140 indexed file(s).
{ "selectors": 61, "selectors_resolved": 61, "indexed_symbols": 1981,
  "working_tree_files": 320, "discovery": "git ls-files, so .gitignore is respected", ... }

-> tools/call codegraph_plan_touching {"target": "src/canon.rs"}
4 work item(s) already cover 'src/canon.rs'. Read matched_by before you treat
one as a claim on this code.

  CI-1   [planned ] bound  via file: src/canon.rs   <title omitted from the public copy>
  CI-2   [done    ] bound  via file: src/canon.rs   <title omitted from the public copy>
  CI-5   [planned ] bound  via file: src/canon.rs   <title omitted from the public copy>
  RH-1   [planned ] bound  via file: src/canon.rs   <title omitted from the public copy>

-> tools/call codegraph_plan_collisions {}
No collisions among 5 active item(s).
{ "project_id": "codegraph", "collisions": [], "active_items": 5 }

-> tools/call codegraph_plan_stale {}
No item points at missing code.
```

This is the board's §1 collaboration story working: an agent about to edit
`src/canon.rs`, the file the board freezes, asks one question and learns that
four work items already claim it, including one that says the file is frozen
pending Mike's decision. All four are `bound`, so this is real coverage
rather than a name collision.

The two empty answers are the interesting ones, because both state their
denominator. "No collisions among 5 active item(s)" cannot be misread as
"collision detection is not working", and neither can an empty stale report
whose `by_reason` tally is `[]` on a roadmap where every one of 303 touches
bound.

## 11. Tests

Nine new tests in `tests/mcp_tools.rs`, on lane L2's own `ops-rust.yaml`
against `tests/fixtures/rust`. That fixture was reused rather than rewritten
on purpose: its verdicts are facts recorded in the fixture's own
`expected.yaml`, so these tests and the resolver's tests are pinned to one
ground truth rather than two.

```
$ cargo test --release --test mcp_tools
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

One per tool, each asserting the substantive field: `touching` finds OPS-1,
OPS-2 and OPS-5 by file and OPS-1 by symbol with `matched_by` reporting the
ambiguous touch as a candidate rather than as coverage; `list` filters to
active and refuses `status: "in-progress"` with the accepted list; `show`
returns OPS-1's one-of-every-confidence touch set and its `blocks` inverse;
`collisions` finds the OPS-1/OPS-2 pair and excludes planned OPS-5 and
non-overlapping OPS-3; `stale` names OPS-1 and keeps `actionable` separate;
`blast` at depth 1 returns exactly the two callers the fixture documents;
`sync` refuses `dependency-cycle.yaml` and a re-`list` is byte-equal to the
pre-refusal listing.

### 11.1 The nine passed on the first run, so they were mutation-checked

Nine new tests going green first time is exactly when a vacuous test hides,
so two mutations were applied to `src/mcp/server.rs` and reverted:

```
# plan_touching ignores its target argument
test plan_touching_finds_planned_work_by_file_and_by_symbol ... FAILED
test plan_touching_separates_nothing_planned_from_nothing_named ... FAILED
test result: FAILED. 7 passed; 2 failed

# plan_blast ignores its depth argument
test plan_blast_returns_the_documented_callers ... FAILED
test result: FAILED. 0 passed; 1 failed
```

Each mutation failed exactly the tests that should have caught it and no
others.

## 12. The three follow-ups from part one, all closed

**The `kind` tag.** L7's `7e8827e` renamed `Fact`'s serde discriminator from
`fact` to `kind`, which is the fix for the foot-gun part one's receipt
recorded. The two tamper helpers were adapted, and `tamper()` gained an
assertion that the tamper actually changed the JSON, so overwriting a field
with the value it already held now fails the test instead of reading as
"tampering was not detected".

**The typed `chain` parameter.** `codegraph_verify_chain`'s `chain` is no
longer `serde_json::Value`, whose published schema was `true` and told an
agent nothing. It is now `ChainArg`, an untagged enum of the real `Chain`
and a string, so the schema is `anyOf [the chain shape, a string]`. The
string arm stays because a client that builds arguments from a template
routinely stringifies a nested object.

This moved where junk is refused, which is an improvement worth naming: an
object that is not a chain is now rejected by rmcp's own extractor before
the handler runs, so there is no code path on which an empty chain could be
constructed and then verified. `verify_chain_refuses_something_that_is_not_a_chain`
was rewritten to assert both halves: the object is refused at the boundary,
and a string that is not a chain is refused one layer in with guidance.

**The facade switch.** `explain_section` now calls
`facade::explain_chains_for_symbol`, the same function the CLI's `--explain`
path calls. The workaround came out once `src/main.rs` stopped re-declaring
the module tree (`grep -c '^mod ' src/main.rs` is 0).
`mcp_chains_match_the_facade_exactly` was kept rather than deleted: it has
stopped being a drift guard and become the round-trip check that the chains
an agent parses out of the response JSON block equal the ones the library
produced.

### 12.1 The chain JSON, re-captured rather than hand-edited

Part one's §4b quoted a live transcript that showed the old `"fact"` tag.
Re-captured from a `codegraph serve` process on the current build:

```
-> tools/call codegraph_impact {"name":"helper","explain":true}
...
    "steps": [
      { "index": 1, "fact": { "kind": "explicit_symbol_query", "name": "helper" } },
      { "index": 2, "fact": { "kind": "live_definitions", "name": "helper", "node_ids": [] } },
      { "index": 3, "fact": { "kind": "node_exists",
                              "node_id": "f52e4b9f07ae853a076c9e391c4ce878",
                              "name": "use_stale", "node_type": "function",
                              "language": "rust", "file_path": "src/stale_caller.rs",
                              "qualified_name": "stale_caller::use_stale", "start_line": 9 } },
      ...

-> tools/call codegraph_verify_chain {"chain": <chain 1, unmodified>}
stale_reference: helper
verify: PASS (10 step(s) re-derived against the live index)

   mutating step 3: node_id f52e4b9f07ae853a076c9e391c4ce878 -> deadbeef...
-> tools/call codegraph_verify_chain {"chain": <tampered>}
stale_reference: helper
verify: FAIL (10 step(s) checked, 1 failed)
  step 3 (node_exists): no node with id "deadbeefdeadbeefdeadbeefdeadbeef"
```

The doubled key is gone: a step is now `{"index": n, "fact": {"kind": ...}}`.
The capture script written for part one broke on the rename with
`KeyError: 'fact'`, which is the cleanest possible demonstration that the
wire format really did change and that nothing was quietly reading both.

## 13. Definition of done, part two

```
$ cargo build --release 2>&1 | grep -c '^warning'
0

$ cargo test --release | awk over '^test result'
TOTAL passed=337 failed=0

$ cargo test --release --test mcp_tools
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

**336 is not a drop from part one's 427, and the arithmetic matters.** L1's
`main.rs` fix removed the duplicated module tree, so the bin target stopped
compiling and running a second copy of the library's unit tests. Measured
rather than assumed:

```
$ cargo test --release --bins
     Running unittests src/main.rs
test result: ok. 0 passed; 0 failed        (was 109 before the fix)

$ cargo test --release --lib
test result: ok. 182 passed; 0 failed

$ grep -c '^mod ' src/main.rs
0
```

427 minus those 109 duplicates is 318, and the suite is 337, so 19 tests
were added across all lanes in the window, 9 of them this lane's. No test
was lost.

**Blocked time.** Part two could not start when it was briefed. Every planes
tool needs `crate::plan::ops`, and until L1's fix landed the bin crate had
no `plan` module, which was probed directly rather than inferred:
`error[E0433]: cannot find plan in crate --> src/mcp/server.rs`. Unlike part
one's facade problem there was no equivalent call to fall back to, since
`src/plan/ops.rs` was not compiled into the bin in any form, and the only
alternative would have been reimplementing L2's module. That was reported
rather than worked around, and work started the minute the fix landed.

---

# Part two addenda: house style on the wire, and a tie-ordering audit

Appended after `344beea`. HEAD at the start of this work was `344beea` on
top of L7's `1a5c6e0`, L3's `660cdc2` and L1's `736f3e4`.

## 14. The em dashes came out of the output strings

Part one's §7 recorded five pre-existing em dashes in output format strings
and left them, because two sit inside `impact_report` and rewriting them
would have broken exactly the byte-identity D1 required and that receipt
demonstrated. With the fixtures re-captured in the same change, that
objection goes away. Six occurrences are gone, the sixth being the filtered
hub line this lane added to match the unfiltered one:

```
$ grep -c "$(printf '\xe2\x80\x94')" src/mcp/server.rs
0
```

The command spells the character as its UTF-8 bytes so that this receipt
does not contain the thing it is reporting the absence of, and so the
command can be pasted without a copy step mangling it.

| Was | Is |
|---|---|
| `- {} — Ca:{} Ce:{} I:{:.2} ({} nodes)` | `- {} (Ca:{} Ce:{} I:{:.2}, {} nodes)` |
| `- {} ({}) — degree:{} in:{} out:{} — {}` | `- {} ({}): degree:{} in:{} out:{}, in {}` |
| `'{}' matches {} distinct symbols — shown separately:` | `'{}' matches {} distinct symbols, shown separately:` |
| `- [AMBIGUOUS] '{}' ({}) — candidates: {}` | `- [AMBIGUOUS] '{}' ({}), candidates: {}` |
| `- {} ({}) — in:{} out:{} total:{} — {}` (twice) | `- {} ({}): in:{} out:{} total:{}, in {}` |

Confirmed on the wire rather than in the source, by capturing every tool
response the fixtures exercise from a live `codegraph serve`:

```
- connect (function): in:2 out:1 total:3, in api/app.py
  - [AMBIGUOUS] 'helper' (function), candidates: alpha::helper, beta::helper
em dashes anywhere in captured output: 0
```

### 14.1 The frozen fixtures were re-captured, not hand-edited

One frozen literal contained a cleaned string, the multi-symbol header. It
was re-captured from a `codegraph serve` process over stdio with the same
script that captured the originals, so the test still asserts against a
transcript nobody typed.

The constants and three test names were renamed with it, because they had
started to lie. `HEAD_RENAME_IMPACT` and
`impact_with_explain_off_is_byte_identical_to_the_pre_change_tree` described
output captured from the binary as it stood before this lane, which is what
made D1's "explain off changes nothing" a claim about real prior output
rather than about a re-run of the code under test. After a deliberate format
change they no longer pin that, so they are now `WIRE_*` and
`..._is_byte_identical_to_the_captured_wire_output`. What they guarantee
today is narrower and is stated in the test's own doc comment: the plain
path and the explain-off path stay byte-equal to a captured transcript. The
original provenance is recorded here rather than erased.

### 14.2 The description-only check became a whole-file check

The old D5 assertion only scanned tool descriptions, which is precisely why
five em dashes sat in output format strings for a whole lane without
failing anything. Output text is read by an agent and by a human over its
shoulder, so it is as public as a description.
`no_em_dash_survives_anywhere_in_this_file` now scans the source of
`src/mcp/server.rs` itself through `include_str!`, covering descriptions,
output strings and comments alike, and reports the offending line numbers.

Mutation-checked, since a passing style test proves nothing on its own:

<!-- credo-lint:allow-fenced verbatim transcript of a planted em dash, which is the mutation under test -->
```
# planted: const DEFAULT_EXPLAIN_LIMIT: usize = 10; // planted — mutation
test mcp::server::tests::no_em_dash_survives_anywhere_in_this_file ... FAILED
em dash or en-dash-as-punctuation in src/mcp/server.rs: [
test result: FAILED. 0 passed; 1 failed

# reverted
test result: ok. 1 passed; 0 failed
```

## 15. Tie-ordering audit: one real fix upstream, nothing to change here

The bug class is a sort on a score alone over rows arriving in store order,
followed by a truncate or take, so a tie straddling the cut changes which
rows come back between runs. Audited every sort and every cut in
`src/mcp/server.rs`:

| Site | Order | Verdict |
|---|---|---|
| `by_count_desc` (scoped summary) | `(count desc, key asc)` | total already |
| `sample_paths` | `sort()` then `truncate(3)` over unique paths | total already |
| filtered hubs | prefix of `graph::hub_nodes::find_hub_nodes` | forwards library order |
| `explain_section` | prefix of `explain::explain_symbol` | forwards library order |
| `clones` | forwards `graph::clones::find_clone_groups` | forwards library order |
| `search`, `quality`, unfiltered `architecture` | forward library results | forwards library order |

So nothing in this file changed, which is the outcome the addendum
anticipated. The two sorts it owns were already total, and every other cut
is a prefix of a library result.

The forwarded orders are now total upstream too, which is what makes
forwarding safe rather than merely someone else's problem.
`find_hub_nodes` sorts `(total_degree desc, file_path asc, name asc)` as of
lane L7's sweep, and the filtered-hub path takes a prefix of that full
ranking rather than of a pre-truncated list, which is why
`architecture_report_filtered` asks for `usize::MAX` and applies `limit`
last.

One consequence worth stating plainly: the group-ordering instability this
receipt reported in §3.1 is a different bug and is still open. That one is
not a tie-break in a sort, it is `code_node` row order varying across
re-indexes of the same tree, so no total order in a query layer fixes it.

## 16. Definition of done, addenda

```
$ touch src/lib.rs src/mcp/server.rs && cargo build --release 2>&1 | grep -c '^warning'
0

$ cargo test --release | awk over '^test result'
TOTAL passed=340 failed=0

$ cargo test --release --test mcp_tools
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`touch src/lib.rs` before the final run is deliberate. Lane L7 reported a
test target reporting a stale pass count until a rebuild was forced, so
every number in this section comes from binaries rebuilt in the same
command. The same check was run against part two's reported total before
this work started, and 337 reproduced exactly, so that figure was not
stale either. 340 is 337 plus the whole-file em dash test, which compiles
into the lib only now that the bin no longer duplicates the module tree,
plus two from other lanes.

## 17. `codegraph_plan_stale`'s description, corrected against the code

L2's `47811f3` and `d95bad6` changed what `stale` reports, so the tool's
description was describing behavior that no longer exists. Two corrections,
both checked against `src/plan/ops.rs` rather than taken from the
instruction:

1. It returns every **planned or active** item carrying an UNRESOLVED touch.
   `stale` skips `Done | Abandoned` (`ops.rs:1237`), so the old wording
   overstated its scope.
2. The paragraph explaining that `actionable` is "deliberately smaller than
   the raw total" because a roadmap citing its own specification documents
   carries permanently unbindable touches is gone. `UnresolvedReason` no
   longer has a `FileTypeNotIndexed` variant at all, every `ReasonCount` is
   now built with `actionable: true`, and such a file binds RESOLVED with
   `indexed: false` without reaching this report.

The replacement text says `actionable` counts unresolved **touches**, not
items. That correction is the one worth recording, because the instruction
to make this change said `actionable` "equals the item count", and the code
says otherwise: it sums `ReasonCount.count` over
`flat_map(|i| i.unresolved.iter())`, which counts incidences. Measured
through the tool rather than argued:

```
-> tools/call codegraph_plan_stale {}     # ops-rust.yaml over tests/fixtures/rust
1 item(s) carry 2 unresolved touch(es) needing action.
  stale items       : 1  -> ['OPS-1']
  unresolved touches: 2
  actionable        : 2
  VERDICT: actionable == item count?   False
  VERDICT: actionable == touch count?  True
```

The two numbers coincide whenever every stale item happens to carry exactly
one unresolved touch, which is the case on this repository's own roadmap and
is exactly how the wrong reading survives. The summary line was reworded too,
since "1 item(s) carry an unresolved touch, 2 of which need action" reads as
a subset of the items.

## 18. A local side effect, disclosed

Probing this behavior, one `codegraph plan sync` ran without an honored
`--db-url` and wrote `work_plane`, `work_item` and `work_touch` rows under
the project id `sp` into this repository's own `.codegraph/graph.db`. The
rows are project-scoped, so no other project's queries see them, and that
store is a gitignored local artifact rather than anything tracked. There is
no `codegraph` subcommand that deletes a project, and adding throwaway code
to a shared tree mid-orchestration is a worse trade than saying so here:
`rm -rf .codegraph/graph.db` followed by a re-index clears it.
