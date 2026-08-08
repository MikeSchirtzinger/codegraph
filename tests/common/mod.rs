//! Shared helpers for the R3b integration/regression suite: spin up an
//! isolated in-memory store against the real schema, run one fixture
//! through the real `index_project` pipeline (which runs the R1 resolver as
//! its own last step — see `index::index_project`), load the resulting
//! nodes/edges back out, and a generic `expected.yaml`-driven assertion
//! harness. Schema documented in `tests/fixtures/README.md`; this module is
//! the "For R3b" section of that file, implemented.
//!
//! Every file under `tests/` pulls this in via `mod common;`; each is its
//! own crate and only exercises a subset of what's here, hence the blanket
//! `dead_code` allow below rather than fighting per-item false positives.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::index::{index_project, IndexConfig, IndexResult, IndexingTier};

// ============================================================================
// Store bootstrap
// ============================================================================

/// A fresh, fully isolated in-memory SurrealDB instance with the real schema
/// loaded (`src/schema.surql`, included verbatim so this can never drift
/// from what `codegraph::db::init_schema` runs — that function itself isn't
/// reachable from here, since `db` is a binary-only module (`mod db;` in
/// `main.rs`), not part of the crate's public lib API; this duplicates only
/// the couple of glue lines that run the DDL, never the DDL text itself).
/// `mem://` gives each call its own independent datastore, so every test
/// using this is parallel-safe with no project_id gymnastics required.
pub async fn fresh_db() -> Result<Arc<Surreal<Any>>> {
    let db = surrealdb::engine::any::connect("mem://")
        .await
        .context("connecting to in-memory SurrealDB failed")?;
    db.use_ns("codegraph")
        .use_db("codegraph")
        .await
        .context("selecting ns/db failed")?;
    db.query(include_str!("../../src/schema.surql"))
        .await
        .context("schema DDL failed")?
        .check()
        .context("schema DDL returned an error")?;
    Ok(Arc::new(db))
}

/// Resolve a path relative to the crate root to an absolute one,
/// independent of whatever cwd `cargo test` happens to run from.
pub fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Index one fixture project through the real pipeline. `index_project`
/// runs the R1 resolver pass as its own last step, so this one call
/// exercises index + resolve together. Always a fresh full index (`force:
/// true`) — every fixture here is meant to be indexed from nothing, never
/// incrementally re-indexed (R4's territory; the rename-refactor pair is
/// deliberately treated as two independent fresh indexes, per
/// `tests/fixtures/README.md`'s "For R3b" note).
pub async fn index_fixture(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    root_relative: &str,
) -> Result<IndexResult> {
    let config = IndexConfig {
        project_id: project_id.to_string(),
        root_path: repo_path(root_relative),
        tier: IndexingTier::Balanced,
        languages: None,
        force: true,
    };
    index_project(db, &config).await
}

// ============================================================================
// expected.yaml manifest schema (see tests/fixtures/README.md)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub fixture: String,
    #[serde(default)]
    pub language: String,
    pub root: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub nodes: Vec<NodeCase>,
    #[serde(default)]
    pub edges: Vec<EdgeCase>,
    #[serde(default)]
    pub cycles: Vec<CycleCase>,
}

#[derive(Debug, Deserialize)]
pub struct NodeCase {
    pub file: String,
    pub name: String,
    pub node_type: String,
    pub qualified_name: String,
}

#[derive(Debug, Deserialize)]
pub struct EdgeCase {
    pub case: String,
    #[serde(default)]
    pub defect: Option<String>,
    pub edge_type: String,
    pub from: String,
    pub from_file: String,
    pub to_name: String,
    pub to_type: String,
    pub confidence: String,
    pub resolved_by: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub candidates: Option<Vec<String>>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CycleCase {
    pub files: Vec<String>,
    pub via: OneOrMany,
}

/// `via` is always a bare scalar (`via: calls`) in every manifest today, but
/// the README frames it as "edge_type(s)" (plural-capable) — accept either
/// shape so a later multi-kind cycle doesn't force a harness change.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }
}

pub fn load_manifest(relative_path: &str) -> Manifest {
    let path = repo_path(relative_path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading manifest {}: {e}", path.display()));
    serde_yaml::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing manifest {}: {e}", path.display()))
}

/// The rename-refactor pair's parent manifest — a different top-level shape
/// (the before/after *delta*, not a per-project manifest; see
/// `tests/fixtures/README.md` "The rename-refactor pair — extra top-level
/// schema"). `before`/`after` are per-project manifests in their own right
/// and load fine via [`load_manifest`].
#[derive(Debug, Deserialize)]
pub struct RenameManifest {
    pub fixture: String,
    #[serde(default)]
    pub description: String,
    pub before: String,
    pub after: String,
    pub kill_test: KillTest,
}

#[derive(Debug, Deserialize)]
pub struct KillTest {
    pub qualified_name_before: String,
    pub qualified_name_after: String,
    pub callers: Vec<KillTestCaller>,
}

#[derive(Debug, Deserialize)]
pub struct KillTestCaller {
    pub from: String,
    pub from_file: String,
    pub before: CallerState,
    pub after: CallerState,
    pub must_flag: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallerState {
    pub confidence: String,
    pub resolved_by: String,
    #[serde(default)]
    pub target: Option<String>,
}

pub fn load_rename_manifest(relative_path: &str) -> RenameManifest {
    let path = repo_path(relative_path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading manifest {}: {e}", path.display()));
    serde_yaml::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing manifest {}: {e}", path.display()))
}

// ============================================================================
// DB row projections — same shape/parsing convention as `graph::QueryEdge`
// and `index::resolve::ResolverNode`, reimplemented locally since those
// helpers are `pub(crate)`/private to the lib and not reachable from an
// external integration-test crate.
// ============================================================================

#[derive(Debug, Clone)]
pub struct DbNode {
    pub node_id: String,
    pub qualified_name: String,
    pub file_path: String,
    pub name: String,
    pub node_type: String,
}

#[derive(Debug, Clone)]
pub struct DbEdge {
    pub from_id: String,
    pub to_id: String,
    pub to_name: String,
    pub to_type: String,
    pub edge_type: String,
    pub confidence: String,
    pub resolved_by: String,
    pub candidates: Vec<String>,
}

pub async fn load_all_nodes(db: &Surreal<Any>, project_id: &str) -> Result<Vec<DbNode>> {
    let mut resp = db
        .query(
            "SELECT node_id, name, node_type, file_path, qualified_name \
             FROM code_node WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading nodes failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let node_id = get_str(obj, "node_id");
            if node_id.is_empty() {
                return None;
            }
            Some(DbNode {
                node_id,
                name: get_str(obj, "name"),
                node_type: get_str(obj, "node_type"),
                file_path: get_str(obj, "file_path"),
                qualified_name: get_str(obj, "qualified_name"),
            })
        })
        .collect())
}

/// Every `code_edge` row for the project, whatever its `edge_type` —
/// deliberately not filtered to `calls` (unlike `graph::load_project_edges`,
/// which takes an explicit edge-type allowlist): a manifest-driven harness
/// should pick up a new edge kind (e.g. a `member_of` case) with zero code
/// changes. Rows shaped differently (`file_ref`, keyed on `from_file`/
/// `to_file` rather than `from_id`/`to_name`) just never match any
/// `from_id`+`to_name`+`to_type`+`edge_type` lookup a manifest case does.
pub async fn load_all_edges(db: &Surreal<Any>, project_id: &str) -> Result<Vec<DbEdge>> {
    let mut resp = db
        .query(
            "SELECT from_id, to_id, to_name, to_type, edge_type, confidence, \
             resolved_by, candidates FROM code_edge WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading edges failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            Some(DbEdge {
                from_id: get_str(obj, "from_id"),
                to_id: get_str(obj, "to_id"),
                to_name: get_str(obj, "to_name"),
                to_type: get_str(obj, "to_type"),
                edge_type: get_str(obj, "edge_type"),
                confidence: get_str(obj, "confidence"),
                resolved_by: get_str(obj, "resolved_by"),
                candidates: get_str_array(obj, "candidates"),
            })
        })
        .collect())
}

fn get_str(obj: &surrealdb_types::Object, key: &str) -> String {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn get_str_array(obj: &surrealdb_types::Object, key: &str) -> Vec<String> {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::Array(arr) => Some(
                arr.iter()
                    .filter_map(|item| match item {
                        surrealdb_types::Value::String(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

// ============================================================================
// Loaded project state + generic manifest assertions
// ============================================================================

/// One project's full node/edge set, loaded once and shared across every
/// assertion — mirrors the pure-core/DB-adapter split the rest of the
/// codebase uses (`index::resolve`, `graph::*`).
pub struct Loaded {
    pub nodes: Vec<DbNode>,
    pub edges: Vec<DbEdge>,
}

impl Loaded {
    pub async fn fetch(db: &Surreal<Any>, project_id: &str) -> Result<Self> {
        Ok(Self {
            nodes: load_all_nodes(db, project_id).await?,
            edges: load_all_edges(db, project_id).await?,
        })
    }

    pub fn by_qualified_name(&self, qn: &str) -> Option<&DbNode> {
        self.nodes.iter().find(|n| n.qualified_name == qn)
    }

    pub fn by_node_id(&self, id: &str) -> Option<&DbNode> {
        if id.is_empty() {
            return None;
        }
        self.nodes.iter().find(|n| n.node_id == id)
    }
}

/// Check every `nodes[]` entry: exactly one indexed node at the given
/// (file, name, node_type), with the expected computed `qualified_name`
/// (R0's accept criterion). Returns `(case count, failure messages)` —
/// deliberately collects every mismatch rather than stopping at the first
/// (via `assert_eq!`/`panic!`), so one bad case in a manifest can never hide
/// evidence about the rest when reporting a discrepancy.
pub fn check_nodes(manifest_nodes: &[NodeCase], loaded: &Loaded) -> (usize, Vec<String>) {
    let mut failures = Vec::new();
    for expect in manifest_nodes {
        let matches: Vec<&DbNode> = loaded
            .nodes
            .iter()
            .filter(|n| {
                n.file_path == expect.file
                    && n.name == expect.name
                    && n.node_type == expect.node_type
            })
            .collect();
        if matches.len() != 1 {
            failures.push(format!(
                "node {}:{} ({}): expected exactly one match, found {}",
                expect.file, expect.name, expect.node_type, matches.len()
            ));
            continue;
        }
        if matches[0].qualified_name != expect.qualified_name {
            failures.push(format!(
                "node {}:{}: qualified_name mismatch — got {:?}, want {:?}",
                expect.file, expect.name, matches[0].qualified_name, expect.qualified_name
            ));
        }
    }
    (manifest_nodes.len(), failures)
}

/// Check every `edges[]` entry: the resolver's binding for the case's exact
/// (from, to_name, to_type, edge_type) key matches confidence/resolved_by/
/// target/candidates (R1's accept criterion). `candidates` is compared as a
/// SET (order-insensitive). Per the manifest's own convention (README,
/// "Representation note"), `[] ≡ absent` for UNRESOLVED — never asserted as
/// a literal `[]` against the DB. Returns `(case count, failure messages)`
/// — see [`check_nodes`] for why this accumulates rather than panics inline.
pub fn check_edges(manifest_edges: &[EdgeCase], loaded: &Loaded) -> (usize, Vec<String>) {
    let mut failures = Vec::new();

    for expect in manifest_edges {
        let label = format!("edge {}", expect.case);
        let Some(from_node) = loaded.by_qualified_name(&expect.from) else {
            failures.push(format!(
                "{label}: caller qualified_name {:?} not found among indexed nodes",
                expect.from
            ));
            continue;
        };
        if from_node.file_path != expect.from_file {
            failures.push(format!(
                "{label}: from_file mismatch for caller {:?} — got {:?}, want {:?}",
                expect.from, from_node.file_path, expect.from_file
            ));
        }

        let matches: Vec<&DbEdge> = loaded
            .edges
            .iter()
            .filter(|e| {
                e.edge_type == expect.edge_type
                    && e.from_id == from_node.node_id
                    && e.to_name == expect.to_name
                    && e.to_type == expect.to_type
            })
            .collect();
        if matches.len() != 1 {
            failures.push(format!(
                "{label}: expected exactly one {} edge from {:?} with to_name {:?}, found {}",
                expect.edge_type, expect.from, expect.to_name, matches.len()
            ));
            continue;
        }
        let edge = matches[0];

        if edge.confidence != expect.confidence {
            failures.push(format!(
                "{label}: confidence mismatch — got {:?}, want {:?}",
                edge.confidence, expect.confidence
            ));
        }
        if edge.resolved_by != expect.resolved_by {
            failures.push(format!(
                "{label}: resolved_by mismatch — got {:?}, want {:?}",
                edge.resolved_by, expect.resolved_by
            ));
        }

        match expect.confidence.as_str() {
            "RESOLVED" => {
                let target_qn = loaded.by_node_id(&edge.to_id).map(|n| n.qualified_name.clone());
                if target_qn.as_deref() != expect.target.as_deref() {
                    failures.push(format!(
                        "{label}: target mismatch — got {target_qn:?}, want {:?}",
                        expect.target
                    ));
                }
            }
            "AMBIGUOUS" => {
                if !edge.to_id.is_empty() {
                    failures.push(format!("{label}: AMBIGUOUS edge must have empty to_id, got {:?}", edge.to_id));
                }
                let mut actual: Vec<String> = edge
                    .candidates
                    .iter()
                    .filter_map(|id| loaded.by_node_id(id).map(|n| n.qualified_name.clone()))
                    .collect();
                actual.sort();
                let mut expected = expect.candidates.clone().unwrap_or_default();
                expected.sort();
                if actual != expected {
                    failures.push(format!(
                        "{label}: candidates mismatch (compared as a set) — got {actual:?}, want {expected:?}"
                    ));
                }
            }
            "UNRESOLVED" => {
                if !edge.to_id.is_empty() {
                    failures.push(format!("{label}: UNRESOLVED edge must have empty to_id, got {:?}", edge.to_id));
                }
                if !edge.candidates.is_empty() {
                    failures.push(format!(
                        "{label}: UNRESOLVED edge must have empty/unset candidates, got {:?}",
                        edge.candidates
                    ));
                }
                if expect.target.is_some() {
                    failures.push(format!("{label}: manifest bug — UNRESOLVED case with a non-null target"));
                }
            }
            other => failures.push(format!("{label}: unknown confidence {other:?} in manifest")),
        }
    }
    (manifest_edges.len(), failures)
}

/// Check every `cycles[]` entry against the real `circular` query
/// (`graph::circular::detect_circular_deps` — the derived `file_ref` +
/// resolved-name-edge cross-file cycle detector, D4's fix). Returns
/// `(case count, failure messages)` — see [`check_nodes`] for why this
/// accumulates rather than panics inline.
pub async fn check_cycles(
    manifest_cycles: &[CycleCase],
    db: &Arc<Surreal<Any>>,
    project_id: &str,
) -> Result<(usize, Vec<String>)> {
    let detected = codegraph::graph::circular::detect_circular_deps(db, project_id).await?;
    let mut failures = Vec::new();
    for expect in manifest_cycles {
        if expect.files.len() != 2 {
            failures.push(format!(
                "cycle {:?}: this harness (and CircularDep) only model file-pair cycles; manifest listed {}",
                expect.files, expect.files.len()
            ));
            continue;
        }
        let want: HashSet<&str> = expect.files.iter().map(String::as_str).collect();
        let Some(found) = detected.iter().find(|c| {
            let got: HashSet<&str> = [c.file_a.as_str(), c.file_b.as_str()].into_iter().collect();
            got == want
        }) else {
            failures.push(format!(
                "cycle {:?}: not detected by the circular query; got {detected:?}",
                expect.files
            ));
            continue;
        };
        let want_via = expect.via.clone().into_vec();
        if found.via != want_via {
            failures.push(format!(
                "cycle {:?}: via mismatch — got {:?}, want {want_via:?}",
                expect.files, found.via
            ));
        }
    }
    Ok((manifest_cycles.len(), failures))
}

/// Case counts asserted for one fixture, for the completion report.
#[derive(Debug, Clone, Copy)]
pub struct FixtureCounts {
    pub nodes: usize,
    pub edges: usize,
    pub cycles: usize,
}

/// Run one per-project fixture through the full pipeline and check every
/// manifest case generically — the shape every `tests/fixtures/<x>/expected.yaml`
/// (and `rename-refactor/{before,after}/expected.yaml`, which follow the same
/// per-project schema — see `tests/fixtures/README.md`) reduces to. Every
/// case (nodes + edges + cycles) is checked and reported together — a
/// failure in one stage never hides evidence from the others.
pub async fn check_fixture(manifest_relpath: &str, project_id: &str) -> FixtureCounts {
    let manifest = load_manifest(manifest_relpath);
    let db = fresh_db().await.expect("fresh in-memory store");
    let result = index_fixture(&db, project_id, &manifest.root)
        .await
        .unwrap_or_else(|e| panic!("{}: indexing failed: {e:?}", manifest.fixture));

    let mut failures = Vec::new();
    if !result.errors.is_empty() {
        failures.push(format!("indexing reported errors: {:?}", result.errors));
    }

    let loaded = Loaded::fetch(&db, project_id)
        .await
        .unwrap_or_else(|e| panic!("{}: loading indexed data failed: {e:?}", manifest.fixture));

    let (nodes, node_failures) = check_nodes(&manifest.nodes, &loaded);
    failures.extend(node_failures);
    let (edges, edge_failures) = check_edges(&manifest.edges, &loaded);
    failures.extend(edge_failures);
    let (cycles, cycle_failures) = check_cycles(&manifest.cycles, &db, project_id)
        .await
        .unwrap_or_else(|e| panic!("{}: cycle query failed: {e:?}", manifest.fixture));
    failures.extend(cycle_failures);

    println!(
        "{}: {nodes} nodes, {edges} edges, {cycles} cycles checked, {} failure(s)",
        manifest.fixture, failures.len()
    );
    if !failures.is_empty() {
        panic!(
            "{}: {}/{} case(s) failed:\n  - {}",
            manifest.fixture,
            failures.len(),
            nodes + edges + cycles,
            failures.join("\n  - ")
        );
    }
    FixtureCounts { nodes, edges, cycles }
}
