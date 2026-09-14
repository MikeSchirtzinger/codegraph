//! explain-v1: rule attribution, evidence chains, and the chain verifier
//! (`specs/explain-v1.md`).
//!
//! Four things are under test here, in the order the spec builds them:
//!
//! 1. **Rule attribution (D1).** The resolver persists not just which rule
//!    bound an edge but which rules it *reached* and why the last one
//!    produced what it did, on both the full and the incremental pass, with
//!    the two passes agreeing.
//! 2. **The fixture oracle (D2).** Runtime attribution is checked against
//!    the corpus manifests' own ground truth.
//! 3. **Chains (D3) and the verifier (D4).** Every chain the explainer
//!    emits over the corpus re-verifies clean against the graph, and a
//!    tampered chain does not.
//! 4. **Byte-compat (D5).** A verdict with explain off serializes to
//!    exactly what it did before `explanations` existed.
//!
//! The tamper tests are the ones that matter. A verifier that passes
//! everything is indistinguishable from no verifier, so each tamper case
//! mutates one specific thing a forger would mutate (a node id, a rule id, a
//! candidate) and asserts both that the chain is rejected *and* which step
//! rejected it.

mod common;

use std::collections::{BTreeSet, HashMap};

use codegraph::facade::{self, StructuralVerdict, VerdictTarget};
use codegraph::graph::explain::{
    self, Chain, ChainVerdict, DeletedSymbol, ExplainGraph, Fact, FindingKind, Membership,
    StepVerdict,
};
use codegraph::index::resolve;

use common::{fresh_db, index_fixture, load_manifest, DbEdge, EdgeCase, Loaded};

// ============================================================================
// D2 — the fixture oracle
// ============================================================================

/// Every per-project manifest in the corpus.
const FIXTURE_MANIFESTS: &[(&str, &str)] = &[
    ("tests/fixtures/rust/expected.yaml", "oracle-rust"),
    ("tests/fixtures/typescript/expected.yaml", "oracle-ts"),
    ("tests/fixtures/python/expected.yaml", "oracle-py"),
    ("tests/fixtures/go/expected.yaml", "oracle-go"),
    ("tests/fixtures/java/expected.yaml", "oracle-java"),
    ("tests/fixtures/c-cpp/expected.yaml", "oracle-ccpp"),
    ("tests/fixtures/polyglot/expected.yaml", "oracle-poly"),
    (
        "tests/fixtures/rename-refactor/before/expected.yaml",
        "oracle-rename-before",
    ),
    (
        "tests/fixtures/rename-refactor/after/expected.yaml",
        "oracle-rename-after",
    ),
];

/// The rule sequence the documented cascade must reach, given which rule the
/// fixture says won and whether the capture was qualified.
///
/// Derived from `specs/resolution-layer-v1.md`'s cascade order and each
/// manifest's own pre-existing `resolved_by` ground truth, NOT from the
/// implementation: a qualified capture is R1 then R2 then the Rust-only R2m
/// then the terminal, a bare one is R3 then R4 then the terminal, and the
/// cascade stops at whichever rule bound the edge. Encoding it this way is
/// what keeps the oracle independent — copying the resolver's own output
/// into the manifests would assert only that the code agrees with itself.
fn expected_attempts(resolved_by: &str, qualified: bool) -> Vec<&'static str> {
    if qualified {
        let full = ["r1", "r2", "r2m", "r6"];
        let stop = full
            .iter()
            .position(|r| *r == resolved_by)
            .unwrap_or_else(|| panic!("{resolved_by:?} is not a rule the qualified branch reaches"));
        full[..=stop].to_vec()
    } else {
        let full = ["r3", "r4", if resolved_by == "r5" { "r5" } else { "r6" }];
        let stop = full
            .iter()
            .position(|r| *r == resolved_by)
            .unwrap_or_else(|| panic!("{resolved_by:?} is not a rule the bare branch reaches"));
        full[..=stop].to_vec()
    }
}

/// `resolution_outcome` values the fixture's own `confidence` admits.
fn expected_outcomes(confidence: &str) -> &'static [&'static str] {
    match confidence {
        "RESOLVED" => &["bound"],
        "AMBIGUOUS" => &["ambiguous"],
        // The manifests record that nothing bound, not *why*. Both reasons
        // are legitimate; `unresolved_outcome_distinguishes_...` below pins
        // the two apart on cases where the answer is known.
        "UNRESOLVED" => &["no_candidates", "no_rule_matched"],
        other => panic!("unknown confidence {other:?} in a manifest"),
    }
}

fn find_edge<'a>(loaded: &'a Loaded, expect: &EdgeCase) -> &'a DbEdge {
    let from = loaded
        .by_qualified_name(&expect.from)
        .unwrap_or_else(|| panic!("case {}: caller {:?} not indexed", expect.case, expect.from));
    let matches: Vec<&DbEdge> = loaded
        .edges
        .iter()
        .filter(|e| {
            e.edge_type == expect.edge_type
                && e.from_id == from.node_id
                && e.to_name == expect.to_name
                && e.to_type == expect.to_type
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "case {}: expected exactly one matching edge, found {}",
        expect.case,
        matches.len()
    );
    matches[0]
}

/// **D2.** Run the real resolver over every fixture and check runtime rule
/// attribution against the corpus ground truth.
///
/// The `resolved_by` half of this passes on the pre-change tree: binding
/// attribution has been persisted since the resolver landed, and
/// `tests/common/mod.rs::check_edges` already asserted it. The
/// `attempted_rules` / `resolution_outcome` half is what explain-v1 adds,
/// and what fails against the pre-change tree (the columns do not exist, so
/// both come back empty).
#[tokio::test]
async fn fixture_oracle_matches_runtime_rule_attribution() {
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (manifest_path, project_id) in FIXTURE_MANIFESTS {
        let manifest = load_manifest(manifest_path);
        let db = fresh_db().await.expect("fresh store");
        index_fixture(&db, project_id, &manifest.root)
            .await
            .expect("index fixture");
        let loaded = Loaded::fetch(&db, project_id).await.expect("load rows");

        for expect in &manifest.edges {
            let edge = find_edge(&loaded, expect);
            let label = format!("{}/{}", manifest.fixture, expect.case);
            checked += 1;

            if edge.resolved_by != expect.resolved_by {
                failures.push(format!(
                    "{label}: resolved_by is {:?}, manifest says {:?}",
                    edge.resolved_by, expect.resolved_by
                ));
            }

            // The manifest records the capture verbatim, so whether the
            // cascade took the qualified or the bare branch is a property of
            // the manifest, not of the runtime.
            let qualified = expect.to_name.contains("::")
                || expect.to_name.contains('.')
                || expect.to_name.contains('/');
            let want = expected_attempts(&expect.resolved_by, qualified);
            if edge.attempted_rules != want {
                failures.push(format!(
                    "{label}: attempted_rules is {:?}, the cascade must reach {want:?}",
                    edge.attempted_rules
                ));
            }

            let allowed = expected_outcomes(&expect.confidence);
            if !allowed.contains(&edge.resolution_outcome.as_str()) {
                failures.push(format!(
                    "{label}: resolution_outcome is {:?}, {} admits {allowed:?}",
                    edge.resolution_outcome, expect.confidence
                ));
            }
        }
    }

    println!("fixture oracle: {checked} edge case(s) checked, {} failure(s)", failures.len());
    assert!(
        failures.is_empty(),
        "{}/{checked} case(s) failed:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    assert_eq!(checked, 51, "the corpus carries 51 edge cases with ground truth");
}

/// The two UNRESOLVED reasons are genuinely different findings, and the
/// resolver must tell them apart: nothing is named this at all, versus
/// something is and no rule admitted it. Pinned on the pure cascade, where
/// both node sets are constructed rather than inferred.
#[test]
fn unresolved_outcome_distinguishes_an_absent_name_from_an_unmatched_rule() {
    let nodes = vec![
        node("n:caller", "caller", "function", "src/a.rs", "a::caller"),
        node("n:node", "node", "function", "src/b.rs", "b::node"),
    ];
    let indices = resolve::build_indices(&nodes);

    // Nothing in the project is named `vanished`: the pool is empty.
    let (binding, trace) = resolve::resolve_one_traced(
        &edge("n:caller", "vanished", "function"),
        &nodes,
        &indices,
    );
    assert_eq!(binding.confidence, "UNRESOLVED");
    assert_eq!(trace.outcome.as_tag(), "no_candidates");
    assert_eq!(trace.pool_size, 0);

    // `cursor::node` bare-tails onto `b::node`, so the pool is NOT empty —
    // but a qualified capture never degrades to its bare tail, so R1/R2/R2m
    // all admit nothing. This is the case the resolver's own doc comment
    // cites as the reason that fallback is forbidden.
    let (binding, trace) = resolve::resolve_one_traced(
        &edge("n:caller", "cursor::node", "function"),
        &nodes,
        &indices,
    );
    assert_eq!(binding.confidence, "UNRESOLVED");
    assert_eq!(trace.outcome.as_tag(), "no_rule_matched");
    assert_eq!(trace.pool_size, 1, "the bare tail `node` does have a definition");
    assert_eq!(trace.attempted_rules(), vec!["r1", "r2", "r2m", "r6"]);
}

/// Tracing must not change any verdict. Same cascade body, two recorders,
/// so the traced and untraced entry points have to agree on every edge of
/// every fixture.
#[tokio::test]
async fn tracing_never_changes_a_binding() {
    let manifest = load_manifest("tests/fixtures/rust/expected.yaml");
    let db = fresh_db().await.expect("fresh store");
    index_fixture(&db, "trace-parity", &manifest.root)
        .await
        .expect("index");
    let loaded = Loaded::fetch(&db, "trace-parity").await.expect("load");

    let nodes: Vec<resolve::ResolverNode> = loaded
        .nodes
        .iter()
        .map(|n| resolve::ResolverNode {
            id: n.node_id.clone(),
            name: n.name.clone(),
            node_type: n.node_type.clone(),
            language: "rust".to_string(),
            file_path: n.file_path.clone(),
            qualified_name: n.qualified_name.clone(),
        })
        .collect();
    let indices = resolve::build_indices(&nodes);

    let mut compared = 0;
    for e in &loaded.edges {
        if e.to_name.is_empty() {
            continue;
        }
        let ue = edge(&e.from_id, &e.to_name, &e.to_type);
        let plain = resolve::resolve_one(&ue, &nodes, &indices);
        let (traced, trace) = resolve::resolve_one_traced(&ue, &nodes, &indices);
        assert_eq!(plain, traced, "tracing changed the binding for {:?}", e.to_name);
        if !trace.attempts.is_empty() {
            assert_eq!(
                trace.attempts.last().unwrap().rule,
                traced.resolved_by,
                "the last rule attempted must be the rule reported for {:?}",
                e.to_name
            );
        }
        compared += 1;
    }
    assert!(compared > 0, "the fixture produced no edges to compare");
    println!("trace parity: {compared} edge(s) agreed");
}

/// **D1, incremental half.** The incremental pass must leave the same
/// attribution a from-scratch index would, or a chain built after an
/// incremental run would describe a cascade that never ran.
#[tokio::test]
async fn incremental_attribution_matches_a_fresh_full_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");

    let write = |rel: &str, body: &str| {
        std::fs::write(root.join(rel), body).expect("write");
    };
    write("src/target.rs", "pub fn helper() {}\n");
    write("src/caller.rs", "pub fn call_it() {\n    helper();\n}\n");
    write("src/other.rs", "pub fn helper() {}\n");

    // Index, then rename one definition away and re-index incrementally.
    let db_inc = fresh_db().await.expect("store");
    index_at(&db_inc, "inc", root, true).await;
    write("src/target.rs", "pub fn helper_v2() {}\n");
    index_at(&db_inc, "inc", root, false).await;

    // The same final tree, indexed once from nothing.
    let db_fresh = fresh_db().await.expect("store");
    index_at(&db_fresh, "inc", root, true).await;

    let inc = attribution_by_key(&Loaded::fetch(&db_inc, "inc").await.expect("load"));
    let fresh = attribution_by_key(&Loaded::fetch(&db_fresh, "inc").await.expect("load"));

    assert!(!fresh.is_empty(), "the fixture produced no name-edges");
    assert_eq!(
        inc.keys().collect::<BTreeSet<_>>(),
        fresh.keys().collect::<BTreeSet<_>>(),
        "incremental and fresh disagree on which edges exist"
    );
    for (key, inc_val) in &inc {
        assert_eq!(
            inc_val, &fresh[key],
            "attribution for {key:?} differs: incremental {inc_val:?} vs fresh {:?}",
            fresh[key]
        );
    }
    println!("incremental parity: {} edge(s) agreed on (resolved_by, attempted_rules, outcome)", inc.len());
}

/// The incremental write-back must leave every edge row byte-identical to
/// what the `--force` bulk-insert path leaves, field for field.
///
/// `write_updates` was rewritten from one generated `UPDATE` statement per
/// row into one statement per chunk iterating a bound array, which is a
/// change to how the rows are written and must not be a change to what is
/// written. The existing oracle test compares rule attribution; this one
/// compares the whole row, including `to_id`, `confidence`, `candidates` and
/// the derived `file_ref` edges that `write_file_refs` now bulk-inserts.
#[tokio::test]
async fn incremental_write_back_matches_the_force_path_field_for_field() {
    // Every fixture with a non-trivial edge set, so this covers RESOLVED,
    // AMBIGUOUS and UNRESOLVED rows, candidate lists, and cross-file refs.
    for root in [
        "tests/fixtures/rust",
        "tests/fixtures/polyglot",
        "tests/fixtures/go",
        "tests/fixtures/rename-refactor/after",
    ] {
        let pid = "writeback-parity";

        // Incremental path: index once without force, which is what a bare
        // `codegraph index` does on a first run.
        let db_inc = fresh_db().await.expect("store");
        index_at(&db_inc, pid, &common::repo_path(root), false).await;

        // Force path: the bulk-insert write-back.
        let db_force = fresh_db().await.expect("store");
        index_at(&db_force, pid, &common::repo_path(root), true).await;

        let inc = Loaded::fetch(&db_inc, pid).await.expect("load");
        let force = Loaded::fetch(&db_force, pid).await.expect("load");

        let key = |e: &DbEdge| {
            (
                e.from_id.clone(),
                e.to_name.clone(),
                e.to_type.clone(),
                e.edge_type.clone(),
            )
        };
        let row = |e: &DbEdge| {
            let mut c = e.candidates.clone();
            c.sort();
            (
                e.to_id.clone(),
                e.confidence.clone(),
                e.resolved_by.clone(),
                c,
                e.attempted_rules.clone(),
                e.resolution_outcome.clone(),
            )
        };

        let inc_rows: HashMap<_, _> = inc.edges.iter().map(|e| (key(e), row(e))).collect();
        let force_rows: HashMap<_, _> = force.edges.iter().map(|e| (key(e), row(e))).collect();

        assert!(!force_rows.is_empty(), "{root}: fixture produced no edges");
        assert_eq!(
            inc_rows.keys().collect::<BTreeSet<_>>(),
            force_rows.keys().collect::<BTreeSet<_>>(),
            "{root}: the two write-backs disagree on which edges exist"
        );
        for (k, inc_val) in &inc_rows {
            assert_eq!(
                inc_val, &force_rows[k],
                "{root}: row {k:?} differs between the incremental and force write-backs"
            );
        }

        // The derived file-level graph too, which `write_file_refs` now
        // bulk-inserts instead of creating one row at a time.
        let refs = |l: &Loaded| {
            let mut v: Vec<(String, String)> = l
                .edges
                .iter()
                .filter(|e| e.edge_type == "file_ref")
                .map(|e| (e.from_id.clone(), e.to_name.clone()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(refs(&inc), refs(&force), "{root}: file_ref rows differ");

        println!("{root}: {} edge row(s) identical across both write-backs", inc_rows.len());
    }
}

/// `write_file_refs` derives the file-level graph, and a bulk insert must
/// produce the same rows a per-pair create did: right count, right fields,
/// no duplicates.
#[tokio::test]
async fn file_refs_are_written_once_with_the_expected_shape() {
    let db = fresh_db().await.expect("store");
    index_fixture(&db, "fileref-shape", "tests/fixtures/rust")
        .await
        .expect("index");

    let mut resp = db
        .query(
            "SELECT from_file, to_file, edge_type, confidence FROM code_edge \
             WHERE project_id = $p AND edge_type = 'file_ref'",
        )
        .bind(("p", "fileref-shape".to_string()))
        .await
        .expect("query");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take");

    assert!(!rows.is_empty(), "the rust fixture has cross-file calls");
    let mut seen = BTreeSet::new();
    for v in &rows {
        let surrealdb_types::Value::Object(o) = v else {
            panic!("file_ref row is not an object")
        };
        let get = |k: &str| match o.get(k) {
            Some(surrealdb_types::Value::String(s)) => s.to_string(),
            _ => String::new(),
        };
        assert_eq!(get("edge_type"), "file_ref");
        assert_eq!(get("confidence"), "RESOLVED");
        let ff = get("from_file");
        let tf = get("to_file");
        assert!(!ff.is_empty() && !tf.is_empty(), "file_ref row missing its endpoints");
        assert_ne!(ff, tf, "file_ref must be cross-file");
        assert!(seen.insert((ff, tf)), "bulk insert duplicated a file_ref pair");
    }
    println!("file_ref: {} distinct cross-file pair(s), no duplicates", seen.len());
}

// ============================================================================
// D3 / D4 — chains and the verifier
// ============================================================================

/// **D3 + D4(a).** Every chain the explainer emits over the corpus verifies
/// clean, and the corpus actually produces all three shapes — a suite where
/// the explainer emitted nothing would pass this vacuously.
#[tokio::test]
async fn every_chain_over_the_corpus_verifies_clean() {
    let mut seen: HashMap<&'static str, usize> = HashMap::new();
    let mut total = 0usize;

    for (manifest_path, project_id) in FIXTURE_MANIFESTS {
        let manifest = load_manifest(manifest_path);
        let db = fresh_db().await.expect("fresh store");
        index_fixture(&db, project_id, &manifest.root)
            .await
            .expect("index fixture");
        let graph = ExplainGraph::load(&db, project_id).await.expect("explain graph");

        // Explain every name any manifest node declares, plus every bare
        // tail any capture uses — the union covers live symbols and
        // renamed-away ones alike.
        let mut subjects: BTreeSet<String> =
            manifest.nodes.iter().map(|n| n.name.clone()).collect();
        for e in &manifest.edges {
            let normalized = e.to_name.replace(['.', '/'], "::");
            subjects.insert(normalized.rsplit("::").next().unwrap_or(&normalized).to_string());
        }

        for subject in &subjects {
            for chain in
                explain::explain_symbol(&graph, project_id, subject, &Membership::ExplicitSymbol)
            {
                let verdict = explain::verify_chain(&chain, &graph);
                assert!(
                    verdict.ok,
                    "{}: chain for {subject:?} ({}) failed to verify: {:?}\n{}",
                    manifest.fixture,
                    chain.finding.as_tag(),
                    verdict.failures(),
                    chain.render()
                );
                assert!(!chain.steps.is_empty(), "a chain with no steps proves nothing");
                *seen.entry(chain.finding.as_tag()).or_default() += 1;
                total += 1;
            }
        }
    }

    println!("corpus chains: {total} emitted, all verified clean — {seen:?}");
    for shape in ["stale_reference", "ambiguous_refusal", "dependent"] {
        assert!(
            seen.get(shape).copied().unwrap_or(0) > 0,
            "the corpus produced no {shape} chains, so this test proved nothing about that shape"
        );
    }
}

/// A chain must round-trip through JSON unchanged, or "hand the chain to a
/// second program" is not something a client can actually do.
#[tokio::test]
async fn a_chain_survives_a_json_round_trip_and_still_verifies() {
    let (db, graph) = rename_after_graph("chain-roundtrip").await;
    let _ = &db;
    let chains = explain::explain_symbol(&graph, "chain-roundtrip", "helper", &Membership::ExplicitSymbol);
    let stale = first_of(&chains, FindingKind::StaleReference);

    let wire = serde_json::to_string(stale).expect("serialize chain");
    let back: Chain = serde_json::from_str(&wire).expect("deserialize chain");
    assert_eq!(&back, stale, "a chain did not survive its own wire format");
    assert!(
        explain::verify_chain(&back, &graph).ok,
        "a round-tripped chain stopped verifying"
    );
}

/// The S chain's shape, on a real incomplete rename: the capture is there,
/// the live-definition set is empty, and the cascade says so.
#[tokio::test]
async fn the_stale_chain_shows_an_empty_live_definition_set() {
    let (db, graph) = rename_after_graph("stale-shape").await;
    let _ = &db;
    let chains =
        explain::explain_symbol(&graph, "stale-shape", "helper", &Membership::ExplicitSymbol);
    let stale = first_of(&chains, FindingKind::StaleReference);

    let live = stale
        .steps
        .iter()
        .find_map(|s| match &s.fact {
            Fact::LiveDefinitions { name, node_ids } => Some((name.clone(), node_ids.clone())),
            _ => None,
        })
        .expect("an S chain must state the live-definition set");
    assert_eq!(live.0, "helper");
    assert!(
        live.1.is_empty(),
        "helper was renamed away, so nothing should still define it: {:?}",
        live.1
    );

    let outcome = stale
        .steps
        .iter()
        .find_map(|s| match &s.fact {
            Fact::CascadeOutcome { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("an S chain must state why the cascade produced nothing");
    assert_eq!(outcome, "no_candidates");
    assert!(explain::verify_chain(stale, &graph).ok);
}

/// The A chain names the discriminator that was absent rather than merely
/// asserting the resolver refused. In `tests/fixtures/rust`, `dispatch`
/// calls a bare `helper` that both `alpha` and `beta` define, and
/// `gamma.rs`'s imports do not narrow it.
#[tokio::test]
async fn the_ambiguous_chain_names_the_absent_discriminator() {
    let db = fresh_db().await.expect("store");
    index_fixture(&db, "ambig-shape", "tests/fixtures/rust")
        .await
        .expect("index");
    let graph = ExplainGraph::load(&db, "ambig-shape").await.expect("graph");

    let chains =
        explain::explain_symbol(&graph, "ambig-shape", "helper", &Membership::ExplicitSymbol);
    let ambiguous = first_of(&chains, FindingKind::AmbiguousRefusal);

    let r4 = ambiguous
        .steps
        .iter()
        .find_map(|s| match &s.fact {
            Fact::RuleApplication { rule, outcome, .. } if rule == "r4" => Some(outcome.clone()),
            _ => None,
        })
        .expect("an A chain must report what the import-informed tier did");
    assert!(
        r4 == "skipped" || r4 == "no_match" || r4 == "not_unique",
        "r4 reported {r4:?}, which is not a non-binding outcome"
    );

    // Every candidate is pinned to a real node, not left as a bare string.
    let candidate_nodes = ambiguous
        .steps
        .iter()
        .filter(|s| matches!(s.fact, Fact::NodeExists { .. }))
        .count();
    assert!(
        candidate_nodes >= 3,
        "expected the referring node plus both candidates as nodes, got {candidate_nodes}"
    );
    assert!(explain::verify_chain(ambiguous, &graph).ok);
}

/// Assert a tamper actually changed something before any verdict is taken.
///
/// This is the guard L7 did not have and L4 needed: `Fact` used to be
/// serde-tagged under `fact` while `Step`'s field is also `fact`, so a
/// consumer reaching one level too high wrote a key serde ignores. The
/// tamper became a no-op, the chain verified clean, and the test passed
/// while proving nothing. A verifier's own suite is the last place a vacuous
/// pass can be allowed, so every tamper below goes through here first.
#[track_caller]
fn assert_tampered(original: &Chain, tampered: &Chain) {
    let before = serde_json::to_string(original).expect("serialize original");
    let after = serde_json::to_string(tampered).expect("serialize tampered");
    assert_ne!(
        before, after,
        "the tamper changed nothing, so whatever this test asserts next is vacuous"
    );
}

/// **D4(b) — tampering.** Each case mutates exactly one thing a forger
/// would, and asserts both rejection and which step caught it.
#[tokio::test]
async fn the_verifier_catches_a_swapped_node_id() {
    let (db, graph) = rename_after_graph("tamper-node").await;
    let _ = &db;
    let chains =
        explain::explain_symbol(&graph, "tamper-node", "helper", &Membership::ExplicitSymbol);
    let original = first_of(&chains, FindingKind::StaleReference).clone();
    let mut chain = original.clone();
    assert!(explain::verify_chain(&chain, &graph).ok, "the chain must start clean");

    let step = chain
        .steps
        .iter_mut()
        .find(|s| matches!(s.fact, Fact::NodeExists { .. }))
        .expect("an S chain names the referring node");
    let tampered_index = step.index;
    if let Fact::NodeExists { node_id, .. } = &mut step.fact {
        *node_id = "code_node:not_a_real_node".to_string();
    }

    assert_tampered(&original, &chain);
    let verdict = explain::verify_chain(&chain, &graph);
    assert!(!verdict.ok, "a swapped node id was accepted");
    let caught: Vec<usize> = verdict.failures().iter().map(|f| f.index).collect();
    assert!(
        caught.contains(&tampered_index),
        "the verifier rejected the chain but blamed {caught:?}, not step {tampered_index}"
    );
}

#[tokio::test]
async fn the_verifier_catches_a_changed_rule_id() {
    let (db, graph) = rename_after_graph("tamper-rule").await;
    let _ = &db;
    let chains =
        explain::explain_symbol(&graph, "tamper-rule", "helper", &Membership::ExplicitSymbol);
    let original = first_of(&chains, FindingKind::StaleReference).clone();
    let mut chain = original.clone();

    // Claim the terminal rule was r3 (a bare-name rule) on a qualified
    // capture the cascade never routed through R3 at all.
    let step = chain
        .steps
        .iter_mut()
        .find(|s| matches!(&s.fact, Fact::RuleApplication { rule, .. } if rule == "r1"))
        .expect("an S chain on a qualified capture reports r1");
    let tampered_index = step.index;
    if let Fact::RuleApplication { rule, .. } = &mut step.fact {
        *rule = "r3".to_string();
    }

    assert_tampered(&original, &chain);
    let verdict = explain::verify_chain(&chain, &graph);
    assert!(!verdict.ok, "a forged rule id was accepted");
    let failure = verdict
        .failures()
        .into_iter()
        .find(|f| f.index == tampered_index)
        .unwrap_or_else(|| panic!("step {tampered_index} was not blamed"));
    assert!(
        failure.reason.as_deref().unwrap_or("").contains("never reached"),
        "expected the verifier to say the cascade never reached r3, said {:?}",
        failure.reason
    );
}

#[tokio::test]
async fn the_verifier_catches_a_dropped_candidate() {
    let db = fresh_db().await.expect("store");
    index_fixture(&db, "tamper-cand", "tests/fixtures/rust")
        .await
        .expect("index");
    let graph = ExplainGraph::load(&db, "tamper-cand").await.expect("graph");
    let chains =
        explain::explain_symbol(&graph, "tamper-cand", "helper", &Membership::ExplicitSymbol);
    let original = first_of(&chains, FindingKind::AmbiguousRefusal).clone();
    let mut chain = original.clone();
    assert!(explain::verify_chain(&chain, &graph).ok, "the chain must start clean");

    // Drop one candidate from the stored edge fact — the shape of a report
    // that quietly narrows an ambiguity into a confident answer.
    let step = chain
        .steps
        .iter_mut()
        .find(|s| matches!(&s.fact, Fact::EdgeExists { candidates, .. } if candidates.len() > 1))
        .expect("an A chain carries a multi-candidate edge");
    let tampered_index = step.index;
    if let Fact::EdgeExists { candidates, .. } = &mut step.fact {
        candidates.pop();
    }

    assert_tampered(&original, &chain);
    let verdict = explain::verify_chain(&chain, &graph);
    assert!(!verdict.ok, "a dropped candidate was accepted");
    let failure = verdict
        .failures()
        .into_iter()
        .find(|f| f.index == tampered_index)
        .unwrap_or_else(|| panic!("step {tampered_index} was not blamed"));
    assert!(
        failure.reason.as_deref().unwrap_or("").contains("candidates"),
        "expected a candidate-set mismatch, said {:?}",
        failure.reason
    );
}

/// Tampering with the *pool* is the subtle one: a forger who wants a stale
/// finding to look inevitable would shrink the pool to empty. The verifier
/// recomputes the pool from the node set, so it does not matter what the
/// chain says.
#[tokio::test]
async fn the_verifier_recomputes_the_candidate_pool_rather_than_trusting_it() {
    let db = fresh_db().await.expect("store");
    index_fixture(&db, "tamper-pool", "tests/fixtures/rust")
        .await
        .expect("index");
    let graph = ExplainGraph::load(&db, "tamper-pool").await.expect("graph");
    let chains =
        explain::explain_symbol(&graph, "tamper-pool", "helper", &Membership::ExplicitSymbol);
    let original = first_of(&chains, FindingKind::AmbiguousRefusal).clone();
    let mut chain = original.clone();

    let step = chain
        .steps
        .iter_mut()
        .find(|s| matches!(&s.fact, Fact::CandidatePool { node_ids, .. } if !node_ids.is_empty()))
        .expect("an A chain states the pool");
    let tampered_index = step.index;
    if let Fact::CandidatePool { node_ids, .. } = &mut step.fact {
        node_ids.clear();
    }

    assert_tampered(&original, &chain);
    let verdict = explain::verify_chain(&chain, &graph);
    assert!(!verdict.ok, "an emptied candidate pool was accepted");
    assert!(verdict.failures().iter().any(|f| f.index == tampered_index));
}

/// A deletion-record membership link must name a record that exists. This is
/// S.1: if the entry point could be forged, every exact link after it would
/// derive a clean-looking path from a wrong origin.
#[tokio::test]
async fn the_verifier_catches_a_forged_deletion_record() {
    let (db, graph) = rename_after_graph("tamper-membership").await;
    let _ = &db;
    let forged = DeletedSymbol {
        name: "helper".to_string(),
        qualified_name: "target::helper".to_string(),
        node_type: "function".to_string(),
        file_path: "src/target.rs".to_string(),
    };
    let chains = explain::explain_symbol(
        &graph,
        "tamper-membership",
        "helper",
        &Membership::Deleted(forged),
    );
    let chain = first_of(&chains, FindingKind::StaleReference);

    // This fixture is indexed fresh, so no deletion was ever recorded: the
    // membership claim is unbacked and must be rejected.
    let verdict = explain::verify_chain(chain, &graph);
    assert!(!verdict.ok, "an unbacked deletion record was accepted");
    let failure = verdict
        .failures()
        .into_iter()
        .find(|f| f.fact == "deletion_record")
        .expect("the deletion-record step should be the one blamed");
    assert!(failure.reason.as_deref().unwrap_or("").contains("deleted_symbol"));
}

// ============================================================================
// D5 — the facade contract
// ============================================================================

/// **D5.** A verdict with explain off must serialize to exactly the bytes it
/// did before `explanations` existed. The expected string below was captured
/// from the pre-change tree, from this same construction.
#[test]
fn json_shape_is_unchanged_when_explain_is_off() {
    let verdict = StructuralVerdict {
        target: VerdictTarget::Paths(vec!["src/a.rs".into()]),
        resolved_dependents: vec![facade::Dependent {
            node_id: "n1".into(),
            name: "caller".into(),
            qualified_name: "m::caller".into(),
            node_type: "function".into(),
            file_path: "src/a.rs".into(),
            depth: 1,
            of_symbol: "m::target".into(),
        }],
        ambiguous_refusals: vec![facade::AmbiguousRefusal {
            from_name: "d".into(),
            from_file: "src/b.rs".into(),
            to_name: "helper".into(),
            to_type: "function".into(),
            candidates: vec!["x::helper".into()],
            of_symbol: "m::target".into(),
        }],
        stale_references: vec![facade::StaleReference {
            from_name: "c".into(),
            from_file: "src/c.rs".into(),
            to_name: "gone".into(),
            to_type: "function".into(),
        }],
        name_ambiguous: true,
        matched_symbols: vec![facade::MatchedSymbol {
            node_id: "n0".into(),
            qualified_name: "m::target".into(),
            name: "target".into(),
            file_path: "src/a.rs".into(),
        }],
        graph_empty: false,
        explanations: None,
    };

    const PRE_CHANGE: &str = r#"{"target":{"Paths":["src/a.rs"]},"resolved_dependents":[{"node_id":"n1","name":"caller","qualified_name":"m::caller","node_type":"function","file_path":"src/a.rs","depth":1,"of_symbol":"m::target"}],"ambiguous_refusals":[{"from_name":"d","from_file":"src/b.rs","to_name":"helper","to_type":"function","candidates":["x::helper"],"of_symbol":"m::target"}],"stale_references":[{"from_name":"c","from_file":"src/c.rs","to_name":"gone","to_type":"function"}],"name_ambiguous":true,"matched_symbols":[{"node_id":"n0","qualified_name":"m::target","name":"target","file_path":"src/a.rs"}],"graph_empty":false}"#;

    assert_eq!(
        serde_json::to_string(&verdict).expect("serialize"),
        PRE_CHANGE,
        "explain off must be byte-identical to the pre-change output"
    );

    // A default verdict must also still omit the field entirely.
    let json = serde_json::to_string(&StructuralVerdict::default()).expect("serialize default");
    assert!(
        !json.contains("explanations"),
        "explain off leaked an explanations key: {json}"
    );

    // And the field must deserialize from output that predates it.
    let parsed: StructuralVerdict = serde_json::from_str(PRE_CHANGE).expect("parse pre-change json");
    assert!(parsed.explanations.is_none());
}

/// Explain on attaches chains without disturbing the verdict beside them.
#[tokio::test]
async fn explain_on_adds_chains_and_leaves_the_verdict_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.keep();
    let url = format!("surrealkv://{}/graph.db", path.display());
    {
        let db = surrealdb::engine::any::connect(&url).await.expect("connect");
        db.use_ns("codegraph").use_db("codegraph").await.expect("ns");
        db.query(include_str!("../src/schema.surql"))
            .await
            .expect("ddl")
            .check()
            .expect("ddl check");
        index_fixture(
            &std::sync::Arc::new(db),
            "facade-explain",
            "tests/fixtures/rename-refactor/after",
        )
        .await
        .expect("index");
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let store = facade::open_store(&url, "facade-explain").await.expect("open");
    let paths = vec!["src/stale_caller.rs".to_string()];

    let plain = facade::impact_verdict_for_paths(&store, &paths).await.expect("plain");
    let explained = facade::explain_verdict_for_paths(&store, &paths).await.expect("explained");

    assert!(plain.explanations.is_none(), "explain off must attach nothing");
    let chains = explained.explanations.clone().expect("explain on attaches chains");

    // Strip the chains and the two must serialize identically: the verdict
    // is the same pure function of the same findings either way.
    let mut stripped = explained.clone();
    stripped.explanations = None;
    assert_eq!(
        serde_json::to_string(&plain).expect("json"),
        serde_json::to_string(&stripped).expect("json"),
        "explain changed the verdict it was supposed to only explain"
    );

    // And every attached chain re-verifies against the same store, opened
    // independently. Dropping the facade `Store` first releases surrealkv's
    // single-writer file lock; reopening while it is held deadlocks.
    drop(store);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let db = surrealdb::engine::any::connect(&url).await.expect("reconnect");
    db.use_ns("codegraph").use_db("codegraph").await.expect("ns");
    let graph = ExplainGraph::load(&db, "facade-explain").await.expect("graph");

    assert!(!chains.is_empty(), "the stale-rename fixture must produce chains");
    for chain in &chains {
        assert!(
            explain::verify_chain(chain, &graph).ok,
            "a facade-attached chain failed to verify:\n{}",
            chain.render()
        );
    }
    println!("facade: {} chain(s) attached, all verified", chains.len());
}

/// The exact call shape the CLI's `--explain` patch uses: a raw connection
/// the binary already holds (never a second `open_store`, which would
/// deadlock on surrealkv's single-writer lock), and the result assigned
/// straight onto the verdict the `--json` path already builds.
///
/// Pinned as a test because the patch itself lands in `src/main.rs`, a file
/// another lane owns. If this compiles and passes, the patch's half of the
/// contract holds.
#[tokio::test]
async fn the_cli_patch_call_shape_compiles_and_verifies() {
    let db = fresh_db().await.expect("store");
    index_fixture(&db, "cli-shape", "tests/fixtures/rename-refactor/after")
        .await
        .expect("index");

    let dep_result = codegraph::graph::dependencies::get_reverse_dependencies(
        &db,
        "cli-shape",
        "helper",
        &codegraph::graph::NAME_EDGE_TYPES,
        3,
        true,
    )
    .await
    .expect("rdeps");

    let mut verdict = facade::project_dependency_result(
        VerdictTarget::Symbol("helper".to_string()),
        "helper",
        &dep_result,
        false,
    );
    assert!(verdict.explanations.is_none(), "the flag defaults to off");

    verdict.explanations = Some(
        facade::explain_chains_for_symbol(&db, "cli-shape", "helper")
            .await
            .expect("explain chains"),
    );

    let chains = verdict.explanations.as_ref().unwrap();
    assert!(!chains.is_empty(), "the stale fixture must yield chains");
    let graph = ExplainGraph::load(&db, "cli-shape").await.expect("graph");
    for chain in chains {
        assert!(explain::verify_chain(chain, &graph).ok);
    }

    // And the serialized form carries the chains only when they are set.
    assert!(serde_json::to_string(&verdict).unwrap().contains("explanations"));
}

/// The serialized shape is a published contract the MCP surface and any
/// client tooling reach into by key path, so it gets pinned rather than left
/// to whatever the derive happens to emit.
#[tokio::test]
async fn a_step_serializes_with_an_unambiguous_kind_discriminator() {
    let (db, graph) = rename_after_graph("wire-shape").await;
    let _ = &db;
    let chains =
        explain::explain_symbol(&graph, "wire-shape", "helper", &Membership::ExplicitSymbol);
    let chain = first_of(&chains, FindingKind::StaleReference);
    let json = serde_json::to_value(chain).expect("serialize");

    let step = &json["steps"][0];
    assert!(step["index"].is_number(), "a step carries its index");
    assert!(
        step["fact"]["kind"].is_string(),
        "the discriminator lives at steps[].fact.kind, got {step}"
    );
    assert!(
        step["fact"]["fact"].is_null(),
        "steps[].fact.fact must not exist: that was the ambiguous shape"
    );
    assert!(
        step["kind"].is_null(),
        "the discriminator must not also appear one level up at steps[].kind"
    );
}

/// **Item 2.** The chain types publish a real JSON schema, so the MCP tool
/// can advertise its input instead of accepting `true`.
#[test]
fn chain_types_publish_a_json_schema_naming_the_kind_discriminator() {
    let schema = schemars::schema_for!(Chain);
    let json = serde_json::to_value(&schema).expect("schema serializes");
    let text = serde_json::to_string(&json).expect("schema to string");

    // The discriminator has to survive into the schema, or a consumer
    // generating code from it would not know how to read a step.
    assert!(
        text.contains("\"kind\""),
        "the schema does not mention the kind discriminator: {text}"
    );
    assert!(
        !text.contains("\"fact\":{\"const\""),
        "the schema still carries the old `fact` discriminator: {text}"
    );
    // Every variant tag must be reachable from the schema.
    for tag in [
        "explicit_symbol_query",
        "deletion_record",
        "live_definitions",
        "node_exists",
        "edge_exists",
        "candidate_pool",
        "rule_application",
        "cascade_outcome",
        "resolved_hop",
    ] {
        assert!(text.contains(tag), "schema is missing variant {tag}");
    }

    // The verdict types are published too, since verify_chain returns them.
    assert!(serde_json::to_value(schemars::schema_for!(ChainVerdict)).is_ok());
    assert!(serde_json::to_value(schemars::schema_for!(StepVerdict)).is_ok());
}

/// No verdict reason may carry a Rust `Debug` struct dump.
///
/// Reasons are wire-facing: they ship in JSON, an agent reads them, and a
/// client re-verifying an audit report may see them. `{:?}` on a struct puts
/// Rust field syntax into a sentence meant for a person, which is both ugly
/// and leaky. This drives every failing branch the verifier has and checks
/// each reason it produces.
#[tokio::test]
async fn no_verdict_reason_contains_a_debug_struct_dump() {
    let (db, graph) = rename_after_graph("reason-shape").await;
    let _ = &db;
    let stale_chains =
        explain::explain_symbol(&graph, "reason-shape", "helper", &Membership::ExplicitSymbol);
    let stale = first_of(&stale_chains, FindingKind::StaleReference).clone();

    let db2 = fresh_db().await.expect("store");
    index_fixture(&db2, "reason-shape-2", "tests/fixtures/rust")
        .await
        .expect("index");
    let graph2 = ExplainGraph::load(&db2, "reason-shape-2").await.expect("graph");
    let amb_chains =
        explain::explain_symbol(&graph2, "reason-shape-2", "helper", &Membership::ExplicitSymbol);
    let ambiguous = first_of(&amb_chains, FindingKind::AmbiguousRefusal).clone();

    // Every tamper the suite knows how to make, so every reason branch runs.
    let mut tampered: Vec<(Chain, &ExplainGraph)> = Vec::new();

    // NOTE: the node id stays VALID here and a different attribute is
    // changed instead. Pointing it at a non-existent id takes the earlier
    // "no node with id" branch, which never reaches the struct comparison,
    // and an earlier version of this test did exactly that and proved
    // nothing: it passed with the Debug dump still in place.
    let mut c = stale.clone();
    for step in c.steps.iter_mut() {
        match &mut step.fact {
            Fact::NodeExists { qualified_name, file_path, .. } => {
                *qualified_name = format!("{qualified_name}_tampered");
                *file_path = format!("{file_path}.moved");
            }
            Fact::RuleApplication { rule, hit_ids, .. } => {
                *rule = "r3".to_string();
                hit_ids.push("code_node:ghost".to_string());
            }
            Fact::CandidatePool { node_ids, .. } => node_ids.push("code_node:ghost".into()),
            Fact::LiveDefinitions { node_ids, .. } => node_ids.push("code_node:ghost".into()),
            Fact::CascadeOutcome { outcome, .. } => *outcome = "bound".to_string(),
            Fact::EdgeExists { confidence, .. } => *confidence = "RESOLVED".to_string(),
            Fact::ExplicitSymbolQuery { name } => *name = "not_the_subject".to_string(),
            _ => {}
        }
    }
    tampered.push((c, &graph));

    // A chain whose edge key names no row at all, which is the branch that
    // used to print an `EdgeKey` Debug dump.
    let mut c = stale.clone();
    for step in c.steps.iter_mut() {
        match &mut step.fact {
            Fact::EdgeExists { key, .. } => key.from_id = "code_node:ghost".to_string(),
            Fact::RuleApplication { key, .. } => key.from_id = "code_node:ghost".to_string(),
            Fact::CascadeOutcome { key, .. } => key.from_id = "code_node:ghost".to_string(),
            _ => {}
        }
    }
    tampered.push((c, &graph));

    let mut c = ambiguous.clone();
    for step in c.steps.iter_mut() {
        if let Fact::EdgeExists { candidates, .. } = &mut step.fact {
            candidates.pop();
        }
        if let Fact::ResolvedHop { key, .. } = &mut step.fact {
            key.from_id = "code_node:ghost".to_string();
        }
    }
    tampered.push((c, &graph2));

    // D chains carry ResolvedHop, which has its own key-not-found branch.
    for chain in amb_chains.iter().filter(|c| c.finding == FindingKind::Dependent) {
        let mut c = chain.clone();
        for step in c.steps.iter_mut() {
            if let Fact::ResolvedHop { to_id, .. } = &mut step.fact {
                *to_id = "code_node:ghost".to_string();
            }
        }
        tampered.push((c, &graph2));
    }

    let mut checked = 0usize;
    let mut branches: BTreeSet<String> = BTreeSet::new();
    let mut node_compare_ran = false;
    let mut edge_key_ran = false;
    for (chain, g) in &tampered {
        let verdict = explain::verify_chain(chain, g);
        for step in &verdict.steps {
            let Some(reason) = &step.reason else { continue };
            checked += 1;
            branches.insert(step.fact.clone());
            if step.fact == "node_exists" && reason.contains("chain claims") {
                node_compare_ran = true;
            }
            if reason.contains("no unique edge") || reason.contains("never reached") {
                edge_key_ran = true;
            }
            assert!(
                !reason.contains('{') && !reason.contains('}'),
                "reason for {} carries brace syntax, so it is a Debug dump: {reason}",
                step.fact
            );
            for field in [
                "node_id:",
                "qualified_name:",
                "from_id:",
                "file_path:",
                "start_line:",
                "edge_type:",
                "node_type:",
            ] {
                assert!(
                    !reason.contains(field),
                    "reason for {} leaks the struct field {field:?}: {reason}",
                    step.fact
                );
            }
        }
    }

    assert!(checked > 0, "no failing reason was produced, so this proved nothing");

    // Vacuity guards. Both offending branches print a *comparison*, so the
    // reason text is what proves the branch ran, not merely the fact tag:
    // several branches report themselves as `node_exists`, and only one of
    // them used to emit a struct dump.
    assert!(
        node_compare_ran,
        "the ExplainNode comparison branch never ran, so its reason was never checked"
    );
    assert!(
        edge_key_ran,
        "the EdgeKey reason branch never ran, so its reason was never checked"
    );
    assert!(
        branches.contains("node_exists"),
        "branches hit: {branches:?}"
    );
    println!("reason shape: {checked} failing reason(s) across {} branch kind(s)", branches.len());
}

/// A chain from an older wire format must come back as "I cannot read this
/// version", never as "this evidence does not hold up".
///
/// The two are different messages to send a client re-verifying an audit
/// report months later, and the `fact` to `kind` rename made them easy to
/// confuse: a v1 chain does not deserialize into a v2 `Fact` at all, so a
/// caller going straight to `serde_json::from_str` would surface "missing
/// field `kind`", which reads as a malformed or forged document.
#[tokio::test]
async fn an_older_chain_version_is_reported_as_unreadable_not_as_invalid() {
    let (db, graph) = rename_after_graph("version-gate").await;
    let _ = &db;

    // A v1 document, in the pre-rename shape: the discriminator is `fact`,
    // one level inside a field also called `fact`.
    let v1 = serde_json::json!({
        "explain_version": 1,
        "finding": "stale_reference",
        "subject": "helper",
        "steps": [
            {"index": 1, "fact": {"fact": "explicit_symbol_query", "name": "helper"}}
        ],
        "replay": []
    })
    .to_string();

    // Straight deserialization is exactly the trap: it fails, and the error
    // says nothing about versions.
    let raw = serde_json::from_str::<Chain>(&v1);
    assert!(raw.is_err(), "a v1 chain must not silently parse as v2");

    let verdict = explain::verify_chain_json(&v1, &graph);
    assert!(!verdict.ok);
    assert_eq!(
        verdict.unsupported_version,
        Some(1),
        "the verdict must name the version it could not read, got {verdict:?}"
    );
    assert!(
        verdict.steps.is_empty(),
        "an unreadable chain has no step findings to report: {:?}",
        verdict.steps
    );

    // A current chain still goes all the way through the JSON entry point.
    let chains =
        explain::explain_symbol(&graph, "version-gate", "helper", &Membership::ExplicitSymbol);
    let current = serde_json::to_string(first_of(&chains, FindingKind::StaleReference))
        .expect("serialize");
    let ok = explain::verify_chain_json(&current, &graph);
    assert!(ok.ok, "a current chain failed through verify_chain_json: {ok:?}");
    assert_eq!(ok.unsupported_version, None);

    // And the struct-level entry point gates on version too, for a chain
    // that parses but claims another format.
    let mut future = first_of(&chains, FindingKind::StaleReference).clone();
    future.explain_version = 99;
    let far = explain::verify_chain(&future, &graph);
    assert!(!far.ok);
    assert_eq!(far.unsupported_version, Some(99));

    // A genuinely malformed document at the right version is a document
    // failure, not a version one, and is distinguishable without string
    // matching: real steps are 1-based.
    let broken = r#"{"explain_version":2,"finding":"stale_reference","subject":"x"}"#;
    let v = explain::verify_chain_json(broken, &graph);
    assert!(!v.ok);
    assert_eq!(v.unsupported_version, None);
    assert_eq!(v.steps.len(), 1);
    assert_eq!(v.steps[0].index, 0);
}

/// Every chain this build emits carries the current version, so a stored
/// chain can always be matched against the build that made it.
#[tokio::test]
async fn emitted_chains_carry_the_current_explain_version() {
    let (db, graph) = rename_after_graph("version-stamp").await;
    let _ = &db;
    let chains =
        explain::explain_symbol(&graph, "version-stamp", "helper", &Membership::ExplicitSymbol);
    assert!(!chains.is_empty());
    for c in &chains {
        assert_eq!(c.explain_version, explain::EXPLAIN_VERSION);
    }
    assert_eq!(
        explain::EXPLAIN_VERSION,
        2,
        "the fact discriminator rename is a format break and must be version 2"
    );
}

/// **Item 3.** Two fresh indexes of the same tree must produce byte-identical
/// query output.
///
/// This is not hypothetical tidiness. Four fresh indexes of the polyglot
/// fixture were measured producing two different orderings, while six queries
/// against one store produced one, which locates the instability in
/// index-time row order leaking through the query rather than in the query
/// itself. An audit that diffs against its own previous run has to be able to
/// tell a real change from a reshuffle.
#[tokio::test]
async fn two_fresh_indexes_produce_byte_identical_query_output() {
    // `connect` is defined in both api/app.py and python/db/connection.py, so
    // it collides and emits one group per root: the exact case that reordered.
    const COLLIDING: &str = "connect";

    let mut rdeps_runs: Vec<String> = Vec::new();
    let mut hub_runs: Vec<String> = Vec::new();
    // Every other query surface that orders or limits its output.
    let mut other_runs: std::collections::BTreeMap<&str, Vec<String>> = Default::default();

    for i in 0..4 {
        // One isolated in-memory store per run, but the SAME project id.
        // Node ids hash (project_id, file_path, name, node_type, start_line)
        // (`index::node_id`), so varying the id would change every id and
        // this test would "detect" nondeterminism it had introduced itself.
        let _ = i;
        let db = fresh_db().await.expect("store");
        let pid = "determinism";
        index_fixture(&db, pid, "tests/fixtures/polyglot")
            .await
            .expect("index");

        let rdeps = codegraph::graph::dependencies::get_reverse_dependencies(
            &db,
            pid,
            COLLIDING,
            &codegraph::graph::NAME_EDGE_TYPES,
            3,
            true,
        )
        .await
        .expect("rdeps");
        rdeps_runs.push(serde_json::to_string(&rdeps).expect("json"));

        let hubs = codegraph::graph::hub_nodes::find_hub_nodes(&db, pid, 20)
            .await
            .expect("hubs");
        hub_runs.push(serde_json::to_string(&hubs).expect("json"));

        // Deliberately a small limit: the defect is not just tie ORDER, it
        // is that a tie straddling the truncate changes which rows come back
        // at all, and a limit larger than the result set could never show it.
        let coupling = codegraph::graph::coupling::calculate_file_coupling(&db, pid, 3)
            .await
            .expect("coupling");
        other_runs
            .entry("coupling")
            .or_default()
            .push(serde_json::to_string(&coupling).expect("json"));

        let circular = codegraph::graph::circular::detect_circular_deps(&db, pid)
            .await
            .expect("circular");
        other_runs
            .entry("circular")
            .or_default()
            .push(serde_json::to_string(&circular).expect("json"));

        let chain = codegraph::graph::call_chain::trace_calls(&db, pid, COLLIDING, 3, true)
            .await
            .expect("call_chain");
        other_runs
            .entry("call_chain")
            .or_default()
            .push(serde_json::to_string(&chain).expect("json"));

        let found = codegraph::graph::search::search_nodes(&db, pid, "n", None, 5)
            .await
            .expect("search");
        other_runs
            .entry("search")
            .or_default()
            .push(serde_json::to_string(&found).expect("json"));
    }

    for (name, runs) in &other_runs {
        let distinct: BTreeSet<&String> = runs.iter().collect();
        assert_eq!(
            distinct.len(),
            1,
            "4 fresh indexes produced {} distinct {name} orderings:\n{}",
            distinct.len(),
            runs.join("\n---\n")
        );
        let parsed: serde_json::Value = serde_json::from_str(&runs[0]).expect("parse");
        assert!(
            parsed.as_array().map(|a| a.len()).unwrap_or(0) > 0
                || parsed.get("groups").is_some(),
            "{name} returned nothing, so its determinism was not exercised"
        );
    }

    let distinct_rdeps: BTreeSet<&String> = rdeps_runs.iter().collect();
    assert_eq!(
        distinct_rdeps.len(),
        1,
        "4 fresh indexes produced {} distinct rdeps orderings:\n{}",
        distinct_rdeps.len(),
        rdeps_runs.join("\n---\n")
    );

    let distinct_hubs: BTreeSet<&String> = hub_runs.iter().collect();
    assert_eq!(
        distinct_hubs.len(),
        1,
        "4 fresh indexes produced {} distinct hub orderings",
        distinct_hubs.len()
    );

    // Guard against a vacuous pass: the query has to have returned something
    // with more than one orderable element for this to mean anything.
    let parsed: serde_json::Value = serde_json::from_str(&rdeps_runs[0]).expect("parse");
    assert!(
        parsed["groups"].as_array().map(|g| g.len()).unwrap_or(0) > 1,
        "{COLLIDING} stopped colliding, so this test no longer exercises group order"
    );
    let hubs: serde_json::Value = serde_json::from_str(&hub_runs[0]).expect("parse");
    assert!(
        hubs.as_array().map(|h| h.len()).unwrap_or(0) > 1,
        "the fixture produced fewer than two hubs"
    );
    println!(
        "determinism: 4 indexes agreed byte for byte on {} rdeps group(s) and {} hub(s)",
        parsed["groups"].as_array().unwrap().len(),
        hubs.as_array().unwrap().len()
    );
}

// ============================================================================
// Helpers
// ============================================================================

fn node(
    id: &str,
    name: &str,
    node_type: &str,
    file: &str,
    qn: &str,
) -> resolve::ResolverNode {
    resolve::ResolverNode {
        id: id.to_string(),
        name: name.to_string(),
        node_type: node_type.to_string(),
        language: "rust".to_string(),
        file_path: file.to_string(),
        qualified_name: qn.to_string(),
    }
}

fn edge(from_id: &str, to_name: &str, to_type: &str) -> resolve::UnresolvedEdge {
    resolve::UnresolvedEdge {
        from_id: from_id.to_string(),
        to_name: to_name.to_string(),
        to_type: to_type.to_string(),
        edge_type: "calls".to_string(),
    }
}

fn first_of(chains: &[Chain], kind: FindingKind) -> &Chain {
    chains
        .iter()
        .find(|c| c.finding == kind)
        .unwrap_or_else(|| panic!("no {} chain was produced", kind.as_tag()))
}

/// The `rename-refactor/after` fixture, which carries one real incomplete
/// rename: `target::helper` became `helper_v2`, and `stale_caller.rs` was
/// never updated.
async fn rename_after_graph(project_id: &str) -> (std::sync::Arc<surrealdb::Surreal<surrealdb::engine::any::Any>>, ExplainGraph) {
    let db = fresh_db().await.expect("store");
    index_fixture(&db, project_id, "tests/fixtures/rename-refactor/after")
        .await
        .expect("index");
    let graph = ExplainGraph::load(&db, project_id).await.expect("graph");
    (db, graph)
}

async fn index_at(
    db: &std::sync::Arc<surrealdb::Surreal<surrealdb::engine::any::Any>>,
    project_id: &str,
    root: &std::path::Path,
    force: bool,
) {
    let config = codegraph::index::IndexConfig {
        project_id: project_id.to_string(),
        root_path: root.to_path_buf(),
        tier: codegraph::index::IndexingTier::Balanced,
        languages: None,
        force,
    };
    codegraph::index::index_project(db, &config)
        .await
        .expect("index_project");
}

/// `(from_id, to_name) -> (resolved_by, attempted_rules, resolution_outcome)`
/// for every name-edge, which is the attribution both passes must agree on.
fn attribution_by_key(loaded: &Loaded) -> HashMap<(String, String), (String, Vec<String>, String)> {
    loaded
        .edges
        .iter()
        .filter(|e| e.edge_type == "calls" && !e.to_name.is_empty())
        .map(|e| {
            (
                (e.from_id.clone(), e.to_name.clone()),
                (
                    e.resolved_by.clone(),
                    e.attempted_rules.clone(),
                    e.resolution_outcome.clone(),
                ),
            )
        })
        .collect()
}
