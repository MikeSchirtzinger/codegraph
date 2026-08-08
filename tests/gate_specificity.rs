//! The gate's **specificity** half: a clean tree must produce a clean
//! verdict for every single file in it.
//!
//! Every existing gate test is a *sensitivity* test — break a rename, prove
//! the gate notices (`tests/kill_test.rs`, `tests/deletion_tracking.rs`).
//! Nothing asserted the converse, and that gap shipped a real false
//! positive: `impact_verdict_for_paths` rejected an **unmodified**
//! `src/graph/dependencies.rs` with 38 stale references on a self-index of
//! this repo, 37 of them the tree-sitter method call `cursor.node()`
//! bare-tailing onto that module's own `fn node()` test helper. A gate that
//! rejects a tree nobody touched is worse than no gate: it trains its
//! operator to ignore it.
//!
//! The fixture below is a small tree that is *correct by construction* —
//! every definition live, every caller pointing at a definition that
//! exists — while deliberately containing each capture shape that produced
//! the false positive:
//!   1. `cursor.node()` — an external method call whose receiver chain
//!      normalizes to `cursor::node`, bare-tailing onto a live `fn node()`
//!      in another file (stale reference #1-37's shape).
//!   2. `fixture_crate::api::get_reverse_dependencies()` — a call that
//!      spells the crate name out, so the capture is a strict *superset* of
//!      the live symbol's crate-relative `qualified_name` (#38's shape).
//!   3. `Lcg { .. }` — a struct literal captured as `to_type = "function"`,
//!      so the cascade's candidate pool never held the live struct.
//!   4. a wrapped `items\n.iter()\n.find(..)` builder chain, whose whole
//!      multi-line source text used to be stored as one `to_name` whose
//!      bare tail is `find` — a live symbol here.
//!
//! Structure: same self-contained shape as `tests/deletion_tracking.rs`
//! (own tempdir fixtures, file-backed `surrealkv://` stores because
//! `facade::open_store` opens its own connection — see
//! `tests/facade_integration.rs`'s module docs on why `mem://` can't cross
//! that boundary, and on the reopen delay).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codegraph::facade;
use codegraph::index::{self, IndexConfig, IndexResult, IndexingTier};

// ============================================================================
// Harness (mirrors tests/deletion_tracking.rs)
// ============================================================================

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(&path, content).expect("write fixture file");
}

async fn reopen_delay() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

fn fresh_store_url() -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.keep();
    format!("surrealkv://{}/graph.db", path.display())
}

async fn index_at(url: &str, project_id: &str, root: &Path, force: bool) -> IndexResult {
    let result = {
        let db = surrealdb::engine::any::connect(url).await.expect("connect for indexing");
        db.use_ns("codegraph").use_db("codegraph").await.expect("use ns/db");
        db.query(include_str!("../src/schema.surql"))
            .await
            .expect("schema DDL")
            .check()
            .expect("schema DDL check");
        let config = IndexConfig {
            project_id: project_id.to_string(),
            root_path: root.to_path_buf(),
            tier: IndexingTier::Full,
            languages: None,
            force,
        };
        index::index_project(&Arc::new(db), &config).await.expect("index_project")
    };
    reopen_delay().await;
    result
}

/// One gate-shaped query, exactly as brevity's structural gate performs it.
async fn paths_verdict(url: &str, project_id: &str, paths: &[&str]) -> facade::StructuralVerdict {
    let verdict = {
        let store = facade::open_store(url, project_id).await.expect("open_store");
        facade::impact_verdict_for_paths(&store, &paths.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .await
            .expect("impact_verdict_for_paths")
    };
    reopen_delay().await;
    verdict
}

// ============================================================================
// The fixture — correct by construction, adversarial by capture shape
// ============================================================================

/// Every file in the fixture tree, in index-relative form (the shape
/// `impact_verdict_for_paths` takes).
const FILES: &[&str] = &[
    "src/lib.rs",
    "src/walker.rs",
    "src/helpers.rs",
    "src/api.rs",
    "src/consumer.rs",
    "src/collect.rs",
];

fn write_clean_tree(root: &Path) {
    write_file(
        root,
        "src/lib.rs",
        "pub mod api;\npub mod collect;\npub mod consumer;\npub mod helpers;\npub mod walker;\n",
    );

    // Shape 1: `cursor.node()` — an external (tree-sitter) method call whose
    // receiver chain normalizes to the qualified `cursor::node`, bare-tailing
    // onto `helpers::node` below. Nothing here is stale: `helpers::node` is
    // live and this call never referred to it.
    write_file(
        root,
        "src/walker.rs",
        "use tree_sitter::TreeCursor;\n\n\
         pub fn walk_calls(cursor: &mut TreeCursor) -> usize {\n\
         \x20   let node = cursor.node();\n\
         \x20   node.child_count()\n\
         }\n",
    );

    // The live symbol whose bare name shape 1 collides with, plus a genuine
    // (RESOLVED) caller of it so the file is not inert.
    write_file(
        root,
        "src/helpers.rs",
        "pub fn node() -> usize {\n    7\n}\n\npub fn use_node() -> usize {\n    node() + 1\n}\n",
    );

    write_file(
        root,
        "src/api.rs",
        "pub fn get_reverse_dependencies() -> usize {\n    3\n}\n",
    );

    // Shape 2: the call spells the crate name out, so the capture
    // (`fixture_crate::api::get_reverse_dependencies`) is a superset of the
    // target's crate-relative qualified_name (`api::get_reverse_dependencies`)
    // and R1/R2 never match it — unresolved, but pointing at a live symbol.
    write_file(
        root,
        "src/consumer.rs",
        "pub fn call_it() -> usize {\n    fixture_crate::api::get_reverse_dependencies()\n}\n",
    );

    // Shape 3 (`Lcg { .. }` — struct literal captured as a function) and
    // shape 4 (a wrapped builder chain whose bare tail is the live `find`).
    write_file(
        root,
        "src/collect.rs",
        "pub struct Lcg {\n\
         \x20   pub state: u64,\n\
         }\n\n\
         pub fn find() -> u64 {\n\
         \x20   3\n\
         }\n\n\
         pub fn seeded() -> Lcg {\n\
         \x20   Lcg { state: find() }\n\
         }\n\n\
         pub fn scan(items: &[u64]) -> Option<&u64> {\n\
         \x20   items\n\
         \x20       .iter()\n\
         \x20       .find(|x| **x > 1)\n\
         }\n",
    );
}

// ============================================================================
// Tests
// ============================================================================

/// THE specificity assertion: index a tree in which nothing is broken, then
/// ask the gate about every file in it, one at a time and all at once.
/// Zero stale references, everywhere.
///
/// `is_clean()` (stale **and** ambiguous both empty) is asserted too, which
/// this fixture can satisfy because it plants no name collision. That is a
/// property of the fixture, not of the fix: a real tree with two symbols
/// legitimately sharing a bare name reports AMBIGUOUS refusals, and those
/// are truthful — the resolver genuinely cannot tell which one a bare call
/// meant. Self-indexing this repo is exactly that case (its own
/// `tests/fixtures/**` plant `helper`/`connect`/`get_str` collisions on
/// purpose), so the stale-reference count is the part of the verdict that
/// must be zero for every file there.
#[tokio::test]
async fn clean_tree_yields_a_clean_verdict_for_every_file() {
    let project_id = "spec-clean-tree";
    let url = fresh_store_url();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_clean_tree(root);
    let indexed = index_at(&url, project_id, root, true).await;
    assert_eq!(
        indexed.files_indexed,
        FILES.len(),
        "fixture regression — every file must actually index, got {indexed:?}"
    );

    for file in FILES {
        let verdict = paths_verdict(&url, project_id, &[file]).await;
        assert!(!verdict.graph_empty, "{file}: the store is indexed");
        assert!(
            verdict.stale_references.is_empty(),
            "{file}: an untouched file in a correct tree reported stale references: {:?}",
            verdict.stale_references
        );
        assert!(
            verdict.is_clean(),
            "{file}: verdict not clean on a correct tree: {verdict:?}"
        );
    }

    // …and the whole plan at once, which is the shape the gate actually
    // passes (a plan touches several files), with every name unioned.
    let all = paths_verdict(&url, project_id, FILES).await;
    assert!(
        all.is_clean(),
        "a plan touching the entire clean tree must pass, got {all:?}"
    );
    assert!(
        !all.matched_symbols.is_empty(),
        "the verdict must be a real one over live symbols, not an empty-target skip"
    );
}

/// The converse, on the same fixture: break exactly one thing and the gate
/// must reject. Paired with the test above this is the actual contract —
/// specific *and* sensitive on the same tree, so neither property can be
/// bought by weakening the other (a scan that always returns nothing would
/// pass the first test alone).
#[tokio::test]
async fn same_tree_still_rejects_an_incomplete_rename() {
    let project_id = "spec-sensitivity";
    let url = fresh_store_url();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_clean_tree(root);
    index_at(&url, project_id, root, true).await;
    assert!(
        paths_verdict(&url, project_id, &["src/helpers.rs"]).await.is_clean(),
        "precondition: the tree starts clean"
    );

    // Rename `node` away and forget its caller — `use_node` still calls it.
    write_file(
        root,
        "src/helpers.rs",
        "pub fn node_v2() -> usize {\n    7\n}\n\npub fn use_node() -> usize {\n    node() + 1\n}\n",
    );
    index_at(&url, project_id, root, false).await;

    let verdict = paths_verdict(&url, project_id, &["src/helpers.rs"]).await;
    assert!(
        !verdict.is_clean(),
        "an incomplete rename must still reject after the specificity fix, got {verdict:?}"
    );
    assert!(
        verdict
            .stale_references
            .iter()
            .any(|r| r.from_name == "use_node" && r.to_name == "node"),
        "the forgotten caller must surface by name, got {:?}",
        verdict.stale_references
    );
}

/// A *completed* rename stays clean even when the vanished name's bare tail
/// also appears inside a wrapped builder chain elsewhere in the tree —
/// `items\n.iter()\n.find(..)` in `src/collect.rs`.
///
/// This is the capture-filter half of the fix, and the one shape the
/// stale-scan rule alone cannot save: renaming `find` away leaves no live
/// definition to compare against, so the scan (correctly) falls back to the
/// bare tail — and before multi-line captures were dropped at extraction,
/// that junk `to_name`'s tail *was* `find`, rejecting a rename with every
/// caller correctly updated.
#[tokio::test]
async fn completed_rename_stays_clean_despite_a_multiline_receiver_capture() {
    let project_id = "spec-multiline-capture";
    let url = fresh_store_url();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_clean_tree(root);
    index_at(&url, project_id, root, true).await;

    // Rename `find` -> `find_v2` AND update its only caller (`seeded`) in the
    // same pass: a complete, correct refactor. `scan`'s wrapped
    // `.iter().find(..)` chain is untouched and is not a reference to it.
    write_file(
        root,
        "src/collect.rs",
        "pub struct Lcg {\n\
         \x20   pub state: u64,\n\
         }\n\n\
         pub fn find_v2() -> u64 {\n\
         \x20   3\n\
         }\n\n\
         pub fn seeded() -> Lcg {\n\
         \x20   Lcg { state: find_v2() }\n\
         }\n\n\
         pub fn scan(items: &[u64]) -> Option<&u64> {\n\
         \x20   items\n\
         \x20       .iter()\n\
         \x20       .find(|x| **x > 1)\n\
         }\n",
    );
    index_at(&url, project_id, root, false).await;

    let verdict = paths_verdict(&url, project_id, &["src/collect.rs"]).await;
    assert!(
        verdict.is_clean(),
        "a completed rename must pass — the multi-line `.find` chain is not a reference to it, got {verdict:?}"
    );
}
