//! Hub node detection — nodes with the highest degree (most connections).
//!
//! Surfaces architectural hotspots: the types/functions everything depends on.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubNode {
    pub node_id: String,
    pub name: String,
    pub node_type: String,
    pub file_path: String,
    pub in_degree: i64,
    pub out_degree: i64,
    pub total_degree: i64,
}

fn get_str(obj: &surrealdb_types::Object, key: &str) -> String {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn get_i64(obj: &surrealdb_types::Object, key: &str) -> i64 {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::Number(n) => n.to_int(),
            _ => None,
        })
        .unwrap_or(0)
}

/// Find the highest-degree nodes in a project.
///
/// Previously this ran a correlated subquery *per node*
/// (`(SELECT count() ... WHERE to_id = $parent.node_id ...)`), which is an
/// O(nodes × edges) full scan — the composite `code_edge` index does not get
/// pushed into a correlated subquery, so on ~1500 nodes this took minutes.
///
/// Instead: aggregate degrees with two `GROUP BY` queries over `code_edge`
/// (one pass each, index-friendly since they filter on the indexed
/// `project_id` prefix), then join against `code_node` in Rust. Mirrors the
/// approach in `graph::coupling`.
pub async fn find_hub_nodes(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    limit: usize,
) -> Result<Vec<HubNode>> {
    let pid = project_id.to_string();

    // Degrees are computed over *name-based* edges (calls / references /
    // implements — those carry `to_name`/`to_type`). Structural edges like
    // `contains` (to_name = NONE) are excluded: "who calls this" is the useful
    // signal, not "which module encloses this".
    //
    // In-degree: how many name-edges target each (name, node_type). We key by
    // name because a call edge's target file isn't known at parse time, so the
    // graph is resolved by name at query time rather than by a write-heavy
    // post-index id-resolution pass. Caveat: symbols sharing a (name, type)
    // share the aggregate in-degree (acceptable for a v1 INFERRED graph).
    let in_rows: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT to_name, to_type, count() AS c FROM code_edge \
             WHERE project_id = $pid AND to_name != NONE GROUP BY to_name, to_type",
        )
        .bind(("pid", pid.clone()))
        .await?
        .take(0)?;

    // Out-degree: how many name-edges originate FROM each node (from_id is a
    // real node_id). Restricted to name-edges too, so in/out are symmetric.
    let out_rows: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT from_id, count() AS c FROM code_edge \
             WHERE project_id = $pid AND to_name != NONE GROUP BY from_id",
        )
        .bind(("pid", pid.clone()))
        .await?
        .take(0)?;

    let mut in_degree: HashMap<(String, String), i64> = HashMap::new();
    for val in &in_rows {
        if let surrealdb_types::Value::Object(obj) = val {
            let name = get_str(obj, "to_name");
            let ntype = get_str(obj, "to_type");
            if !name.is_empty() {
                in_degree.insert((name, ntype), get_i64(obj, "c"));
            }
        }
    }

    let mut out_degree: HashMap<String, i64> = HashMap::new();
    for val in &out_rows {
        if let surrealdb_types::Value::Object(obj) = val {
            let id = get_str(obj, "from_id");
            if !id.is_empty() {
                out_degree.insert(id, get_i64(obj, "c"));
            }
        }
    }

    let nodes: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT node_id, name, node_type, file_path FROM code_node \
             WHERE project_id = $pid AND node_type != 'import'",
        )
        .bind(("pid", pid))
        .await?
        .take(0)?;

    let mut hubs = Vec::with_capacity(nodes.len());
    for val in &nodes {
        if let surrealdb_types::Value::Object(obj) = val {
            let node_id = get_str(obj, "node_id");
            let name = get_str(obj, "name");
            let node_type = get_str(obj, "node_type");
            let in_d = *in_degree
                .get(&(name.clone(), node_type.clone()))
                .unwrap_or(&0);
            let out_d = *out_degree.get(&node_id).unwrap_or(&0);

            hubs.push(HubNode {
                node_id,
                name,
                node_type,
                file_path: get_str(obj, "file_path"),
                in_degree: in_d,
                out_degree: out_d,
                total_degree: in_d + out_d,
            });
        }
    }

    hubs.sort_by_key(|h| std::cmp::Reverse(h.total_degree));
    hubs.truncate(limit);

    Ok(hubs)
}
