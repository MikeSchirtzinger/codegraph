//! Clone-detection integration tests (lane 3, Task 3).
//!
//! `graph::clones::find_clone_groups` — the engine behind `codegraph
//! clones` — groups live symbols whose neighborhood certificates hash
//! identically, above a minimum-structure threshold. The fixture constructs
//! a deliberate structural clone pair under different names, plus decoys
//! that must NOT group: a same-size-but-different-shape cluster, and
//! pure-containment shapes that are trivially same-shape at any size.

use std::path::Path;
use std::sync::Arc;

use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::graph::clones::find_clone_groups;
use codegraph::index::{self, IndexConfig, IndexingTier};

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

async fn index(db: &Arc<Surreal<Any>>, project_id: &str, root: &Path) {
    let config = IndexConfig {
        project_id: project_id.to_string(),
        root_path: root.to_path_buf(),
        tier: IndexingTier::Balanced,
        languages: None,
        force: true,
    };
    index::index_project(db, &config).await.expect("index_project");
}

/// The constructed clone pair: `alpha` and `beta` each call three distinct
/// helpers (identical 3-edge star shape, different names everywhere);
/// `gamma` also has a 3-edge neighborhood but a different shape (three
/// calls to ONE helper). Only {alpha, beta} may group at `--min-edges 3`.
#[tokio::test]
async fn constructed_clone_pair_groups_and_decoy_does_not() {
    let db = mem_db().await;
    let project_id = "clones-pair";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write_file(
        root,
        "src/cluster_a.rs",
        "pub fn alpha() {\n    h_one();\n    h_two();\n    h_three();\n}\n\
         pub fn h_one() {}\npub fn h_two() {}\npub fn h_three() {}\n",
    );
    write_file(
        root,
        "src/cluster_b.rs",
        "pub fn beta() {\n    k_one();\n    k_two();\n    k_three();\n}\n\
         pub fn k_one() {}\npub fn k_two() {}\npub fn k_three() {}\n",
    );
    write_file(
        root,
        "src/cluster_c.rs",
        "pub fn gamma() {\n    m_one();\n    m_one();\n    m_one();\n}\npub fn m_one() {}\n",
    );
    index(&db, project_id, root).await;

    let groups = find_clone_groups(&db, project_id, 3).await.expect("find_clone_groups");
    assert_eq!(
        groups.len(),
        1,
        "exactly the constructed pair may group at min-edges 3, got {groups:?}"
    );
    let names: Vec<&str> = groups[0].members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta"], "the structural clone pair, names ignored");
    assert_eq!(groups[0].edge_count, 3);
    assert!(
        groups[0].members.iter().all(|m| m.start_line == Some(1)),
        "members must carry file:line display info, got {:?}",
        groups[0].members
    );

    // gamma's shape (3 calls to one target) must be a different fingerprint
    // — multiplicity toward one callee is not three distinct callees.
    assert!(
        !groups.iter().any(|g| g.members.iter().any(|m| m.name == "gamma")),
        "gamma has the same SIZE but not the same SHAPE"
    );
}

/// Threshold + triviality exclusions: below `min_edges` nothing reports
/// even when certificates match (every 1-caller leaf is "identical"), and
/// pure-containment shapes (zero non-`contains` edges) never report at any
/// threshold — that is arity, not behavior.
#[tokio::test]
async fn min_edges_threshold_and_containment_exclusion() {
    let db = mem_db().await;
    let project_id = "clones-thresholds";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Two identical leaf functions (1 caller each) — clones only in the
    // trivial sense; and two modules each containing one function — a pure
    // containment shape.
    write_file(
        root,
        "src/leaves.rs",
        "pub fn caller() {\n    leaf_a();\n    leaf_b();\n}\n\
         pub fn leaf_a() {}\npub fn leaf_b() {}\n",
    );
    write_file(root, "src/mod_one.rs", "pub mod one_inner {\n    pub fn f_one() {}\n}\n");
    write_file(root, "src/mod_two.rs", "pub mod two_inner {\n    pub fn f_two() {}\n}\n");
    index(&db, project_id, root).await;

    let strict = find_clone_groups(&db, project_id, 3).await.expect("min-edges 3");
    assert!(
        strict.is_empty(),
        "nothing here has >=3 edges of structure, got {strict:?}"
    );

    let loose = find_clone_groups(&db, project_id, 1).await.expect("min-edges 1");
    assert!(
        loose.iter().flat_map(|g| &g.members).any(|m| m.name == "leaf_a"),
        "at min-edges 1 the leaf pair legitimately reports, got {loose:?}"
    );
    assert!(
        !loose
            .iter()
            .flat_map(|g| &g.members)
            .any(|m| m.node_type == "module"),
        "pure-containment shapes are trivially same-shape and excluded at ANY \
         threshold, got {loose:?}"
    );
}
