//! Deletion-tracking integration tests — the killtest-6 closure
//! (`specs/o-spine-step2-design.md` §10, candidate fix 1: "deletion tracking
//! at incremental re-index").
//!
//! The blind spot being closed: `impact_verdict_for_paths` resolves touched
//! files to the symbols *currently defined* in them, and an incomplete
//! rename removes the old name from that set by definition — so the gate's
//! paths entry could never query the one name whose stale callers matter.
//! The fix records disappearing definitions in `deleted_symbol` at
//! incremental re-index and unions those names back into the paths query.
//!
//! Self-contained like `tests/incremental_reresolution.rs` (own tiny inline
//! fixtures in tempdirs, no `tests/common` dependency): capture semantics
//! run on fast isolated `mem://` instances; the facade end-to-end tests use
//! file-backed `surrealkv://` stores because `facade::open_store` opens its
//! own connection (see `tests/facade_integration.rs` module docs on why
//! `mem://` can't cross that boundary, and on the reopen delay).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::facade;
use codegraph::index::{self, IndexConfig, IndexResult, IndexingTier};

// ============================================================================
// Harness
// ============================================================================

async fn mem_db() -> Arc<Surreal<Any>> {
    let db = surrealdb::engine::any::connect("mem://")
        .await
        .expect("connect to mem://");
    db.use_ns("codegraph_test")
        .use_db("codegraph_test")
        .await
        .expect("use_ns/use_db");
    db.query(include_str!("../src/schema.surql"))
        .await
        .expect("schema DDL query")
        .check()
        .expect("schema DDL validation");
    Arc::new(db)
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(&path, content).expect("write fixture file");
}

async fn run_index(db: &Arc<Surreal<Any>>, project_id: &str, root: &Path, force: bool) -> IndexResult {
    let config = IndexConfig {
        project_id: project_id.to_string(),
        root_path: root.to_path_buf(),
        tier: IndexingTier::Balanced,
        languages: None,
        force,
    };
    index::index_project(db, &config).await.expect("index_project")
}

#[derive(Debug, Clone, PartialEq)]
struct DeletedRow {
    name: String,
    qualified_name: String,
    node_type: String,
    file_path: String,
}

async fn load_deleted_symbols(db: &Surreal<Any>, project_id: &str) -> Vec<DeletedRow> {
    let mut resp = db
        .query(
            "SELECT name, qualified_name, node_type, file_path FROM deleted_symbol \
             WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .expect("query deleted_symbol");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take deleted_symbol rows");

    let mut out: Vec<DeletedRow> = rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(o) = v else {
                return None;
            };
            let get = |k: &str| {
                o.get(k)
                    .and_then(|v| match v {
                        surrealdb_types::Value::String(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default()
            };
            Some(DeletedRow {
                name: get("name"),
                qualified_name: get("qualified_name"),
                node_type: get("node_type"),
                file_path: get("file_path"),
            })
        })
        .collect();
    out.sort_by(|a, b| (&a.qualified_name, &a.file_path).cmp(&(&b.qualified_name, &b.file_path)));
    out
}

async fn count_nodes_of_type(db: &Surreal<Any>, project_id: &str, node_type: &str) -> usize {
    let mut resp = db
        .query("SELECT name FROM code_node WHERE project_id = $pid AND node_type = $nt")
        .bind(("pid", project_id.to_string()))
        .bind(("nt", node_type.to_string()))
        .await
        .expect("query nodes by type");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take nodes");
    rows.len()
}

// ============================================================================
// Capture semantics (mem://, fast)
// ============================================================================

/// The core lifecycle: a rename records exactly the renamed-away definition;
/// restoring it heals the row (a re-added symbol is no longer deleted).
#[tokio::test]
async fn rename_records_deletion_and_restore_heals() {
    let db = mem_db().await;
    let project_id = "del-rename-heal";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    write_file(root, "src/caller.rs", "pub fn call_it() {\n    foo();\n}\n");
    run_index(&db, project_id, root, true).await;
    assert!(
        load_deleted_symbols(&db, project_id).await.is_empty(),
        "a fresh index has observed no deletions"
    );

    write_file(root, "src/target.rs", "pub fn foo_renamed() {}\n");
    run_index(&db, project_id, root, false).await;

    let rows = load_deleted_symbols(&db, project_id).await;
    assert_eq!(rows.len(), 1, "exactly the renamed-away definition, got {rows:?}");
    assert_eq!(rows[0].name, "foo");
    assert!(
        rows[0].qualified_name.ends_with("foo"),
        "qualified_name must be foo's, got {:?}",
        rows[0].qualified_name
    );
    assert_eq!(rows[0].node_type, "function");
    assert_eq!(rows[0].file_path, "src/target.rs");

    // Restore: the same (qualified_name, file_path) is defined again → foo's
    // row heals. Symmetrically, the restore just deleted foo_renamed — the
    // capture must record that with the same diligence it recorded foo.
    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    run_index(&db, project_id, root, false).await;
    let rows = load_deleted_symbols(&db, project_id).await;
    assert!(
        rows.iter().all(|r| r.name != "foo"),
        "a re-added definition must heal its deletion row, got {rows:?}"
    );
    assert_eq!(
        rows.len(),
        1,
        "renaming back deletes the interim name — one new fact, got {rows:?}"
    );
    assert_eq!(rows[0].name, "foo_renamed");

    // Define both names → nothing is deleted anymore → table fully drains.
    write_file(root, "src/target.rs", "pub fn foo() {}\npub fn foo_renamed() {}\n");
    run_index(&db, project_id, root, false).await;
    assert!(
        load_deleted_symbols(&db, project_id).await.is_empty(),
        "every recorded name is defined again — all rows must heal"
    );
}

/// A whole-file delete records every definition the file had — but never its
/// `import` nodes: an import statement is a reference, not a definition
/// (same exclusion the facade applies to touched symbols).
#[tokio::test]
async fn whole_file_delete_records_definitions_but_never_imports() {
    let db = mem_db().await;
    let project_id = "del-whole-file";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(
        root,
        "src/doomed.rs",
        "use std::collections::HashMap;\n\npub fn doomed_fn() {\n    let _m: HashMap<u8, u8> = HashMap::new();\n}\n\npub fn also_doomed() {}\n",
    );
    write_file(root, "src/keeper.rs", "pub fn keeper() {\n    doomed_fn();\n}\n");
    run_index(&db, project_id, root, true).await;
    assert!(
        count_nodes_of_type(&db, project_id, "import").await >= 1,
        "fixture must actually contain an import node for the exclusion to be meaningful"
    );

    std::fs::remove_file(root.join("src/doomed.rs")).expect("rm doomed.rs");
    run_index(&db, project_id, root, false).await;

    let rows = load_deleted_symbols(&db, project_id).await;
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"doomed_fn") && names.contains(&"also_doomed"),
        "both definitions must be recorded, got {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.node_type != "import"),
        "import nodes are references, not definitions — must never be recorded, got {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.file_path == "src/doomed.rs"),
        "every row must carry the deleted file's path, got {rows:?}"
    );
}

/// Changes that don't remove a definition record nothing: a body-only edit
/// (same qualified names) and a no-op re-index must both leave the table
/// empty. This is the capture-side half of "zero false positives".
#[tokio::test]
async fn body_edit_and_noop_reindex_record_nothing() {
    let db = mem_db().await;
    let project_id = "del-noop";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/lib_code.rs", "pub fn stable() {\n    helper_call();\n}\npub fn helper_call() {}\n");
    run_index(&db, project_id, root, true).await;

    // Body edit: hash changes, definitions don't.
    write_file(
        root,
        "src/lib_code.rs",
        "pub fn stable() {\n    // a comment\n    helper_call();\n}\npub fn helper_call() {}\n",
    );
    let r = run_index(&db, project_id, root, false).await;
    assert_eq!(r.files_indexed, 1, "the edited file must actually re-index");
    assert!(
        load_deleted_symbols(&db, project_id).await.is_empty(),
        "a body-only edit removes no definition"
    );

    // No-op: nothing changed at all.
    let r = run_index(&db, project_id, root, false).await;
    assert_eq!(r.files_indexed, 0);
    assert!(load_deleted_symbols(&db, project_id).await.is_empty());
}

/// `--force` wipes deletion history: a force run skips `detect_changes`
/// entirely and re-baselines "what exists" — the design doc §10's documented
/// graceful degradation, and the operator's escape hatch.
#[tokio::test]
async fn force_reindex_wipes_deletion_history() {
    let db = mem_db().await;
    let project_id = "del-force-wipe";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    run_index(&db, project_id, root, true).await;
    write_file(root, "src/target.rs", "pub fn foo_renamed() {}\n");
    run_index(&db, project_id, root, false).await;
    assert_eq!(load_deleted_symbols(&db, project_id).await.len(), 1);

    run_index(&db, project_id, root, true).await;
    assert!(
        load_deleted_symbols(&db, project_id).await.is_empty(),
        "--force must reset deletion history"
    );
}

/// Healing is keyed on (qualified_name, file_path), not qualified_name
/// alone: crate-root files (`src/lib.rs`, `src/main.rs`) legally produce
/// colliding bare qualified names, and re-indexing one file must never erase
/// the still-true deletion fact belonging to another.
#[tokio::test]
async fn heal_is_scoped_per_file_under_qualified_name_collision() {
    let db = mem_db().await;
    let project_id = "del-collision-heal";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Both crate roots: module_path("src/lib.rs") == module_path("src/main.rs") == ""
    // → both `shared` definitions carry the bare qualified name "shared".
    write_file(root, "src/lib.rs", "pub fn shared() {}\n");
    write_file(root, "src/main.rs", "pub fn shared() {}\npub fn main_only() {}\n");
    run_index(&db, project_id, root, true).await;

    // Delete main.rs's copy.
    write_file(root, "src/main.rs", "pub fn main_only() {}\n");
    run_index(&db, project_id, root, false).await;
    let rows = load_deleted_symbols(&db, project_id).await;
    assert_eq!(rows.len(), 1, "main.rs's shared() was deleted, got {rows:?}");
    assert_eq!((rows[0].name.as_str(), rows[0].file_path.as_str()), ("shared", "src/main.rs"));

    // Touch lib.rs (body edit; it still defines shared with the SAME bare
    // qualified name). A heal keyed on qualified_name alone would wrongly
    // erase main.rs's row here.
    write_file(root, "src/lib.rs", "pub fn shared() {\n    // edited\n}\n");
    run_index(&db, project_id, root, false).await;
    let rows = load_deleted_symbols(&db, project_id).await;
    assert_eq!(
        rows.len(),
        1,
        "lib.rs re-defining its own `shared` must not heal main.rs's deletion fact, got {rows:?}"
    );
    assert_eq!(rows[0].file_path, "src/main.rs");
}

// ============================================================================
// Facade end-to-end (surrealkv://, the gate's real entry path)
// ============================================================================

/// See `tests/facade_integration.rs` module docs: dropping a `Surreal<Any>`
/// handle releases the surrealkv file lock asynchronously; reopening
/// immediately in-process can race it.
async fn reopen_delay() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// A fresh on-disk store URL whose tempdir outlives the test (leaked on
/// purpose, exactly like `facade_integration.rs`'s `tmp.keep()`).
fn fresh_store_url() -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.keep();
    format!("surrealkv://{}/graph.db", path.display())
}

/// Open the store URL on a raw indexing connection, run one index pass,
/// drop the connection (releasing the single-writer lock), wait it out.
async fn index_at(url: &str, project_id: &str, root: &Path, force: bool) -> IndexResult {
    let result = {
        let db = surrealdb::engine::any::connect(url).await.expect("connect for indexing");
        db.use_ns("codegraph").use_db("codegraph").await.expect("use ns/db");
        db.query(include_str!("../src/schema.surql"))
            .await
            .expect("schema DDL")
            .check()
            .expect("schema DDL check");
        run_index(&Arc::new(db), project_id, root, force).await
    };
    reopen_delay().await;
    result
}

/// One gate-shaped query: open the store exactly as brevity's structural
/// gate does, take the paths verdict, release the lock again.
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

/// THE killtest-6 regression, end-to-end through the gate's real entry:
/// rename a definition while a cross-file caller still uses the old name,
/// re-index incrementally, and ask for the verdict of a plan touching the
/// renamed file. Before this fix the old name was unreachable from the
/// paths entry (only currently-defined symbols were ever queried) and the
/// gate passed silently; now the deletion row routes the old name into the
/// same stale-reference scan the by-name query always had.
#[tokio::test]
async fn paths_verdict_catches_incomplete_rename_then_clean_after_fix() {
    let project_id = "del-facade-rename";
    let url = fresh_store_url();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/helper.rs", "pub fn helper() {}\n");
    write_file(root, "src/stale_caller.rs", "pub fn use_stale() {\n    helper();\n}\n");
    index_at(&url, project_id, root, true).await;

    // The incomplete rename: definition changes, the caller is forgotten.
    write_file(root, "src/helper.rs", "pub fn helper_v2() {}\n");
    index_at(&url, project_id, root, false).await;

    let verdict = paths_verdict(&url, project_id, &["src/helper.rs"]).await;
    assert!(!verdict.graph_empty);
    assert!(
        !verdict.is_clean(),
        "the paths entry must reject an incomplete rename — this was killtest-6's FAIL, got {verdict:?}"
    );
    assert!(
        verdict
            .stale_references
            .iter()
            .any(|r| r.from_name == "use_stale" && r.to_name == "helper"),
        "the forgotten caller must surface by name, got {:?}",
        verdict.stale_references
    );
    // The live-symbol path alone could never have produced this: the touched
    // file defines only the NEW name now.
    assert!(
        verdict.matched_symbols.iter().all(|m| m.name != "helper"),
        "no live symbol answers to the old name — the signal must have come from the deletion row"
    );

    // Complete the rename: fix the caller. The verdict must go clean even
    // though the deletion row is retained (rows are query targets, not
    // verdicts — with zero UNRESOLVED edges left on the old name it
    // contributes nothing). THE no-false-positive property.
    write_file(root, "src/stale_caller.rs", "pub fn use_stale() {\n    helper_v2();\n}\n");
    index_at(&url, project_id, root, false).await;

    let verdict = paths_verdict(&url, project_id, &["src/helper.rs", "src/stale_caller.rs"]).await;
    assert!(
        verdict.is_clean(),
        "a completed rename must pass — a retained deletion row must never reject on its own, got {verdict:?}"
    );

    // The retention itself, asserted on a raw connection (the row is the
    // durable memory that lets a plan touching this file re-check later).
    let rows = {
        let db = surrealdb::engine::any::connect(&url).await.expect("connect raw");
        db.use_ns("codegraph").use_db("codegraph").await.expect("use ns/db");
        load_deleted_symbols(&db, project_id).await
    };
    assert!(
        rows.iter().any(|r| r.name == "helper" && r.file_path == "src/helper.rs"),
        "the deletion fact is retained until healed by a re-added definition, got {rows:?}"
    );
}

/// The straight-delete variant: the definition's whole file is removed from
/// disk, the plan touches that (now nonexistent) path. The live-symbol query
/// finds nothing there by definition; only the deletion rows can route the
/// gate to the dangling caller.
#[tokio::test]
async fn paths_verdict_catches_whole_file_delete() {
    let project_id = "del-facade-file-delete";
    let url = fresh_store_url();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/helper.rs", "pub fn helper() {}\n");
    write_file(root, "src/stale_caller.rs", "pub fn use_stale() {\n    helper();\n}\n");
    index_at(&url, project_id, root, true).await;

    std::fs::remove_file(root.join("src/helper.rs")).expect("rm helper.rs");
    index_at(&url, project_id, root, false).await;

    let verdict = paths_verdict(&url, project_id, &["src/helper.rs"]).await;
    assert!(
        !verdict.is_clean(),
        "deleting a file out from under a live caller must reject, got {verdict:?}"
    );
    assert!(
        verdict
            .stale_references
            .iter()
            .any(|r| r.from_name == "use_stale" && r.to_name == "helper"),
        "the dangling caller must surface, got {:?}",
        verdict.stale_references
    );
    assert!(
        verdict.matched_symbols.is_empty(),
        "a deleted file defines nothing live — every signal here came through deletion tracking"
    );

    // A plan touching only an unrelated file stays clean — the deletion rows
    // belong to src/helper.rs and must not leak into other paths' verdicts.
    let unrelated = paths_verdict(&url, project_id, &["src/stale_caller.rs"]).await;
    assert!(
        unrelated.is_clean(),
        "deletion rows are scoped to their file — unrelated paths must not inherit the rejection, got {unrelated:?}"
    );
}
