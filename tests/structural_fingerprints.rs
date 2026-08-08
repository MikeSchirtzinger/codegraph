//! Structural-fingerprint integration tests (lane 3, Task 2).
//!
//! The property under test: a symbol's persisted fingerprint is the hash of
//! its rooted 1-hop typed neighborhood's canonical certificate — structure,
//! edge types, and node kinds only, never names. Therefore:
//!   * two fresh indexes of the same fixture agree exactly (determinism);
//!   * a PURE rename (definition + callers updated together) moves no hash
//!     — the headline rename-invariance property;
//!   * an added call moves the target's hash — structure is not ignored;
//!   * incremental re-indexes shift hash → prev_hash (one retained prior
//!     generation), tombstone disappeared keys for exactly one generation,
//!     and `--force` wipes history (the spec'd degradation).
//!
//! Style: the deletion-tracking suite's harness (tiny inline tempdir
//! fixtures on isolated `mem://` instances, real `index_project` pipeline);
//! the determinism case runs the committed `tests/fixtures/rust` project
//! through the manifest suite's own `common` harness.

mod common;

use std::path::Path;
use std::sync::Arc;

use surrealdb::engine::any::Any;
use surrealdb::Surreal;

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FpRow {
    qualified_name: String,
    file_path: String,
    node_type: String,
    hash: Option<String>,
    prev_hash: Option<String>,
    generation: i64,
    edge_count: i64,
}

async fn load_fingerprints(db: &Surreal<Any>, project_id: &str) -> Vec<FpRow> {
    let mut resp = db
        .query(
            "SELECT qualified_name, file_path, node_type, hash, prev_hash, generation, \
             edge_count FROM fingerprint WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .expect("query fingerprint");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("take fingerprint rows");

    let mut out: Vec<FpRow> = rows
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
            let get_int = |k: &str| {
                o.get(k)
                    .and_then(|v| match v {
                        surrealdb_types::Value::Number(n) => n.to_int(),
                        _ => None,
                    })
                    .unwrap_or(0)
            };
            let opt = |s: String| if s.is_empty() { None } else { Some(s) };
            Some(FpRow {
                qualified_name: get("qualified_name"),
                file_path: get("file_path"),
                node_type: get("node_type"),
                hash: opt(get("hash")),
                prev_hash: opt(get("prev_hash")),
                generation: get_int("generation"),
                edge_count: get_int("edge_count"),
            })
        })
        .collect();
    out.sort();
    out
}

/// The LIVE row whose qualified_name is exactly `name` or ends in `::name`
/// (module-path prefixes differ per fixture layout; bare crate-root names
/// are legal).
fn live<'a>(rows: &'a [FpRow], name: &str) -> &'a FpRow {
    let suffix = format!("::{name}");
    rows.iter()
        .find(|r| r.hash.is_some() && (r.qualified_name == name || r.qualified_name.ends_with(&suffix)))
        .unwrap_or_else(|| panic!("no live fingerprint for {name} in {rows:?}"))
}

// ============================================================================
// Determinism (committed fixture through the manifest suite's harness)
// ============================================================================

/// Two fresh indexes of the same fixture into two fresh in-memory stores
/// must produce the identical fingerprint set — hashes included. This is
/// what makes the hashes persistable at all (and what `DefaultHasher` would
/// break: its per-process seed survives exactly one process).
#[tokio::test]
async fn determinism_two_fresh_indexes_agree_exactly() {
    let mut sets = Vec::new();
    for _ in 0..2 {
        let db = common::fresh_db().await.expect("fresh db");
        common::index_fixture(&db, "fp-determinism", "tests/fixtures/rust")
            .await
            .expect("index rust fixture");
        sets.push(load_fingerprints(&db, "fp-determinism").await);
    }
    assert!(!sets[0].is_empty(), "the rust fixture must produce fingerprints");
    assert_eq!(sets[0], sets[1], "fingerprint sets must be run-independent");
}

// ============================================================================
// Rename-invariance (the headline property)
// ============================================================================

const CALLER_ONE_FOO: &str = "pub fn one() {\n    foo();\n}\n";
const CALLER_TWO_FOO: &str = "pub fn two() {\n    foo();\n}\n";
const CALLER_ONE_BAR: &str = "pub fn one() {\n    bar();\n}\n";
const CALLER_TWO_BAR: &str = "pub fn two() {\n    bar();\n}\n";

/// A pure rename — definition and every caller updated in lockstep, no
/// structural change — must leave the renamed symbol's fingerprint (and its
/// callers') byte-identical. An added call must move it. Names never enter
/// the hash; structure always does.
#[tokio::test]
async fn pure_rename_preserves_fingerprint_added_call_changes_it() {
    // Before: foo, called by one() and two().
    let db_before = mem_db().await;
    let tmp_before = tempfile::tempdir().expect("tempdir");
    write_file(tmp_before.path(), "src/target.rs", "pub fn foo() {}\n");
    write_file(tmp_before.path(), "src/caller_one.rs", CALLER_ONE_FOO);
    write_file(tmp_before.path(), "src/caller_two.rs", CALLER_TWO_FOO);
    run_index(&db_before, "fp-before", tmp_before.path(), true).await;
    let before = load_fingerprints(&db_before, "fp-before").await;

    // After: the pure rename foo -> bar, callers updated.
    let db_after = mem_db().await;
    let tmp_after = tempfile::tempdir().expect("tempdir");
    write_file(tmp_after.path(), "src/target.rs", "pub fn bar() {}\n");
    write_file(tmp_after.path(), "src/caller_one.rs", CALLER_ONE_BAR);
    write_file(tmp_after.path(), "src/caller_two.rs", CALLER_TWO_BAR);
    run_index(&db_after, "fp-after", tmp_after.path(), true).await;
    let after = load_fingerprints(&db_after, "fp-after").await;

    let foo = live(&before, "foo");
    let bar = live(&after, "bar");
    assert!(foo.edge_count >= 2, "fixture sanity: foo must have resolved callers, got {foo:?}");
    assert_eq!(
        foo.hash, bar.hash,
        "THE headline property: a pure rename must not move the fingerprint"
    );
    assert_eq!(
        live(&before, "one").hash,
        live(&after, "one").hash,
        "an updated caller's own structure is unchanged — its fingerprint must hold too"
    );

    // Added call: same as `after` plus a third caller of bar.
    let db_grown = mem_db().await;
    let tmp_grown = tempfile::tempdir().expect("tempdir");
    write_file(tmp_grown.path(), "src/target.rs", "pub fn bar() {}\n");
    write_file(tmp_grown.path(), "src/caller_one.rs", CALLER_ONE_BAR);
    write_file(tmp_grown.path(), "src/caller_two.rs", CALLER_TWO_BAR);
    write_file(tmp_grown.path(), "src/caller_three.rs", "pub fn three() {\n    bar();\n}\n");
    run_index(&db_grown, "fp-grown", tmp_grown.path(), true).await;
    let grown = load_fingerprints(&db_grown, "fp-grown").await;

    assert_ne!(
        bar.hash,
        live(&grown, "bar").hash,
        "an added call is structural — the target's fingerprint must change"
    );
}

// ============================================================================
// Incremental maintenance: generation shift, tombstones, force wipe
// ============================================================================

/// The persisted lifecycle across incremental runs: every run stamps a new
/// generation and shifts hash → prev_hash; a symbol that disappears leaves
/// exactly one tombstone generation (its last hash in prev_hash, hash NONE)
/// — the raw material `structural_delta_for_paths` pairs renames from —
/// then self-cleans; `--force` resets history entirely.
#[tokio::test]
async fn incremental_runs_shift_generations_and_tombstone_disappearances() {
    let db = mem_db().await;
    let project_id = "fp-lifecycle";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    write_file(root, "src/caller_one.rs", CALLER_ONE_FOO);
    write_file(root, "src/caller_two.rs", CALLER_TWO_FOO);
    let r = run_index(&db, project_id, root, true).await;
    assert_eq!(r.fingerprints.generation, 1);
    let gen1 = load_fingerprints(&db, project_id).await;
    assert!(gen1.iter().all(|r| r.generation == 1 && r.prev_hash.is_none() && r.hash.is_some()));
    let foo_hash = live(&gen1, "foo").hash.clone();

    // No-op incremental run: generation advances, every hash preserved.
    let r = run_index(&db, project_id, root, false).await;
    assert_eq!(r.files_indexed, 0, "nothing changed on disk");
    assert_eq!(r.fingerprints.generation, 2);
    let gen2 = load_fingerprints(&db, project_id).await;
    assert_eq!(gen2.len(), gen1.len(), "no tombstones from a no-op");
    assert!(
        gen2.iter().all(|r| r.generation == 2 && r.hash == r.prev_hash),
        "a no-op run must report every symbol structure-preserved, got {gen2:?}"
    );

    // In-place pure rename (definition + callers in one incremental pass):
    // the old key tombstones carrying its last hash; the new key appears
    // with the SAME hash value — the rename-pairing evidence.
    write_file(root, "src/target.rs", "pub fn bar() {}\n");
    write_file(root, "src/caller_one.rs", CALLER_ONE_BAR);
    write_file(root, "src/caller_two.rs", CALLER_TWO_BAR);
    let r = run_index(&db, project_id, root, false).await;
    assert_eq!(r.fingerprints.generation, 3);
    assert_eq!(r.fingerprints.removed, 1, "exactly foo's key disappeared");
    let gen3 = load_fingerprints(&db, project_id).await;
    let tomb = gen3
        .iter()
        .find(|r| r.hash.is_none())
        .expect("foo must leave a tombstone");
    assert!(tomb.qualified_name.ends_with("foo"));
    assert_eq!(tomb.prev_hash, foo_hash, "the tombstone carries foo's last hash");
    let bar = live(&gen3, "bar");
    assert_eq!(bar.hash, foo_hash, "pure rename: bar's hash IS foo's hash");
    assert!(bar.prev_hash.is_none(), "bar's key is new this generation");

    // One more no-op: the tombstone has served its one generation and drops.
    run_index(&db, project_id, root, false).await;
    let gen4 = load_fingerprints(&db, project_id).await;
    assert!(
        gen4.iter().all(|r| r.hash.is_some()),
        "tombstones self-clean after one generation, got {gen4:?}"
    );

    // --force: history resets (the spec'd degradation, same as deleted_symbol).
    let r = run_index(&db, project_id, root, true).await;
    assert_eq!(r.fingerprints.generation, 1, "force re-baselines generations");
    let forced = load_fingerprints(&db, project_id).await;
    assert!(
        forced.iter().all(|r| r.generation == 1 && r.prev_hash.is_none()),
        "force must wipe fingerprint history, got {forced:?}"
    );
}

// ============================================================================
// Refactor-neutrality delta, end-to-end through the facade (Task 4).
// surrealkv:// like tests/deletion_tracking.rs's facade section:
// facade::open_store opens its own connection, which mem:// can't cross.
// ============================================================================

use std::time::Duration;

use codegraph::facade;

/// See `tests/facade_integration.rs` module docs: dropping a `Surreal<Any>`
/// handle releases the surrealkv file lock asynchronously; reopening
/// immediately in-process can race it.
async fn reopen_delay() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

fn fresh_store_url() -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.keep();
    format!("surrealkv://{}/graph.db", path.display())
}

async fn index_at(url: &str, project_id: &str, root: &Path, force: bool) {
    {
        let db = surrealdb::engine::any::connect(url).await.expect("connect for indexing");
        db.use_ns("codegraph").use_db("codegraph").await.expect("use ns/db");
        db.query(include_str!("../src/schema.surql"))
            .await
            .expect("schema DDL")
            .check()
            .expect("schema DDL check");
        run_index(&Arc::new(db), project_id, root, force).await;
    }
    reopen_delay().await;
}

async fn paths_delta(url: &str, project_id: &str, paths: &[&str]) -> facade::StructuralDelta {
    let delta = {
        let store = facade::open_store(url, project_id).await.expect("open_store");
        facade::structural_delta_for_paths(
            &store,
            &paths.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .await
        .expect("structural_delta_for_paths")
    };
    reopen_delay().await;
    delta
}

/// THE Task 4 proof, both halves, through the gate's real entry
/// (`open_store` → `structural_delta_for_paths` on a real surrealkv
/// store — the sequence `examples/neutrality_delta.rs` mirrors for the
/// shell): a pure rename re-indexed incrementally reports every touched
/// symbol `preserved` (the renamed one paired with `renamed_from` — a
/// PROVEN structure-neutral refactor); an added call reports the callee
/// `changed` and the new caller `added` — NOT neutral.
#[tokio::test]
async fn delta_proves_pure_rename_neutral_and_added_call_not() {
    let project_id = "delta-facade";
    let url = fresh_store_url();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(root, "src/target.rs", "pub fn foo() {}\n");
    write_file(root, "src/caller_one.rs", CALLER_ONE_FOO);
    write_file(root, "src/caller_two.rs", CALLER_TWO_FOO);
    index_at(&url, project_id, root, true).await;

    // The pure rename: definition AND both callers, one incremental pass.
    write_file(root, "src/target.rs", "pub fn bar() {}\n");
    write_file(root, "src/caller_one.rs", CALLER_ONE_BAR);
    write_file(root, "src/caller_two.rs", CALLER_TWO_BAR);
    index_at(&url, project_id, root, false).await;

    let touched = ["src/target.rs", "src/caller_one.rs", "src/caller_two.rs"];
    let delta = paths_delta(&url, project_id, &touched).await;
    assert!(!delta.graph_empty);
    assert!(
        delta.is_structure_neutral(),
        "a pure rename must be PROVABLY structure-neutral, got {delta:?}"
    );
    assert!(delta.preserved.len() >= 3, "target + both callers, got {delta:?}");
    let renamed = delta
        .preserved
        .iter()
        .find(|s| s.renamed_from.is_some())
        .expect("the renamed symbol must surface as a paired preservation");
    assert!(renamed.qualified_name.ends_with("bar"), "got {renamed:?}");
    assert!(
        renamed.renamed_from.as_deref().unwrap_or("").ends_with("foo"),
        "provenance must name the old symbol, got {renamed:?}"
    );

    // Now a structural change: a third caller of bar appears.
    write_file(root, "src/caller_three.rs", "pub fn three() {\n    bar();\n}\n");
    index_at(&url, project_id, root, false).await;

    let delta = paths_delta(&url, project_id, &["src/target.rs", "src/caller_three.rs"]).await;
    assert!(
        !delta.is_structure_neutral(),
        "an added call is structural change, got {delta:?}"
    );
    assert!(
        delta.changed.iter().any(|s| s.qualified_name.ends_with("bar")),
        "the callee gained an in-edge — it must report changed, got {delta:?}"
    );
    assert!(
        delta.added.iter().any(|s| s.qualified_name.ends_with("three")),
        "the new caller is an added symbol, got {delta:?}"
    );

    // Untouched-paths sanity: a delta over only the unchanged callers stays
    // neutral — the change is scoped to the files that carry it.
    let unrelated = paths_delta(&url, project_id, &["src/caller_one.rs"]).await;
    assert!(unrelated.is_structure_neutral(), "caller_one didn't change, got {unrelated:?}");
}
