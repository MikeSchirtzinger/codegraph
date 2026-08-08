//! Integration tests for the codegraph facade (`src/facade.rs`) — the
//! gate-facing API brevity's structural gate links against
//! (`specs/o-spine-step2-design.md` §3). Exercises `open_store` /
//! `impact_verdict` / `impact_verdict_for_paths` / `is_indexed` against a
//! real (file-backed) SurrealDB store, unlike `facade.rs`'s own unit tests
//! (which only exercise the pure `DependencyResult` → `StructuralVerdict`
//! projection with synthetic data, no DB).
//!
//! `mem://` can't be used here: each `connect("mem://")` call gets its own
//! *isolated* in-memory instance (confirmed by every other test file in
//! this suite holding one connection for its whole lifetime, never
//! reopening) — but a facade `Store` can only be constructed by
//! `open_store`, which opens its own fresh connection. So every test here
//! indexes into a real `surrealkv://` file first, then reopens that same
//! path via `facade::open_store` — mirroring how `codegraph index` and a
//! later `codegraph query` (or the gate) are really two separate processes
//! sharing one on-disk store. A short delay is required between the two
//! connections in-process (see `reopen_delay` below); real CLI usage never
//! needs this since each invocation is its own OS process and the file lock
//! is always gone by the time the next one starts.

mod common;

use std::sync::Arc;
use std::time::Duration;

use codegraph::facade::{self, VerdictTarget};
use common::index_fixture;

/// Dropping a `Surreal<Any>` handle releases the underlying surrealkv file
/// lock asynchronously, not synchronously on drop — reopening immediately
/// in the same process can otherwise race the lock's release with a
/// "database ... already locked" error. See module docs.
async fn reopen_delay() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Index `fixture_root` into a fresh on-disk surrealkv store under
/// `project_id`, then hand back its URL for a later `facade::open_store`.
/// The indexing connection is dropped (and its lock released) before this
/// returns.
async fn index_into_fresh_store(project_id: &str, fixture_root: &str) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.keep(); // outlive this fn — facade::open_store reopens it later
    let url = format!("surrealkv://{}/graph.db", path.display());
    {
        let db = surrealdb::engine::any::connect(&url).await.expect("connect for indexing");
        db.use_ns("codegraph").use_db("codegraph").await.expect("use ns/db for indexing");
        db.query(include_str!("../src/schema.surql"))
            .await
            .expect("schema DDL")
            .check()
            .expect("schema DDL check");
        index_fixture(&Arc::new(db), project_id, fixture_root).await.expect("index fixture");
    }
    reopen_delay().await;
    url
}

/// The URL of a freshly created, empty (never-indexed) surrealkv store —
/// for the `graph_empty` case. `open_store` itself runs the schema DDL, so
/// this deliberately does *not* pre-create anything.
fn empty_store_url() -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.keep();
    format!("surrealkv://{}/graph.db", path.display())
}

// ============================================================================
// 1. verdict on a known fixture
// ============================================================================

/// `impact_verdict` on a real symbol from the `rust` fixture must return a
/// non-skipped, populated verdict: `graph_empty` false, the queried symbol
/// present in `matched_symbols`, and (per the fixture's known shape) at
/// least the resolved reverse-dependents `deps`/`rdeps` already assert on
/// via `graph::dependencies` directly (`tests/fixtures_integration.rs`).
#[tokio::test]
async fn impact_verdict_on_known_fixture_returns_populated_verdict() {
    let project_id = "facade-rust-fixture";
    let url = index_into_fresh_store(project_id, "tests/fixtures/rust").await;

    let store = facade::open_store(&url, project_id).await.expect("open_store");
    assert!(facade::is_indexed(&store).await.expect("is_indexed"), "fixture must have indexed nodes");

    // helper() is called by caller_b in the rename-refactor-style rust
    // fixture set; pick a symbol known (from tests/fixtures/rust/expected.yaml)
    // to have at least one real caller so the verdict is non-trivially
    // populated, not just structurally valid.
    let manifest = common::load_manifest("tests/fixtures/rust/expected.yaml");
    let called_case = manifest
        .edges
        .iter()
        .find(|e| e.edge_type == "calls" && e.confidence == "RESOLVED")
        .expect("fixture manifest has at least one RESOLVED calls edge to verify against");
    let target_bare = called_case.to_name.rsplit("::").next().unwrap_or(&called_case.to_name);

    let verdict = facade::impact_verdict(&store, target_bare).await.expect("impact_verdict");

    assert!(!verdict.graph_empty, "an indexed project must not report graph_empty");
    match verdict.target {
        VerdictTarget::Symbol(ref s) => assert_eq!(s, target_bare),
        VerdictTarget::Paths(_) => panic!("expected Symbol target"),
    }
    assert!(
        !verdict.matched_symbols.is_empty(),
        "querying a symbol the manifest confirms exists must match at least one root"
    );
    assert!(
        verdict.resolved_dependent_count() >= 1,
        "the manifest's own RESOLVED calls edge means this symbol has ≥1 real caller, got {verdict:?}"
    );
}

// ============================================================================
// 2. graph_empty on an empty store
// ============================================================================

/// A store that was never indexed (schema created by `open_store` itself,
/// zero `code_node` rows) must report `is_indexed() == false` and every
/// verdict's `graph_empty == true` — the ONLY graph-state that maps to the
/// gate's `SkippedNoGraph`, never conflated with "matched nothing".
#[tokio::test]
async fn empty_store_reports_graph_empty() {
    let project_id = "facade-empty-store";
    let url = empty_store_url();

    let store = facade::open_store(&url, project_id).await.expect("open_store on a fresh store");
    assert!(!facade::is_indexed(&store).await.expect("is_indexed"), "a never-indexed store must not be indexed");

    let verdict = facade::impact_verdict(&store, "anything").await.expect("impact_verdict on empty store");
    assert!(verdict.graph_empty);
    assert!(verdict.matched_symbols.is_empty());
    assert!(verdict.is_clean(), "an empty graph has no stale/ambiguous signal to reject on");

    let paths_verdict = facade::impact_verdict_for_paths(&store, &["src/anything.rs".to_string()])
        .await
        .expect("impact_verdict_for_paths on empty store");
    assert!(paths_verdict.graph_empty);
}

// ============================================================================
// 3. stale_references populated after a simulated rename
// ============================================================================

/// Reuses the rename-refactor fixture pair (`tests/fixtures/rename-refactor`,
/// the same one `tests/kill_test.rs` uses at the `graph::dependencies`
/// layer): index the `after/` tree, where one caller (`stale_caller.rs`)
/// still references the pre-rename name. `impact_verdict` on the OLD name
/// must surface it via `stale_references` — the facade's projection of
/// exactly the signal `tests/kill_test.rs` already proves at the query
/// layer, now proven through the gate-facing facade instead.
#[tokio::test]
async fn impact_verdict_surfaces_stale_reference_after_rename() {
    let parent = common::load_rename_manifest("tests/fixtures/rename-refactor/expected.yaml");
    assert!(!parent.kill_test.callers.is_empty(), "kill-test manifest has no callers to check — fixture regression");

    let project_id = "facade-rename-after";
    let url = index_into_fresh_store(project_id, "tests/fixtures/rename-refactor/after").await;
    let store = facade::open_store(&url, project_id).await.expect("open_store");

    let old_name_bare = parent.kill_test.qualified_name_before.rsplit("::").next().unwrap();
    let verdict = facade::impact_verdict(&store, old_name_bare).await.expect("impact_verdict on old name");

    assert!(!verdict.graph_empty);
    assert!(
        verdict.matched_symbols.is_empty(),
        "no live symbol should answer to the renamed-away name, got {:?}",
        verdict.matched_symbols
    );
    assert!(!verdict.is_clean(), "a populated stale_references list must make the verdict dirty");
    assert!(
        verdict.stale_references.iter().any(|r| r.from_name == "use_stale"),
        "kill-test regression: stale caller must surface via the facade's stale_references, got {:?}",
        verdict.stale_references
    );
    assert_eq!(verdict.ambiguous_refusals.len(), 0);

    // No false positives: the correctly-migrated new name must come back
    // clean, matching tests/kill_test.rs's own assertion at the query layer.
    let new_name_bare = parent.kill_test.qualified_name_after.rsplit("::").next().unwrap();
    let clean_verdict = facade::impact_verdict(&store, new_name_bare).await.expect("impact_verdict on new name");
    assert!(clean_verdict.is_clean());
    assert!(clean_verdict.stale_references.is_empty());
}
