//! Circular dependency detection.
//!
//! Finds pairs of files where A depends on B and B depends on A, over the
//! union of two sources (D4's fix — R2): (a) every `RESOLVED` name-edge
//! (`calls`/`member_of`/`implements`), joined to file paths, which also
//! gives the per-direction edge-kind breakdown (`via`); and (b) the derived
//! `file_ref` graph the resolver writes from those same bindings
//! (`index::resolve::resolve_project`). `file_ref` is language-agnostic by
//! construction (it's just cross-file RESOLVED pairs, whatever edge kind
//! produced them), so cross-file cycle detection now works uniformly across
//! all 6 languages instead of being effectively calls-only-and-broken.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use super::{get_str, NAME_EDGE_TYPES};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircularDep {
    pub file_a: String,
    pub file_b: String,
    pub a_to_b_edges: i64,
    pub b_to_a_edges: i64,
    /// Which edge kind(s), once RESOLVED, produced this cycle
    /// (`calls`/`member_of`/`implements`, any combination). Falls back to
    /// `["file_ref"]` for a pair found only via the derived graph with no
    /// matching name-edge breakdown — shouldn't happen given today's
    /// sources, but stays honest about it rather than guessing.
    pub via: Vec<String>,
}

/// Detect circular dependencies between files.
pub async fn detect_circular_deps(db: &Arc<Surreal<Any>>, project_id: &str) -> Result<Vec<CircularDep>> {
    let nodes = super::load_project_nodes(db, project_id).await?;
    let name_edges = super::load_project_edges(db, project_id, &NAME_EDGE_TYPES).await?;
    let file_ref_pairs = load_file_ref_pairs(db, project_id).await?;

    let node_to_file: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.file_path.as_str()))
        .collect();

    let resolved_file_edges: Vec<(String, String, String)> = name_edges
        .iter()
        .filter(|e| e.confidence == "RESOLVED" && !e.to_id.is_empty())
        .filter_map(|e| {
            let from_file = node_to_file.get(e.from_id.as_str())?;
            let to_file = node_to_file.get(e.to_id.as_str())?;
            (from_file != to_file)
                .then(|| (from_file.to_string(), to_file.to_string(), e.edge_type.clone()))
        })
        .collect();

    Ok(find_cycles(&resolved_file_edges, &file_ref_pairs))
}

async fn load_file_ref_pairs(db: &Surreal<Any>, project_id: &str) -> Result<Vec<(String, String)>> {
    let mut resp = db
        .query("SELECT from_file, to_file FROM code_edge WHERE project_id = $pid AND edge_type = 'file_ref'")
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading file_ref edges failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let ff = get_str(obj, "from_file");
            let tf = get_str(obj, "to_file");
            (!ff.is_empty() && !tf.is_empty()).then_some((ff, tf))
        })
        .collect())
}

/// Pure cycle-finder — no DB access, unit-testable. `name_edges` are
/// `(from_file, to_file, edge_type)` triples (already cross-file only);
/// `file_ref_pairs` are `(from_file, to_file)` pairs straight from the
/// derived graph.
fn find_cycles(
    name_edges: &[(String, String, String)],
    file_ref_pairs: &[(String, String)],
) -> Vec<CircularDep> {
    let mut breakdown: HashMap<(String, String), HashMap<String, i64>> = HashMap::new();
    for (from_file, to_file, edge_type) in name_edges {
        *breakdown
            .entry((from_file.clone(), to_file.clone()))
            .or_default()
            .entry(edge_type.clone())
            .or_insert(0) += 1;
    }
    // Defensive: make sure a file_ref-only pair (no matching name-edge
    // breakdown — not possible today, but file_ref is the authoritative
    // graph) still participates in detection below.
    for (from_file, to_file) in file_ref_pairs {
        breakdown.entry((from_file.clone(), to_file.clone())).or_default();
    }

    let keys: Vec<(String, String)> = breakdown.keys().cloned().collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut circular = Vec::new();

    for key in &keys {
        let reverse = (key.1.clone(), key.0.clone());
        let Some(bwd) = breakdown.get(&reverse) else {
            continue;
        };
        let pair_key = if key.0 < key.1 { key.clone() } else { reverse.clone() };
        if !seen.insert(pair_key.clone()) {
            continue;
        }

        let fwd = &breakdown[key];
        let (ab, ba) = if *key == pair_key { (fwd, bwd) } else { (bwd, fwd) };

        let mut via: Vec<String> = ab
            .keys()
            .chain(ba.keys())
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if via.is_empty() {
            via.push("file_ref".to_string());
        }
        via.sort();

        circular.push(CircularDep {
            file_a: pair_key.0,
            file_b: pair_key.1,
            a_to_b_edges: ab.values().sum(),
            b_to_a_edges: ba.values().sum(),
            via,
        });
    }

    circular.sort_by(|x, y| {
        (y.a_to_b_edges + y.b_to_a_edges)
            .cmp(&(x.a_to_b_edges + x.b_to_a_edges))
            .then_with(|| x.file_a.cmp(&y.file_a))
            .then_with(|| x.file_b.cmp(&y.file_b))
    });
    circular
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(pairs: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        pairs
            .iter()
            .map(|(a, b, t)| (a.to_string(), b.to_string(), t.to_string()))
            .collect()
    }

    /// D4: a reciprocal pair of resolved `calls` edges is a cross-file cycle.
    #[test]
    fn detects_reciprocal_calls_cycle() {
        let name_edges = edges(&[("a.rs", "b.rs", "calls"), ("b.rs", "a.rs", "calls")]);
        let cycles = find_cycles(&name_edges, &[]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].file_a, "a.rs");
        assert_eq!(cycles[0].file_b, "b.rs");
        assert_eq!(cycles[0].a_to_b_edges, 1);
        assert_eq!(cycles[0].b_to_a_edges, 1);
        assert_eq!(cycles[0].via, vec!["calls".to_string()]);
    }

    #[test]
    fn one_directional_reference_is_not_a_cycle() {
        let name_edges = edges(&[("a.rs", "b.rs", "calls")]);
        assert!(find_cycles(&name_edges, &[]).is_empty());
    }

    /// A cycle can be formed from two *different* resolved edge kinds
    /// (e.g. a Go `member_of` one way, a `calls` the other) — neither alone
    /// is reciprocal, but together they are, and `via` reports both.
    #[test]
    fn cycle_can_combine_different_edge_kinds() {
        let name_edges = edges(&[("a.rs", "b.rs", "member_of"), ("b.rs", "a.rs", "calls")]);
        let cycles = find_cycles(&name_edges, &[]);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].via, vec!["calls".to_string(), "member_of".to_string()]);
    }

    #[test]
    fn dedupes_regardless_of_which_direction_is_seen_first() {
        let name_edges = edges(&[("b.rs", "a.rs", "calls"), ("a.rs", "b.rs", "calls")]);
        assert_eq!(find_cycles(&name_edges, &[]).len(), 1);
    }

    /// file_ref alone (no name-edge breakdown) still detects a cycle, with
    /// an honest `via` fallback.
    #[test]
    fn file_ref_only_pair_still_detected() {
        let file_ref_pairs = vec![("a.rs".to_string(), "b.rs".to_string()), ("b.rs".to_string(), "a.rs".to_string())];
        let cycles = find_cycles(&[], &file_ref_pairs);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].via, vec!["file_ref".to_string()]);
    }
}
