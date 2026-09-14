//! Code search — find nodes by name pattern, type, or file path.

use anyhow::Result;
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use super::GraphNode;

/// Search for nodes matching a query string (substring match on name).
pub async fn search_nodes(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    query: &str,
    node_type: Option<&str>,
    limit: usize,
) -> Result<Vec<GraphNode>> {
    // The ORDER BY in both queries below is load-bearing, not cosmetic: a
    // LIMIT over an unordered result set means *which* matches come back
    // depends on storage order, which is not stable across two indexes of
    // the same tree. Ordering before the limit is the only way the same
    // search returns the same symbols twice.
    let pid = project_id.to_string();
    let _pattern = format!("%{query}%");
    let limit_i = limit as i64;

    let rows: Vec<surrealdb_types::Value> = if let Some(ntype) = node_type {
        db.query(
            "SELECT node_id, name, node_type, file_path, language, start_line
             FROM code_node
             WHERE project_id = $pid AND name CONTAINS $q AND node_type = $ntype
             ORDER BY file_path, name, node_id
             LIMIT $lim",
        )
        .bind(("pid", pid))
        .bind(("q", query.to_string()))
        .bind(("ntype", ntype.to_string()))
        .bind(("lim", limit_i))
        .await?
        .take(0)?
    } else {
        db.query(
            "SELECT node_id, name, node_type, file_path, language, start_line
             FROM code_node
             WHERE project_id = $pid AND name CONTAINS $q AND node_type != 'import'
             ORDER BY file_path, name, node_id
             LIMIT $lim",
        )
        .bind(("pid", pid))
        .bind(("q", query.to_string()))
        .bind(("lim", limit_i))
        .await?
        .take(0)?
    };

    let mut results = Vec::new();
    for val in &rows {
        if let Some(node) = super::value_to_node(val) {
            results.push(node);
        }
    }

    Ok(results)
}

/// Get a summary of a project: node counts by type, file counts by language.
pub async fn project_summary(db: &Arc<Surreal<Any>>, project_id: &str) -> Result<ProjectSummary> {
    let pid = project_id.to_string();

    let type_rows: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT node_type, count() FROM code_node
             WHERE project_id = $pid GROUP BY node_type ORDER BY count DESC",
        )
        .bind(("pid", pid.clone()))
        .await?
        .take(0)?;

    let lang_rows: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT language, count() FROM code_node
             WHERE project_id = $pid GROUP BY language ORDER BY count DESC",
        )
        .bind(("pid", pid.clone()))
        .await?
        .take(0)?;

    let edge_rows: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT edge_type, count() FROM code_edge
             WHERE project_id = $pid GROUP BY edge_type ORDER BY count DESC",
        )
        .bind(("pid", pid))
        .await?
        .take(0)?;

    let mut node_types = Vec::new();
    for val in &type_rows {
        if let surrealdb_types::Value::Object(obj) = val {
            let nt = extract_str(obj, "node_type");
            let c = extract_i64(obj, "count");
            if !nt.is_empty() {
                node_types.push((nt, c));
            }
        }
    }

    let mut languages = Vec::new();
    for val in &lang_rows {
        if let surrealdb_types::Value::Object(obj) = val {
            let l = extract_str(obj, "language");
            let c = extract_i64(obj, "count");
            if !l.is_empty() {
                languages.push((l, c));
            }
        }
    }

    let mut edge_types = Vec::new();
    for val in &edge_rows {
        if let surrealdb_types::Value::Object(obj) = val {
            let et = extract_str(obj, "edge_type");
            let c = extract_i64(obj, "count");
            if !et.is_empty() {
                edge_types.push((et, c));
            }
        }
    }

    Ok(ProjectSummary {
        node_types,
        languages,
        edge_types,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectSummary {
    pub node_types: Vec<(String, i64)>,
    pub languages: Vec<(String, i64)>,
    pub edge_types: Vec<(String, i64)>,
}

fn extract_str(obj: &surrealdb_types::Object, key: &str) -> String {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn extract_i64(obj: &surrealdb_types::Object, key: &str) -> i64 {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::Number(n) => n.to_int(),
            _ => None,
        })
        .unwrap_or(0)
}
