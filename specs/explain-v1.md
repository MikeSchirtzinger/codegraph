# explain-v1: machine-checkable evidence chains for impact verdicts

**STATUS: v1 PARTIALLY IMPLEMENTED (2026-09-14).**
Chain shapes S, A and D ship, with a chain verifier and a fixture oracle.
Chain shape N (neutrality certificate) does NOT ship; see §10 for why and
what would unblock it. The `--explain` CLI flag is not yet wired: L7
delivered the library API and the exact CLI patch, and the flag lands in the
integration step (`src/cli.rs` and `src/main.rs` belonged to concurrent
work). Receipt with every command and its output:
`specs/receipts/explain-v1-20260914.md`.

**Correction to §4 below, found on contact with the code.** This spec says
`resolved_by` "exists only as fixture-side ground truth" and that "runtime
output has no rule attribution". That was already wrong when the spec was
written. Binding attribution has been persisted on every RESOLVED,
AMBIGUOUS and UNRESOLVED edge since the resolver first landed (commit
`6382480`), by both the full and the incremental pass, and
`tests/common/mod.rs::check_edges` has asserted it against the corpus the
whole time. Measured: stripping that write fails 9 of 9 fixture projects.
What was genuinely missing is the half a *failed* resolution needs, which is
everything the rules that did not bind did. `resolved_by` names only the
rule that won, and on an UNRESOLVED or AMBIGUOUS edge no rule won, so the
row recorded that resolution failed and nothing about what was tried. That
gap is what L7 closed. See §10.

---

**Original header: DRAFT v0.1 (2026-07-30). Spec only, no implementation started.**
Grounded in: a repository capability audit 2026-07-30 at `8423624`; full read of HyperRAG (arXiv 2602.14470, WWW '26); source-level inspection of the HyperGraphReasoning reference implementation (arXiv 2601.04878, manuscript itself abstract-only). Both are covered in §7.

## 1. Motivation

The gate already *decides* correctly (e4572d6 = sensitivity, 833c17f = specificity). What it cannot do is **show its work**. `StructuralVerdict` carries reason *payloads* (`stale_references` with `from_name`/`from_file`/`to_name`/`to_type`, `ambiguous_refusals` with candidate lists, `Dependent` with `depth`/`of_symbol`) but not the *derivation*: which rule fired, against which candidate pool, and why no alternative was admissible. A rename audit needs exactly that derivation: "here is every place this rename didn't land, **with proof**." Today the proof exists only inside `resolve_one`'s control flow. explain-v1 exposes it.

Grep-confirmed: no `--explain` flag, spec, or plan exists anywhere in the repo. This is greenfield.

## 2. Definitions

- **Evidence chain.** An ordered list of **links**, each link a *fact* independently re-derivable from the store or the source tree: an edge, a definition, a deletion record, a candidate-pool computation, a fingerprint pair. No link may originate from a heuristic score or model output.
- **Chain validity.** A chain is valid iff replaying each link's lookup against the same index state reproduces the link. Replay is the checker; there is no trusted reporter.
- **Verdict function.** The verdict must be a pure, documented function of the chains, using the same decision table as today, unchanged. explain-v1 adds derivation, never alters decisions. `--explain` output with explanations stripped MUST be byte-identical to today's `--json` output (additive-only contract).

## 3. Chain shapes (one per finding type)

**S, stale reference** (the audit's core finding):
1. *Query membership:* how the queried name entered the query set, either an explicit symbol argument or a `deleted_symbol` record `(qualified_name, file_path, recorded_at_index_run)` reached via `impact_verdict_for_paths`.
2. *The capture:* the surviving reference, meaning `from_name`, `from_file`, span, `to_name`, `to_type`.
3. *The matching rule applied:* bare-tail (candidate-pool key: bare name + `to_type` + language family) or qualified exact-or-`::`-suffix, the same rule `resolve_one` uses (the 833c17f invariant), with the computed candidate pool shown **empty of live definitions**.
4. *Replay block:* the exact invocation re-deriving 1–3.

**A, ambiguous refusal:** capture, then cascade position reached, then full candidate set, then the discriminator that was *absent* (for example no import edge to disambiguate, the R4 data gap outside Rust, `README.md:266-271`). Makes "the resolver refused to guess" inspectable instead of asserted.

**D, dependent:** the edge path from root to dependent (depth ≤ 3, fixed), each hop carrying its resolution status and the cascade rule (r1..r6) that bound it. **Requires the schema change in §4.**

**N, neutrality certificate** (the audit's "receipt"): per symbol, the fingerprint pair (prior generation vs current from `src/index/fingerprint.rs`), the canonical hash under `src/canon.rs`, and the typed 1-hop neighborhood that hashed. Replay = re-canonicalize. `renamed_from` reported when the hash survived a name change.

## 4. Design

**Schema change (the one real prerequisite).** Persist `resolved_by: r1..r6` on every RESOLVED/AMBIGUOUS edge at resolve time. Today `resolved_by` exists only as fixture-side ground truth (`tests/fixtures/*/expected.yaml`). Runtime output has no rule attribution, which blocks chain shape D and cheapens S/A. Candidate pools are **recomputed at explain time** (deterministic), not persisted.

**Surfaces (all additive):**
- CLI: `codegraph query --kind rdeps --explain [--json]`. Human format renders indented chains; `--json` adds `explanations` keyed by finding.
- MCP: optional `explain: bool` on `codegraph_impact`.
- Facade: new fn `explain_verdict_for_paths(store, paths) -> ExplainedVerdict` beside `impact_verdict_for_paths` (`src/facade.rs:246`). The facade is a frozen contract, so existing signatures are untouched. Precedent: `structural_delta_for_paths` was added the same way.

**Output:** versioned JSON (`explain_version: 1`), serde-plain like `StructuralVerdict` (no SurrealDB types across the boundary). Every chain ends with a `replay` block: exact commands (`codegraph query ...`, `cargo run --example ...`) that re-derive the chain against the same index. The replay block is what makes the audit deliverable self-verifying for a skeptical client: they run it on their machines, without us.

**Bundled fix:** the stale-message printer bug (`src/main.rs:399`, `src/mcp/server.rs:198`, where "no live symbol currently has this name" was printed unconditionally; disclosed in 833c17f). explain-v1 makes the message derive from chain link S.1, which eliminates the unconditional branch rather than patching it.

## 5. Verification

- **Chain-replay harness** (test-side `verify_chain`): re-executes every link lookup and fails on any mismatch. Applied to every chain shape on the fixture corpus.
- **Kill-tests, both directions** (mirroring `tests/gate_specificity.rs`, 330 lines): (i) clean tree ⇒ every explanation is a certificate, no S/A chains, per file and in aggregate; (ii) the same tree with one incomplete rename ⇒ an S chain exists whose link 3 shows an empty live candidate pool, and `verify_chain` passes on it. Neither property may be bought by weakening the other; both must fail against a stub that fabricates chains.
- **Byte-compat test:** `--json` without `--explain` unchanged vs today.

## 6. Non-goals

No LLM anywhere in chain construction, admission, or rendering. No cross-repo chains. No depth parameter (stays fixed at 3, per `facade.rs:18-22`). No blast-radius-based rejection (frozen rationale, `design.md:415-423`). No re-litigation of verdict semantics.

## 7. Related work & terminology (decided)

**HyperRAG (arXiv 2602.14470, WWW '26)** is the nearest published neighbor: it makes chains of n-ary graph edges the unit of evidence, and its pseudo-binary reification (head, mediating fact, tail) is structurally our edge-hop. The resemblance ends there, on four axes: **construction** (ours deterministic; theirs LLM-extracted, non-reproducible run-to-run), **admission** (fact vs plausibility score from a trained MLP/LLM), **audience** (user-facing contract vs internal prompt context, since their chains terminate as prompt tokens and are never surfaced), **checkability** (replayable certificate vs unevaluated heuristic, and chain quality is never measured in the paper; "verifiable," "certificate," "witness," "machine-checkable" never appear). Citing it *strengthens* the novelty claim.

**Higher-Order Knowledge Representations for Agentic Scientific Reasoning (arXiv 2601.04878, Stewart & Buehler, MIT LAMM)** is the nearest neighbor on *posture* rather than on chains, and it is the more useful of the two to have read. It equips agents with hypergraph traversal tools under node-intersection constraints and concludes, verbatim, that "hypergraph topology acts as a verifiable guardrail." That is §2's no-scoring requirement, reached independently, in materials science, by a group with an ACL/NeurIPS/ICML track record. Their traversal admission is genuinely exact and not merely described as such: `intersects()` is `len(A & B) >= s` (`GraphReasoning/graph_tools.py:1532`), candidate paths rank by hop count alone (`:1566`, `sorted(paths_found, key=len)`), no weight or similarity enters the walk, and the shipped agent config sets `intersection_threshold = 2` rather than the permissive `s=1` default (`Notebooks/SG/Agents.ipynb`), so the higher-order constraint does real work rather than degenerating to ordinary connectivity.

**The instructive part is where their exactness stops.** Entry into the graph is soft: an LLM extracts keywords from the question (`:2590`), and those keywords are matched to nodes by embedding cosine at threshold 0.9 (`:153`). The guardrail therefore engages only *after* an unverified hop that can silently seat the traversal on the wrong node, and nothing downstream can detect it, precisely *because* every subsequent link is exact: the machinery will faithfully derive a clean, replayable-looking path from a wrong origin. Exactness after an inexact first step buys the appearance of rigor, not rigor.

That failure mode is what chain link **S.1** (§3) exists to close, and it is why query membership is a *link* rather than a preamble: it is admissible only as an explicit symbol argument or a `deleted_symbol` record `(qualified_name, file_path, recorded_at_index_run)`, both exact and both replayable. codegraph is exact at *both* ends of the chain; S.1 is the load-bearing difference from the nearest prior art, not a formality. **Design consequence, binding on future work:** any convenience feature that resolves a user's fuzzy string to a symbol by similarity would reintroduce their gap at exactly this link. If such an entry point is ever added it must be a separate, explicitly labeled, non-chain surface, and it may never satisfy S.1. Add it to §6 non-goals if it is ever proposed for v1.

*Provenance for the above:* the repository (Apache-2.0, `github.com/lamm-mit/HyperGraphReasoning`, the paper's own artifact, matching title, authors, and self-citation) was inspected directly at the cited lines; the "verifiable guardrail" phrasing and the ~1,100-manuscript / 161,172-node / 320,201-hyperedge / ~1.23-exponent figures are quoted from the arXiv abstract. The manuscript body has **not** been read and its reported figures have **not** been independently checked against the shipped artifact, and no claim in this section depends on them. Note also that their corpus is not reproducible from the repo (source PDFs and converted markdown are deliberately withheld), which is a fair contrast to draw on *construction* but not one this spec needs.

**Terminology:** our term of art is **"machine-checkable evidence chain."** We deliberately avoid **"relational chain"**, because in the RAG literature it now denotes a probabilistically-scored retrieval path, the exact connotation we reject. Proof-carrying vocabulary (certificate, witness, replay, machine-checkable) is unclaimed in that literature and is ours to own. **One correction to the earlier draft of this line: "verifiable" is *not* unclaimed.** Stewart & Buehler use it in exactly our sense. Treat it as shared vocabulary and lead with "machine-checkable" where the distinction has to carry weight; claiming to have coined "verifiable" here would be checkable and wrong.

## 8. Acceptance criteria (binary)

1. `query --kind rdeps --explain --json` emits ≥1 valid chain per finding on the killtest fixture; `verify_chain` green on all.
2. Replay block executes cleanly on a fresh checkout + index of the same SHA (the client-machine scenario).
3. Byte-compat: non-explain `--json` output unchanged.
4. Printer bug gone; message text derived from chain link S.1.
5. Suite stays green; the two new kill-tests fail against pre-implementation code.
6. `resolved_by` present on every RESOLVED/AMBIGUOUS edge after `resolve`; absent on UNRESOLVED (nothing fired).

## 9. Open questions

1. Does `ExplainedVerdict` fold into the JSONL governance events (AgentViz would render chains), or stay query-side for v1? (Lean: query-side; events stay lean.)
2. Chain rendering in the audit report: generate the reader-facing markdown directly from `explanations`, or keep report authoring manual at first? (Lean: manual first, template after two audits.)
3. Does N (certificate) ship in v1, or does v1 ship S/A/D and certificates remain `structural_delta_for_paths`-only until the audit needs the merged view? (Lean: ship all four; the audit's "receipt" section needs N.)

## 10. What shipped, and how the open questions were resolved

2026-09-14. Built on the documented leans, with one forced departure.

**Q1, resolved as leaned: query-side.** Chains live on `StructuralVerdict`
as an optional `explanations` field and nowhere else. Nothing was added to
the governance event stream. The field is `Option<Vec<Chain>>` rather than a
bare `Vec` so a consumer can tell "explain was not requested" from "explain
was requested and there was nothing to explain", and it carries
`skip_serializing_if`, so a verdict produced with explain off serializes to
exactly the bytes it did before the field existed. That is pinned two ways:
a byte-for-byte comparison against output captured from the pre-change tree,
and a live `codegraph query --kind rdeps --json` run whose top-level keys are
still the original seven.

**Q2, resolved as leaned: report authoring stays manual.** No markdown
generator was built. `Chain::render` produces an indented human rendering for
a terminal, and every chain serializes to JSON for a tool, which is enough
for the first reports to be written by hand.

**Q3, departed from the lean: N does not ship.** The lean was to ship all
four shapes. N cannot be built in this lane and, more importantly, should not
be faked. Its verification step is re-canonicalization, which lives in
`src/canon.rs` and `src/index/fingerprint.rs`. Both are frozen under the
board's hard constraint 1 and were not opened. The `fingerprint` table itself
is readable, so a chain *could* be assembled that reports the fingerprint pair
and then "verifies" by re-reading the same row it was built from. That is a
receipt, not a certificate: it re-checks nothing, and shipping it under the
"machine-checkable" banner would make the banner mean less everywhere else it
appears. S, A and D each re-derive, so they earn the word. N is deferred
until canon unfreezes.

**One design rule added, binding on later work.** Verification re-derives; it
never compares against a recording. `verify_chain` re-runs the real resolver
cascade and recomputes candidate pools and live-definition sets from the node
set, so a chain that agrees with a tampered row still fails. Tamper coverage
is a test obligation, not a nicety: a verifier that accepts everything is
indistinguishable from no verifier, so `tests/explain.rs` mutates a node id, a
rule id, a candidate, a candidate pool and a membership record, and asserts
both rejection and which step caught it.

**The §4 schema prerequisite, as actually built.** Two fields were added
beside the existing `resolved_by`, written by both the full and the
incremental pass:

- `attempted_rules`: every cascade rule that actually ran, in order.
- `resolution_outcome`: why the cascade ended where it did, as one of
  `bound`, `ambiguous`, `no_candidates`, `no_rule_matched`, `orphan_source`.

The last two are the distinction a stale-reference finding turns on, and the
spec did not anticipate needing them apart: `no_candidates` means nothing in
the project answers to that name at all, which is the renamed-away regime,
while `no_rule_matched` means something does and no rule admitted it, which
is the qualified-capture regime (`std::env::args`, `cursor::node`). Reporting
both as "unresolved" would tell a client the second case is an incomplete
rename when it is not.

`code_edge` is SCHEMALESS, so neither field needed a DDL change for its
writes to land, which is confirmed by dumping the raw columns out of a store
written before any DDL existed for them. Both are nonetheless declared in
`src/schema.surql` beside `resolved_by`, `resolution_gen` and `candidates`,
matching that file's convention of declaring every resolution-layer field
explicitly. Both are `option`, because `EXTRACTED` and derived `file_ref`
rows never pass through the resolver and carry neither.
