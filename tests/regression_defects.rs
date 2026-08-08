//! R3b, item 2: D1-D4 regression tests. Fixture-based and deterministic —
//! unlike the broad manifest sweep in `fixtures_integration.rs` (which
//! checks the resolver's DB row state directly via `common::assert_edges`/
//! `assert_cycles`), these specifically drive the QUERY layer
//! (`call_chain`/`dependencies`/`circular` — the functions backing the
//! `calls`/`deps`/`rdeps`/`circular` CLI commands and MCP tools) since each
//! defect's original failure mode, per `specs/resolution-layer-v1.md`
//! ("Verified defects"), was observed there, not in a raw DB row. A
//! regression here means the user-visible symptom came back even if the
//! underlying resolver row still looks fine.
//!
//! Each case is looked up by its manifest `defect` tag (`D1`..`D4`), not by
//! the `case` id letter — the two are 1:1 in every fixture today, but
//! keying off `defect` is the more direct, self-documenting link to the
//! thing being regression-tested, and (for D1/D2/D4) it lets `polyglot`
//! participate for free despite its different case-lettering convention
//! (`a-go`/`c-cross-language`).

mod common;

use codegraph::graph::call_chain::trace_calls;
use codegraph::graph::circular::detect_circular_deps;
use codegraph::graph::dependencies::{get_dependencies, get_reverse_dependencies};

use common::{fresh_db, index_fixture, load_manifest, Loaded};

/// Every language fixture, D1/D2/D4 loop over all of these (defect-tag
/// lookup makes polyglot's different case-lettering irrelevant here).
const ALL_LANGUAGE_FIXTURES: [&str; 7] =
    ["rust", "typescript", "python", "go", "java", "c-cpp", "polyglot"];

/// D3's collision case is a genuinely different shape in `polyglot`
/// (case `c-cross-language` is RESOLVED — proving a cross-language collision
/// must NOT go ambiguous — the opposite of the single-language AMBIGUOUS
/// case this test targets), so it's scoped to the six single-language
/// fixtures; polyglot's case is still checked, generically, by
/// `fixtures_integration.rs`'s manifest sweep.
const SINGLE_LANGUAGE_FIXTURES: [&str; 6] = ["rust", "typescript", "python", "go", "java", "c-cpp"];

fn manifest_path(fixture: &str) -> String {
    format!("tests/fixtures/{fixture}/expected.yaml")
}

/// D1: `query --kind calls` returned 0 for real functions — every extractor
/// emitted `calls` via a name-edge with `to_id=""`; only Rust macro
/// invocations got a real id. Regression guard: the resolved `calls` query
/// (`call_chain::trace_calls`) must surface the defect's planted, real,
/// same-file callee for every language.
#[tokio::test]
async fn d1_calls_query_returns_real_callees() {
    for fixture in ALL_LANGUAGE_FIXTURES {
        let manifest = load_manifest(&manifest_path(fixture));
        let case = manifest
            .edges
            .iter()
            .find(|e| e.defect.as_deref() == Some("D1"))
            .unwrap_or_else(|| panic!("{fixture}: manifest has no D1-tagged edge"));

        let db = fresh_db().await.expect("fresh store");
        let project_id = format!("d1-{fixture}");
        index_fixture(&db, &project_id, &manifest.root).await.expect("index fixture");
        let loaded = Loaded::fetch(&db, &project_id).await.expect("load indexed data");

        let caller = loaded
            .by_qualified_name(&case.from)
            .unwrap_or_else(|| panic!("{fixture}: caller {:?} not indexed", case.from));

        let result = trace_calls(&db, &project_id, &caller.name, 2, false)
            .await
            .expect("trace_calls");
        assert!(
            !result.groups.is_empty(),
            "{fixture}: D1 regression — calls query found no group for caller {:?}",
            caller.name
        );
        let found = result
            .groups
            .iter()
            .any(|g| g.entries.iter().any(|e| e.callee_name == case.to_name));
        assert!(
            found,
            "{fixture}: D1 regression — calls query on {:?} did not return real callee {:?} (groups: {result:?})",
            caller.name, case.to_name
        );
    }
}

/// D2: qualified calls (e.g. `db::connect`) never resolved in `deps`/`rdeps`
/// — callee text was stored verbatim and never matched to a node id (`deps
/// --name main` returned 0). Regression guard: `dependencies::get_dependencies`
/// must walk the defect's planted qualified call through to its real,
/// possibly-cross-file target for every language.
#[tokio::test]
async fn d2_qualified_calls_resolve_via_deps_query() {
    for fixture in ALL_LANGUAGE_FIXTURES {
        let manifest = load_manifest(&manifest_path(fixture));
        let case = manifest
            .edges
            .iter()
            .find(|e| e.defect.as_deref() == Some("D2"))
            .unwrap_or_else(|| panic!("{fixture}: manifest has no D2-tagged edge"));
        let target_qn = case
            .target
            .clone()
            .unwrap_or_else(|| panic!("{fixture}: D2 case must be RESOLVED with a target"));

        let db = fresh_db().await.expect("fresh store");
        let project_id = format!("d2-{fixture}");
        index_fixture(&db, &project_id, &manifest.root).await.expect("index fixture");
        let loaded = Loaded::fetch(&db, &project_id).await.expect("load indexed data");

        let caller = loaded
            .by_qualified_name(&case.from)
            .unwrap_or_else(|| panic!("{fixture}: caller {:?} not indexed", case.from));

        let result = get_dependencies(&db, &project_id, &caller.name, &["calls"], 2, false)
            .await
            .expect("get_dependencies");

        let found = result.groups.iter().any(|g| {
            g.items
                .iter()
                .any(|item| loaded.by_node_id(&item.node_id).map(|n| n.qualified_name.as_str()) == Some(target_qn.as_str()))
        });
        assert!(
            found,
            "{fixture}: D2 regression — deps query on {:?} did not resolve qualified call to {target_qn:?} (groups: {result:?})",
            caller.name
        );
    }
}

/// D3: no module/file scoping meant a project-wide bare-name collision got
/// blended (`rdeps --name walk_calls` blended 11 hits from 6 unrelated
/// extractor files). Regression guard: `dependencies::get_reverse_dependencies`
/// must return one independent group per colliding symbol — never a blend —
/// for the defect's planted collision, in every single-language fixture.
#[tokio::test]
async fn d3_collision_groups_per_symbol_never_blended() {
    for fixture in SINGLE_LANGUAGE_FIXTURES {
        let manifest = load_manifest(&manifest_path(fixture));
        let case = manifest
            .edges
            .iter()
            .find(|e| e.defect.as_deref() == Some("D3"))
            .unwrap_or_else(|| panic!("{fixture}: manifest has no D3-tagged edge"));
        let expected_candidates = case
            .candidates
            .clone()
            .unwrap_or_else(|| panic!("{fixture}: D3 case must be AMBIGUOUS with candidates"));
        assert!(
            expected_candidates.len() >= 2,
            "{fixture}: D3 case must have 2+ candidates to prove non-blending"
        );

        let db = fresh_db().await.expect("fresh store");
        let project_id = format!("d3-{fixture}");
        index_fixture(&db, &project_id, &manifest.root).await.expect("index fixture");

        let result = get_reverse_dependencies(&db, &project_id, &case.to_name, &["calls"], 2, false)
            .await
            .expect("get_reverse_dependencies");

        assert_eq!(
            result.groups.len(),
            expected_candidates.len(),
            "{fixture}: D3 regression — collision on {:?} produced {} group(s), want one per symbol ({})",
            case.to_name, result.groups.len(), expected_candidates.len()
        );
        assert!(
            result.name_ambiguous,
            "{fixture}: D3 regression — collision on {:?} not flagged name_ambiguous",
            case.to_name
        );
        for g in &result.groups {
            assert!(
                expected_candidates.contains(&g.root_qualified_name),
                "{fixture}: unexpected group root {:?}, want one of {:?}",
                g.root_qualified_name, expected_candidates
            );
        }
    }
}

/// D4: `circular` was structurally inert cross-file — `calls`' `to_id` was
/// always empty pre-resolver, no extractor ever emitted `references`, and
/// `imports` was Rust-only and same-file by construction (0 cycles on
/// self-index despite real mutual recursion). Regression guard: the
/// `circular` query (`circular::detect_circular_deps`, backed by resolved
/// name-edges + the derived `file_ref` graph) must detect the planted
/// cross-file cycle in every language fixture, including polyglot.
#[tokio::test]
async fn d4_cross_file_cycle_detected_per_language() {
    for fixture in ALL_LANGUAGE_FIXTURES {
        let manifest = load_manifest(&manifest_path(fixture));
        assert!(
            !manifest.cycles.is_empty(),
            "{fixture}: manifest has no planted cycle — fixture regression, not a resolver one"
        );

        let db = fresh_db().await.expect("fresh store");
        let project_id = format!("d4-{fixture}");
        index_fixture(&db, &project_id, &manifest.root).await.expect("index fixture");

        let detected = detect_circular_deps(&db, &project_id).await.expect("detect_circular_deps");
        for expect in &manifest.cycles {
            let want: std::collections::HashSet<&str> = expect.files.iter().map(String::as_str).collect();
            let hit = detected.iter().any(|c| {
                let got: std::collections::HashSet<&str> =
                    [c.file_a.as_str(), c.file_b.as_str()].into_iter().collect();
                got == want
            });
            assert!(
                hit,
                "{fixture}: D4 regression — planted cycle {:?} not detected by circular query; got {:?}",
                expect.files, detected
            );
        }
    }
}
