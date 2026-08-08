//! R4 integration tests: incremental re-resolution
//! (`specs/resolution-layer-v1.md` §"Incremental re-resolution", §R4).
//!
//! Self-contained by design: doesn't touch `tests/fixtures/**` (R3a/R3b's
//! territory — builds its own tiny fixtures inline, in a tempdir) and
//! doesn't share a `tests/common` module with any other integration test
//! file, so it can neither be destabilized by, nor destabilize, parallel
//! test-suite work landing in `tests/*.rs` at the same time. Each test gets
//! its own fresh `mem://` SurrealDB instance (isolated, lock-free, fast).
//!
//! Five shapes, matching the R4 and edge-cleanup task briefs' acceptance:
//! - `fresh_index_with_no_old_ids_stores_the_complete_edge_set` — the
//!   empty-old-ID fast path still stores and resolves the new file's edges.
//! - `reindex_replaces_only_edges_emitted_by_the_changed_file` — the
//!   non-empty-old-ID path removes a changed source's old edges without
//!   deleting an unchanged caller's edge into a changed target.
//! - `touch_one_file_re_resolves_only_the_affected_edges` — selectivity
//!   (the gen counter proves only the affected edges moved) + correctness.
//! - `renaming_a_symbol_flips_the_caller_and_back` — a RESOLVED edge must
//!   flip to UNRESOLVED when its target is renamed away, and back when
//!   restored.
//! - `incremental_end_state_matches_a_fresh_full_index` — the oracle
//!   property: after an arbitrary sequence of incremental changes, the
//!   resulting edge set (`to_id`/`confidence`/`resolved_by`) must be
//!   identical to indexing the same final tree from scratch.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::index::{self, IndexConfig, IndexResult, IndexingTier};

// ============================================================================
// Test harness — deliberately not shared with any other tests/*.rs file.
// ============================================================================

/// A brand-new, isolated in-memory SurrealDB instance with the production
/// schema applied. Mirrors `codegraph::db::connect`/`init_schema` — not
/// reused directly, since `db` isn't part of the crate's public API (see
/// `src/lib.rs`) and duplicating ~10 lines here keeps this file fully
/// independent of any other test module. No `signin` call: embedded engines
/// (`mem://`) have no auth, exactly as `db::connect`'s own comment notes.
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

/// Write one fixture file, creating parent directories as needed.
fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(&path, content).expect("write fixture file");
}

async fn index_fresh(db: &Arc<Surreal<Any>>, project_id: &str, root: &Path) -> IndexResult {
    let config = IndexConfig {
        project_id: project_id.to_string(),
        root_path: root.to_path_buf(),
        tier: IndexingTier::Balanced,
        languages: None,
        force: true,
    };
    index::index_project(db, &config)
        .await
        .expect("fresh (force=true) index_project")
}

async fn index_incremental(db: &Arc<Surreal<Any>>, project_id: &str, root: &Path) -> IndexResult {
    let config = IndexConfig {
        project_id: project_id.to_string(),
        root_path: root.to_path_buf(),
        tier: IndexingTier::Balanced,
        languages: None,
        force: false,
    };
    index::index_project(db, &config)
        .await
        .expect("incremental (force=false) index_project")
}

// -- Minimal raw code_edge row access ---------------------------------------
// Mirrors `index::resolve`'s own manual `surrealdb_types::Value` extraction
// style (its `extract_str`/`extract_i64` aren't `pub`, so this can't just
// reuse them) rather than pulling in serde deserialization for a handful of
// fields across three tests.

#[derive(Debug, Clone)]
struct EdgeRow {
    to_id: String,
    confidence: String,
    resolved_by: Option<String>,
    resolution_gen: Option<i64>,
}

fn as_object(v: &surrealdb_types::Value) -> Option<&surrealdb_types::Object> {
    match v {
        surrealdb_types::Value::Object(o) => Some(o),
        _ => None,
    }
}

fn field_str(o: &surrealdb_types::Object, key: &str) -> String {
    o.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn field_opt_str(o: &surrealdb_types::Object, key: &str) -> Option<String> {
    o.get(key).and_then(|v| match v {
        surrealdb_types::Value::String(s) => Some(s.to_string()),
        _ => None,
    })
}

fn field_opt_i64(o: &surrealdb_types::Object, key: &str) -> Option<i64> {
    o.get(key).and_then(|v| match v {
        surrealdb_types::Value::Number(surrealdb_types::Number::Int(n)) => Some(*n),
        _ => None,
    })
}

/// Every `calls`-type edge in the project, keyed by `(from_id, to_name)` —
/// unique for every fixture in this file (no caller here ever calls the
/// same bare name twice).
async fn load_calls_edges(db: &Surreal<Any>, project_id: &str) -> HashMap<(String, String), EdgeRow> {
    let mut resp = db
        .query(
            "SELECT from_id, to_name, to_id, confidence, resolved_by, resolution_gen \
             FROM code_edge WHERE project_id = $pid AND edge_type = 'calls'",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .expect("query calls edges");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take calls edges");

    rows.iter()
        .filter_map(|v| {
            let o = as_object(v)?;
            let from_id = field_str(o, "from_id");
            let to_name = field_str(o, "to_name");
            if from_id.is_empty() {
                return None;
            }
            Some((
                (from_id, to_name),
                EdgeRow {
                    to_id: field_str(o, "to_id"),
                    confidence: field_str(o, "confidence"),
                    resolved_by: field_opt_str(o, "resolved_by"),
                    resolution_gen: field_opt_i64(o, "resolution_gen"),
                },
            ))
        })
        .collect()
}

/// `node_id` for the (unique-in-this-file) node matching `name`.
async fn node_id_by_name(db: &Surreal<Any>, project_id: &str, name: &str) -> String {
    let mut resp = db
        .query("SELECT VALUE node_id FROM code_node WHERE project_id = $pid AND name = $name")
        .bind(("pid", project_id.to_string()))
        .bind(("name", name.to_string()))
        .await
        .expect("query node id");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take node id");
    rows.into_iter()
        .find_map(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no node named {name}"))
}

/// `array::len(candidates)` for the (unique-in-this-file) `calls` edge from
/// the node named `from_name` to `to_name` — used only to check an
/// AMBIGUOUS edge's candidate set grew as expected, without hand-parsing a
/// `Value::Array`.
async fn candidates_len(db: &Surreal<Any>, project_id: &str, from_name: &str, to_name: &str) -> Option<i64> {
    let mut resp = db
        .query(
            "SELECT VALUE array::len(candidates) FROM code_edge \
             WHERE project_id = $pid AND edge_type = 'calls' AND to_name = $to_name \
             AND from_id IN (SELECT VALUE node_id FROM code_node WHERE project_id = $pid AND name = $from_name)",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("to_name", to_name.to_string()))
        .bind(("from_name", from_name.to_string()))
        .await
        .expect("query candidates len");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take candidates len");
    rows.into_iter().find_map(|v| match v {
        surrealdb_types::Value::Number(surrealdb_types::Number::Int(n)) => Some(n),
        _ => None,
    })
}

// ============================================================================
// Edge cleanup: empty-old-ID and non-empty-old-ID paths.
// ============================================================================

/// Regression test for verification defect D7.
///
/// The edge cleanup deletes by `from_id IN $ids` while the matching node
/// delete is by `file_path` and therefore unconditional. That makes the edge
/// delete only as complete as `$ids`, which `index_project` derives from
/// `resolve::load_file_symbols`. That loader used to drop rows with an empty
/// `name` — so an unnamed node would be deleted while its outgoing edges
/// survived, orphaned against a dead `node_id` and accumulating on every
/// re-index.
///
/// No extractor produces an unnamed node today (every named-entity call site
/// guards `name.is_empty()`, and import/include/export nodes are named from
/// raw source text), so this cannot be provoked through the parser. The node
/// is therefore injected directly — the point is to pin the *invariant*, not
/// to claim the parser can currently violate it. One relaxed guard or one new
/// extractor and this becomes reachable, silently.
#[tokio::test]
async fn unnamed_nodes_are_still_covered_by_the_edge_delete_set() {
    let project_id = "edge-cleanup-unnamed";
    let db = mem_db().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(
        root,
        "src/caller.rs",
        "pub fn caller() {\n    target();\n}\n",
    );
    write_file(root, "src/target.rs", "pub fn target() {}\n");
    index_fresh(&db, project_id, root).await;

    // Inject an unnamed node belonging to src/target.rs, plus an edge it
    // emits. This is the shape the parser would produce if any extractor ever
    // stopped guarding an empty name.
    db.query(
        "CREATE code_node SET node_id = $nid, project_id = $pid, name = '', \
         node_type = 'function', language = 'rust', file_path = 'src/target.rs', \
         qualified_name = 'src::target::'",
    )
    .bind(("nid", "unnamed-node-1"))
    .bind(("pid", project_id))
    .await
    .expect("inject unnamed node")
    .check()
    .expect("unnamed node validates");

    db.query(
        "CREATE code_edge SET project_id = $pid, from_id = $nid, to_id = '', \
         to_name = 'somewhere', to_type = 'function', edge_type = 'calls', \
         confidence = 'INFERRED'",
    )
    .bind(("nid", "unnamed-node-1"))
    .bind(("pid", project_id))
    .await
    .expect("inject unnamed node's edge")
    .check()
    .expect("unnamed edge validates");

    assert_eq!(
        count_edges_from(&db, project_id, "unnamed-node-1").await,
        1,
        "fixture must actually be in place before the re-index"
    );

    // Re-index src/target.rs. This is the non-empty-$ids path, so the file's
    // nodes are deleted by file_path and its edges by from_id IN $ids.
    write_file(
        root,
        "src/target.rs",
        "pub fn target() {\n    let _changed = 1;\n}\n",
    );
    index_incremental(&db, project_id, root).await;

    assert_eq!(
        count_nodes_with_id(&db, project_id, "unnamed-node-1").await,
        0,
        "the unnamed node is deleted by file_path — that half was never in doubt"
    );
    assert_eq!(
        count_edges_from(&db, project_id, "unnamed-node-1").await,
        0,
        "D7: the unnamed node's edges must be deleted with it, not left orphaned \
         pointing at a dead node_id. If this fails, load_file_symbols has started \
         filtering rows again and clean_file_nodes' delete set is incomplete."
    );

    // The real file's edges must be unaffected by any of the above.
    let calls = load_calls_edges(&db, project_id).await;
    let caller_id = node_id_by_name(&db, project_id, "caller").await;
    let target_id = node_id_by_name(&db, project_id, "target").await;
    let edge = &calls[&(caller_id, "target".to_string())];
    assert_eq!(edge.to_id, target_id);
    assert_eq!(edge.confidence, "RESOLVED");
}

async fn count_edges_from(db: &Surreal<Any>, project_id: &str, from_id: &str) -> usize {
    let mut resp = db
        .query("SELECT * FROM code_edge WHERE project_id = $pid AND from_id = $fid")
        .bind(("pid", project_id.to_string()))
        .bind(("fid", from_id.to_string()))
        .await
        .expect("count edges query");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("edge rows");
    rows.len()
}

async fn count_nodes_with_id(db: &Surreal<Any>, project_id: &str, node_id: &str) -> usize {
    let mut resp = db
        .query("SELECT * FROM code_node WHERE project_id = $pid AND node_id = $nid")
        .bind(("pid", project_id.to_string()))
        .bind(("nid", node_id.to_string()))
        .await
        .expect("count nodes query");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("node rows");
    rows.len()
}

#[tokio::test]
async fn fresh_index_with_no_old_ids_stores_the_complete_edge_set() {
    let project_id = "edge-cleanup-fresh";
    let db = mem_db().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(
        root,
        "src/caller.rs",
        "pub fn caller() {\n    target();\n}\n",
    );
    write_file(root, "src/target.rs", "pub fn target() {}\n");

    let indexed = index_fresh(&db, project_id, root).await;
    assert_eq!(indexed.files_indexed, 2);
    assert!(
        indexed.errors.is_empty(),
        "fresh index errors: {:?}",
        indexed.errors
    );

    let calls = load_calls_edges(&db, project_id).await;
    assert_eq!(calls.len(), 1, "fresh cleanup must not discard the new edge");
    let caller_id = node_id_by_name(&db, project_id, "caller").await;
    let target_id = node_id_by_name(&db, project_id, "target").await;
    let edge = &calls[&(caller_id, "target".to_string())];
    assert_eq!(edge.to_id, target_id);
    assert_eq!(edge.confidence, "RESOLVED");
}

#[tokio::test]
async fn reindex_replaces_only_edges_emitted_by_the_changed_file() {
    let project_id = "edge-cleanup-reindex";
    let db = mem_db().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(
        root,
        "src/caller.rs",
        "pub fn caller() {\n    target();\n}\n",
    );
    write_file(
        root,
        "src/target.rs",
        "pub fn target() {}\npub fn replacement() {}\n",
    );
    index_fresh(&db, project_id, root).await;

    // Re-index the target only. The caller's incoming edge belongs to the
    // unchanged caller file, so source-only cleanup must preserve it.
    write_file(
        root,
        "src/target.rs",
        "pub fn target() {\n    let _changed = 1;\n}\npub fn replacement() {}\n",
    );
    let target_reindex = index_incremental(&db, project_id, root).await;
    assert_eq!(target_reindex.files_indexed, 1);

    let caller_id = node_id_by_name(&db, project_id, "caller").await;
    let target_id = node_id_by_name(&db, project_id, "target").await;
    let after_target = load_calls_edges(&db, project_id).await;
    let retained = &after_target[&(caller_id.clone(), "target".to_string())];
    assert_eq!(retained.to_id, target_id);
    assert_eq!(retained.confidence, "RESOLVED");

    // Re-index the source. Its old emitted edge must disappear and the new
    // emitted edge must be the only calls edge left for this project.
    write_file(
        root,
        "src/caller.rs",
        "pub fn caller() {\n    replacement();\n}\n",
    );
    let caller_reindex = index_incremental(&db, project_id, root).await;
    assert_eq!(caller_reindex.files_indexed, 1);

    let replacement_id = node_id_by_name(&db, project_id, "replacement").await;
    let after_caller = load_calls_edges(&db, project_id).await;
    assert_eq!(
        after_caller.len(),
        1,
        "the old emitted edge must be removed"
    );
    assert!(
        !after_caller.contains_key(&(caller_id.clone(), "target".to_string())),
        "the changed caller's old target edge survived cleanup"
    );
    let replacement = &after_caller[&(caller_id, "replacement".to_string())];
    assert_eq!(replacement.to_id, replacement_id);
    assert_eq!(replacement.confidence, "RESOLVED");
}

// ============================================================================
// Touch one file — selectivity (gen counter) + correctness.
// ============================================================================

/// v1: `b.rs` calls `helper()` (undefined — UNRESOLVED) and `c.rs` calls
/// `totally_unrelated()` (undefined, and stays that way forever — the
/// control group). Then `a.rs` is modified to *define* `helper()` (calling
/// same-file `placeholder_a()`). Only the edges the R4 spec's two-sided
/// rule actually predicts should move:
/// (a) `helper -> placeholder_a`, brand new, from the changed file itself;
/// (b) `caller_b -> helper`, elsewhere in the project, whose `to_name`
///     bare-tails to `helper` — now in the delta since `a.rs` just started
///     defining it.
/// `caller_c -> totally_unrelated` must be untouched: same confidence, same
/// `resolution_gen` — the whole point of "incremental".
#[tokio::test]
async fn touch_one_file_re_resolves_only_the_affected_edges() {
    let db = mem_db().await;
    let project_id = "r4-touch-one-file";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/a.rs", "pub fn placeholder_a() {}\n");
    write_file(
        root,
        "src/b.rs",
        "pub fn caller_b() {\n    helper();\n}\n",
    );
    write_file(
        root,
        "src/c.rs",
        "pub fn caller_c() {\n    totally_unrelated();\n}\n",
    );

    let v1 = index_fresh(&db, project_id, root).await;
    assert_eq!(v1.resolution.resolved, 0, "helper() and totally_unrelated() are both undefined in v1");
    assert_eq!(v1.resolution.unresolved, 2);

    let before = load_calls_edges(&db, project_id).await;
    assert_eq!(before.len(), 2);
    for edge in before.values() {
        assert_eq!(edge.confidence, "UNRESOLVED");
        assert_eq!(edge.resolution_gen, Some(1));
    }

    // Modify a.rs: define helper(), which itself calls the already-existing
    // placeholder_a() (same file, bare call — R3).
    write_file(
        root,
        "src/a.rs",
        "pub fn placeholder_a() {}\n\npub fn helper() {\n    placeholder_a();\n}\n",
    );
    let v2 = index_incremental(&db, project_id, root).await;

    // Selectivity: exactly the two edges the two-sided rule predicts, not
    // the whole project's edge set.
    assert_eq!(
        v2.resolution.edges_considered, 2,
        "affected set must be exactly {{helper->placeholder_a, caller_b->helper}}, not every edge"
    );
    assert_eq!(v2.resolution.resolved, 2);

    let after = load_calls_edges(&db, project_id).await;
    assert_eq!(after.len(), 3, "one brand-new edge (helper->placeholder_a) plus the original two");

    let helper_id = node_id_by_name(&db, project_id, "helper").await;
    let placeholder_id = node_id_by_name(&db, project_id, "placeholder_a").await;

    let caller_b_edge = after
        .iter()
        .find(|((_, to_name), _)| to_name == "helper")
        .map(|(_, e)| e)
        .expect("caller_b -> helper edge");
    assert_eq!(caller_b_edge.confidence, "RESOLVED");
    assert_eq!(caller_b_edge.resolved_by.as_deref(), Some("r5"));
    assert_eq!(caller_b_edge.to_id, helper_id);
    assert_eq!(caller_b_edge.resolution_gen, Some(2), "criterion (b): to_name delta match");

    let helper_edge = after
        .iter()
        .find(|((_, to_name), _)| to_name == "placeholder_a")
        .map(|(_, e)| e)
        .expect("helper -> placeholder_a edge");
    assert_eq!(helper_edge.confidence, "RESOLVED");
    assert_eq!(helper_edge.resolved_by.as_deref(), Some("r3"));
    assert_eq!(helper_edge.to_id, placeholder_id);
    assert_eq!(helper_edge.resolution_gen, Some(2), "criterion (a): edge from the changed file");

    // The control: an edge nothing about this change should touch at all.
    let caller_c_edge = after
        .iter()
        .find(|((_, to_name), _)| to_name == "totally_unrelated")
        .map(|(_, e)| e)
        .expect("caller_c -> totally_unrelated edge");
    assert_eq!(caller_c_edge.confidence, "UNRESOLVED", "must remain untouched");
    assert_eq!(
        caller_c_edge.resolution_gen,
        Some(1),
        "gen counter proves selectivity: this edge was never re-examined"
    );
}

// ============================================================================
// Test 2: renaming a symbol flips its caller RESOLVED -> UNRESOLVED -> back.
// ============================================================================

#[tokio::test]
async fn renaming_a_symbol_flips_the_caller_and_back() {
    let db = mem_db().await;
    let project_id = "r4-rename-symbol";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    write_file(root, "src/caller.rs", "pub fn call_it() {\n    foo();\n}\n");

    index_fresh(&db, project_id, root).await;
    let v1 = load_calls_edges(&db, project_id).await;
    let call_it_edge = v1
        .iter()
        .find(|((_, to_name), _)| to_name == "foo")
        .map(|(_, e)| e)
        .expect("call_it -> foo edge");
    assert_eq!(call_it_edge.confidence, "RESOLVED");
    assert_eq!(call_it_edge.resolved_by.as_deref(), Some("r5"));
    assert_eq!(call_it_edge.resolution_gen, Some(1));

    // Rename foo -> foo_renamed. Nothing calls foo_renamed, and nothing else
    // is named foo anywhere in the project, so call_it's edge must go dark.
    write_file(root, "src/target.rs", "pub fn foo_renamed() {}\n");
    let v2 = index_incremental(&db, project_id, root).await;
    assert_eq!(v2.resolution.edges_considered, 1, "only call_it's edge bare-tails to the delta ({{foo, foo_renamed}})");

    let after_rename = load_calls_edges(&db, project_id).await;
    let call_it_edge = after_rename
        .iter()
        .find(|((_, to_name), _)| to_name == "foo")
        .map(|(_, e)| e)
        .expect("call_it -> foo edge still present (to_name is verbatim, never rewritten)");
    assert_eq!(call_it_edge.confidence, "UNRESOLVED", "foo no longer exists anywhere");
    assert_eq!(call_it_edge.to_id, "");
    assert_eq!(call_it_edge.resolution_gen, Some(2));

    // Restore foo. call_it's now-UNRESOLVED edge must flip back.
    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    let v3 = index_incremental(&db, project_id, root).await;
    assert_eq!(v3.resolution.edges_considered, 1);

    let after_restore = load_calls_edges(&db, project_id).await;
    let call_it_edge = after_restore
        .iter()
        .find(|((_, to_name), _)| to_name == "foo")
        .map(|(_, e)| e)
        .expect("call_it -> foo edge");
    let foo_id = node_id_by_name(&db, project_id, "foo").await;
    assert_eq!(call_it_edge.confidence, "RESOLVED");
    assert_eq!(call_it_edge.resolved_by.as_deref(), Some("r5"));
    assert_eq!(call_it_edge.to_id, foo_id);
    assert_eq!(call_it_edge.resolution_gen, Some(3));
}

// ============================================================================
// Test 3: the oracle property — incremental end-state == fresh full index.
// ============================================================================

/// Applies a mixed sequence of incremental changes (modify-causing-a-rename,
/// delete-causing-a-dangling-caller, add-causing-an-ambiguity-to-grow) to a
/// small project, one `index_project` call at a time, then compares the
/// resulting edge set against indexing the identical final tree from
/// scratch (a *different*, independent `mem://` instance + the untouched
/// `resolve::resolve_project` full-pass code path — real cross-validation,
/// not the incremental code checked against itself).
#[tokio::test]
async fn incremental_end_state_matches_a_fresh_full_index() {
    let project_id = "r4-oracle";

    // -- Build up the incremental side across a sequence of real changes --
    let db_incremental = mem_db().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/main.rs", "pub fn entry() {\n    helper();\n    ghost();\n}\n");
    write_file(root, "src/helper_mod.rs", "pub fn helper() {}\n");
    write_file(root, "src/doomed.rs", "pub fn doomed_fn() {}\n");
    write_file(
        root,
        "src/caller_of_doomed.rs",
        "pub fn call_doomed() {\n    doomed_fn();\n}\n",
    );
    write_file(root, "src/collide_a.rs", "pub fn shared_name() {}\n");
    write_file(root, "src/collide_b.rs", "pub fn shared_name() {}\n");
    write_file(
        root,
        "src/ambig_caller.rs",
        "pub fn call_shared() {\n    shared_name();\n}\n",
    );

    index_fresh(&db_incremental, project_id, root).await;

    // Change 1: rename helper -> helper_v2. entry()'s call to helper() must
    // eventually go dark.
    let helper_v2_src = "pub fn helper_v2() {}\n";
    write_file(root, "src/helper_mod.rs", helper_v2_src);
    let v1 = index_incremental(&db_incremental, project_id, root).await;
    assert_eq!(v1.resolution.edges_considered, 1, "only entry->helper bare-tails to the {{helper, helper_v2}} delta");

    // Change 2: delete doomed.rs. call_doomed()'s reference must eventually
    // go dark too.
    std::fs::remove_file(root.join("src/doomed.rs")).expect("rm doomed.rs");
    let v2 = index_incremental(&db_incremental, project_id, root).await;
    assert_eq!(v2.resolution.edges_considered, 1, "only call_doomed->doomed_fn bare-tails to the delta");

    // Change 3: add a third same-named collider. call_shared()'s ambiguity
    // must grow from 2 to 3 candidates.
    let collide_c_src = "pub fn shared_name() {}\n";
    write_file(root, "src/collide_c.rs", collide_c_src);
    let v3 = index_incremental(&db_incremental, project_id, root).await;
    assert_eq!(v3.resolution.edges_considered, 1, "only call_shared->shared_name bare-tails to the delta");

    // -- Independently build the identical final tree, indexed fresh -------
    let db_fresh = mem_db().await;
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let root2 = tmp2.path();

    write_file(root2, "src/main.rs", "pub fn entry() {\n    helper();\n    ghost();\n}\n");
    write_file(root2, "src/helper_mod.rs", helper_v2_src);
    // src/doomed.rs deliberately absent.
    write_file(
        root2,
        "src/caller_of_doomed.rs",
        "pub fn call_doomed() {\n    doomed_fn();\n}\n",
    );
    write_file(root2, "src/collide_a.rs", "pub fn shared_name() {}\n");
    write_file(root2, "src/collide_b.rs", "pub fn shared_name() {}\n");
    write_file(root2, "src/collide_c.rs", collide_c_src);
    write_file(
        root2,
        "src/ambig_caller.rs",
        "pub fn call_shared() {\n    shared_name();\n}\n",
    );

    index_fresh(&db_fresh, project_id, root2).await;

    // -- Compare -------------------------------------------------------------
    let incremental_edges = load_calls_edges(&db_incremental, project_id).await;
    let fresh_edges = load_calls_edges(&db_fresh, project_id).await;
    // 4 calls edges total throughout (entry->helper, entry->ghost,
    // call_doomed->doomed_fn, call_shared->shared_name); each of the three
    // incremental passes above touched exactly 1 of them (asserted at each
    // call site) — never the other 3, and never duplicated a stale one.
    assert_eq!(incremental_edges.len(), 4);
    assert_eq!(fresh_edges.len(), 4);

    let incremental_keys: HashSet<_> = incremental_edges.keys().cloned().collect();
    let fresh_keys: HashSet<_> = fresh_edges.keys().cloned().collect();
    assert_eq!(
        incremental_keys, fresh_keys,
        "no phantom edges left over from a deleted/modified file, and none missing"
    );

    let mut mismatches = Vec::new();
    for (key, inc) in &incremental_edges {
        let fresh = &fresh_edges[key];
        // Deliberately NOT comparing resolution_gen: the incremental side
        // passed through this data several times (higher gens), the fresh
        // side exactly once (gen 1) — that's expected, not a defect. The
        // oracle property is about the *binding*, per the R4 task brief.
        if inc.to_id != fresh.to_id || inc.confidence != fresh.confidence || inc.resolved_by != fresh.resolved_by {
            mismatches.push(format!(
                "{key:?}: incremental={{to_id={}, confidence={}, resolved_by={:?}}} fresh={{to_id={}, confidence={}, resolved_by={:?}}}",
                inc.to_id, inc.confidence, inc.resolved_by, fresh.to_id, fresh.confidence, fresh.resolved_by
            ));
        }
    }
    assert!(mismatches.is_empty(), "oracle mismatch(es):\n{}", mismatches.join("\n"));

    // Spot-check the three interesting transitions landed where expected,
    // on both sides (belt-and-suspenders beyond the blanket comparison
    // above).
    let entry_to_helper = &fresh_edges[&(node_id_by_name(&db_fresh, project_id, "entry").await, "helper".to_string())];
    assert_eq!(entry_to_helper.confidence, "UNRESOLVED", "helper was renamed away");

    let entry_to_ghost = &fresh_edges[&(node_id_by_name(&db_fresh, project_id, "entry").await, "ghost".to_string())];
    assert_eq!(entry_to_ghost.confidence, "UNRESOLVED", "ghost() was never defined, on either side");

    let call_doomed_to_doomed_fn = &fresh_edges[&(
        node_id_by_name(&db_fresh, project_id, "call_doomed").await,
        "doomed_fn".to_string(),
    )];
    assert_eq!(call_doomed_to_doomed_fn.confidence, "UNRESOLVED", "doomed_fn's file was deleted");

    let call_shared_to_shared_name = &fresh_edges[&(
        node_id_by_name(&db_fresh, project_id, "call_shared").await,
        "shared_name".to_string(),
    )];
    assert_eq!(call_shared_to_shared_name.confidence, "AMBIGUOUS", "3-way collide_a/b/c collision");

    // The interesting incremental-specific behavior: this edge went through
    // TWO resolutions on the incremental side (2-way ambiguous after v1,
    // then re-examined and widened to 3-way once collide_c.rs was added) —
    // both sides must still agree on the final candidate count.
    for (db, project_id_ref) in [(&db_incremental, project_id), (&db_fresh, project_id)] {
        let n = candidates_len(db, project_id_ref, "call_shared", "shared_name").await;
        assert_eq!(n, Some(3), "candidate set must list all three colliding shared_name definitions");
    }
}
