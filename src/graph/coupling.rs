//! Coupling metrics per file.
//!
//! Afferent coupling (Ca): how many other files depend on this file
//! Efferent coupling (Ce): how many files this file depends on
//! Instability (I): Ce / (Ca + Ce) — 0.0 = maximally stable, 1.0 = maximally unstable

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCoupling {
    pub file_path: String,
    pub afferent: i64,
    pub efferent: i64,
    pub instability: f64,
    pub node_count: i64,
}

/// Calculate coupling metrics per file for a project.
pub async fn calculate_file_coupling(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    limit: usize,
) -> Result<Vec<FileCoupling>> {
    let pid = project_id.to_string();

    let get_str = |obj: &surrealdb_types::Object, key: &str| -> Option<String> {
        obj.get(key).and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
    };

    // Name-based edges only (calls/references/implements carry to_name/to_type);
    // the target file is resolved by name at query time.
    let edges: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT from_id, to_name, to_type FROM code_edge
             WHERE project_id = $pid AND to_name != NONE",
        )
        .bind(("pid", pid.clone()))
        .await?
        .take(0)?;

    // node_id -> file_path (source side) and (name, type) -> file_path (target
    // side; first match wins on ambiguity).
    let nodes: Vec<surrealdb_types::Value> = db
        .query("SELECT node_id, name, node_type, file_path FROM code_node WHERE project_id = $pid")
        .bind(("pid", pid.clone()))
        .await?
        .take(0)?;

    let mut node_to_file: HashMap<String, String> = HashMap::new();
    let mut name_to_file: HashMap<(String, String), String> = HashMap::new();
    let mut file_node_count: HashMap<String, i64> = HashMap::new();

    for val in &nodes {
        if let surrealdb_types::Value::Object(obj) = val {
            let fp = get_str(obj, "file_path");
            if let (Some(nid), Some(fp)) = (get_str(obj, "node_id"), fp.clone()) {
                node_to_file.insert(nid, fp.clone());
                *file_node_count.entry(fp).or_insert(0) += 1;
            }
            if let (Some(name), Some(ntype), Some(fp)) =
                (get_str(obj, "name"), get_str(obj, "node_type"), fp)
            {
                name_to_file.entry((name, ntype)).or_insert(fp);
            }
        }
    }

    // Count cross-file edges
    let mut afferent: HashMap<String, i64> = HashMap::new();
    let mut efferent: HashMap<String, i64> = HashMap::new();

    for val in &edges {
        if let surrealdb_types::Value::Object(obj) = val {
            let from_id = get_str(obj, "from_id");
            let to_name = get_str(obj, "to_name");
            let to_type = get_str(obj, "to_type");

            if let (Some(fid), Some(tname), Some(ttype)) = (from_id, to_name, to_type) {
                let from_file = node_to_file.get(&fid);
                let to_file = name_to_file.get(&(tname, ttype));

                if let (Some(ff), Some(tf)) = (from_file, to_file) {
                    if ff != tf {
                        // Cross-file edge: ff depends on tf
                        *efferent.entry(ff.clone()).or_insert(0) += 1;
                        *afferent.entry(tf.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    // Build results
    let all_files: std::collections::HashSet<&String> = file_node_count.keys().collect();
    let mut results: Vec<FileCoupling> = all_files
        .iter()
        .map(|fp| {
            let ca = *afferent.get(*fp).unwrap_or(&0);
            let ce = *efferent.get(*fp).unwrap_or(&0);
            let instability = if ca + ce > 0 {
                ce as f64 / (ca + ce) as f64
            } else {
                0.0
            };

            FileCoupling {
                file_path: fp.to_string(),
                afferent: ca,
                efferent: ce,
                instability,
                node_count: *file_node_count.get(*fp).unwrap_or(&0),
            }
        })
        .collect();

    // Sort by total coupling (Ca + Ce) descending
    results.sort_by_key(|b| std::cmp::Reverse(b.afferent + b.efferent));
    results.truncate(limit);

    Ok(results)
}
