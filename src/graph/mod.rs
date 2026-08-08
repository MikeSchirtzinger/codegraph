//! Graph analysis tools for querying the code graph.
//!
//! All queries are scoped by `project_id` and run against the
//! `code_node` / `code_edge` tables via SurrealQL.
//!
//! `call_chain`, `dependencies`, and `circular` all consume the R1 resolver's
//! materialized output uniformly (`specs/resolution-layer-v1.md` §R2): each
//! loads a project's nodes/edges once via [`load_project_nodes`] /
//! [`load_project_edges`] below, then traverses/analyzes them with a pure,
//! DB-free function — the same pure-core/DB-adapter split
//! `index::resolve` itself uses (and for the same reason: the interesting
//! logic becomes unit-testable without a live store, and a future caller
//! can rebuild the in-memory shape however it likes). `hub_nodes` and
//! `coupling` predate this split and still match by `(to_name, to_type)` at
//! query time — coarser aggregate metrics that don't need per-edge
//! identity, and out of R2's scope.

pub mod call_chain;
pub mod circular;
pub mod clones;
pub mod coupling;
pub mod dependencies;
pub mod hub_nodes;
pub mod search;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::index::resolve::ResolverNode;

/// Every edge_type an extractor ever emits as a name-edge (`to_id` starts
/// empty; the R1 resolver binds or triages it to
/// RESOLVED/AMBIGUOUS/UNRESOLVED). `contains` (pure structural containment)
/// and the Rust-only `imports`/macro-invocation-site edges are deliberately
/// excluded: they carry a real `to_id` from the moment an extractor writes
/// them (`parser::ExtractionContext::add_edge`, confidence `EXTRACTED`) and
/// never pass through the resolver, so they never carry a
/// RESOLVED/AMBIGUOUS/UNRESOLVED confidence at all — see
/// `index::extractors::rust::extract_macro_invocation`. `calls`, `deps`,
/// `rdeps`, `circular`, and MCP `impact` all read exactly this set, so
/// "every query consumes the resolved graph uniformly" is true of the code,
/// not just the intent.
pub const NAME_EDGE_TYPES: [&str; 3] = ["calls", "member_of", "implements"];

/// Load every `code_node` row for a project, projected to the fields graph
/// queries need to traverse and report on (id/name/type/language/file/
/// qualified_name) — the exact shape the resolver itself uses
/// (`index::resolve::ResolverNode`), reused here so a query and a
/// resolution pass never risk drifting to two different ideas of "what a
/// node is".
pub async fn load_project_nodes(db: &Surreal<Any>, project_id: &str) -> Result<Vec<ResolverNode>> {
    let mut resp = db
        .query(
            "SELECT node_id, name, node_type, language, file_path, qualified_name \
             FROM code_node WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading project nodes failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let id = get_str(obj, "node_id");
            if id.is_empty() {
                return None;
            }
            Some(ResolverNode {
                id,
                name: get_str(obj, "name"),
                node_type: get_str(obj, "node_type"),
                language: get_str(obj, "language"),
                file_path: get_str(obj, "file_path"),
                qualified_name: get_str(obj, "qualified_name"),
            })
        })
        .collect())
}

/// One `code_edge` row exactly as resolution leaves it, covering every
/// confidence level — unlike the resolver's own `UnresolvedEdge` (which
/// only ever sees pre-pass `to_id = ''` rows), a query needs `RESOLVED`,
/// `AMBIGUOUS`, and `UNRESOLVED` rows all at once so it can apply the
/// resolved-by-default / `--include-ambiguous` / always-visible-unresolved
/// policy itself, in Rust, rather than the DB filtering any of it away
/// before the query ever sees it.
#[derive(Debug, Clone)]
pub struct QueryEdge {
    pub from_id: String,
    pub to_id: String,
    pub to_name: String,
    pub to_type: String,
    pub edge_type: String,
    pub confidence: String,
    /// Populated only for `AMBIGUOUS` edges — survivor node ids, per the
    /// resolver's own `candidates` field.
    pub candidates: Vec<String>,
}

/// Load every `code_edge` row of the given `edge_types` for a project, at
/// every confidence level (see [`QueryEdge`]).
pub async fn load_project_edges(
    db: &Surreal<Any>,
    project_id: &str,
    edge_types: &[&str],
) -> Result<Vec<QueryEdge>> {
    let etypes: Vec<String> = edge_types.iter().map(|s| s.to_string()).collect();
    let mut resp = db
        .query(
            "SELECT from_id, to_id, to_name, to_type, edge_type, confidence, candidates \
             FROM code_edge WHERE project_id = $pid AND edge_type IN $etypes",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("etypes", etypes))
        .await
        .context("loading project edges failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let from_id = get_str(obj, "from_id");
            if from_id.is_empty() {
                return None;
            }
            Some(QueryEdge {
                from_id,
                to_id: get_str(obj, "to_id"),
                to_name: get_str(obj, "to_name"),
                to_type: get_str(obj, "to_type"),
                edge_type: get_str(obj, "edge_type"),
                confidence: get_str(obj, "confidence"),
                candidates: get_str_array(obj, "candidates"),
            })
        })
        .collect())
}

pub(crate) fn get_str(obj: &surrealdb_types::Object, key: &str) -> String {
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

/// A node returned from graph queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub node_id: String,
    pub name: String,
    pub node_type: String,
    pub file_path: String,
    pub language: String,
    pub start_line: Option<i64>,
}

/// An edge returned from graph queries.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from_name: String,
    pub to_name: String,
    pub edge_type: String,
    pub confidence: String,
}

/// Helper to extract GraphNode fields from a SurrealDB Value.
pub fn value_to_node(val: &surrealdb_types::Value) -> Option<GraphNode> {
    if let surrealdb_types::Value::Object(obj) = val {
        let get_str = |key: &str| -> String {
            obj.get(key)
                .and_then(|v| match v {
                    surrealdb_types::Value::String(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let get_int = |key: &str| -> Option<i64> {
            obj.get(key).and_then(|v| match v {
                surrealdb_types::Value::Number(n) => n.to_int(),
                _ => None,
            })
        };

        Some(GraphNode {
            node_id: get_str("node_id"),
            name: get_str("name"),
            node_type: get_str("node_type"),
            file_path: get_str("file_path"),
            language: get_str("language"),
            start_line: get_int("start_line"),
        })
    } else {
        None
    }
}
