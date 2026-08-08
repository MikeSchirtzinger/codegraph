//! Call chain tracing — follow `calls` edges recursively from a starting
//! function name.
//!
//! Split like `index::resolve`: a pure, DB-free core (`compute_call_chains`)
//! that only ever sees already-loaded nodes/edges, and a thin SurrealDB
//! adapter (`trace_calls`) that loads one project's data and calls it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::index::resolve::ResolverNode;

use super::QueryEdge;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallChainEntry {
    pub depth: usize,
    pub caller_name: String,
    pub callee_name: String,
    pub caller_file: String,
    pub callee_file: String,
}

/// A `calls` edge seen while walking the chain whose target couldn't be
/// pinned to a single node — surfaced only with `--include-ambiguous`, and
/// (like every AMBIGUOUS edge) never walked further: R6's "never guessed"
/// rule applies to traversal, not just resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousCall {
    pub depth: usize,
    pub caller_name: String,
    pub caller_file: String,
    pub to_name: String,
    pub to_type: String,
    pub candidates: Vec<String>,
}

/// One starting function's call chain, plus whatever ambiguous calls were
/// seen while walking it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallChainGroup {
    pub root_qualified_name: String,
    pub root_file: String,
    pub entries: Vec<CallChainEntry>,
    pub ambiguous: Vec<AmbiguousCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CallChainResult {
    pub groups: Vec<CallChainGroup>,
    /// True when `function_name` itself matched more than one distinct
    /// function in the project (D3) — each got its own group and its own
    /// independent BFS rather than being blended into one chain.
    pub name_ambiguous: bool,
}

/// Trace call chains starting from every function named `function_name`
/// (BFS, depth-limited), walking only `RESOLVED` `calls` edges — the
/// resolver (R1) has already bound every callable name-edge it could; this
/// just reads that shape directly (D1's fix: `to_id` is a real node id, not
/// an unresolved name-edge placeholder). `--include-ambiguous` additionally
/// surfaces `AMBIGUOUS` calls encountered along the way, clearly labeled
/// and never traversed into.
pub async fn trace_calls(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    function_name: &str,
    max_depth: usize,
    include_ambiguous: bool,
) -> Result<CallChainResult> {
    let nodes = super::load_project_nodes(db, project_id).await?;
    let edges = super::load_project_edges(db, project_id, &["calls"]).await?;

    Ok(compute_call_chains(
        &nodes,
        &edges,
        function_name,
        max_depth,
        include_ambiguous,
    ))
}

/// Pure BFS core — no DB access, so it's directly unit-testable. `nodes`/
/// `edges` are expected to already be scoped to one project (the SurrealDB
/// adapter above does that).
fn compute_call_chains(
    nodes: &[ResolverNode],
    edges: &[QueryEdge],
    function_name: &str,
    max_depth: usize,
    include_ambiguous: bool,
) -> CallChainResult {
    let by_id: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

    let mut out_resolved: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut out_ambiguous: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        match e.confidence.as_str() {
            "RESOLVED" if !e.to_id.is_empty() => {
                out_resolved.entry(e.from_id.as_str()).or_default().push(i)
            }
            "AMBIGUOUS" => out_ambiguous.entry(e.from_id.as_str()).or_default().push(i),
            _ => {}
        }
    }

    let roots: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| (n.name == function_name && n.node_type == "function").then_some(i))
        .collect();

    let mut groups = Vec::with_capacity(roots.len());
    for root_idx in &roots {
        let root = &nodes[*root_idx];
        let mut entries = Vec::new();
        let mut ambiguous = Vec::new();
        let mut visited: HashSet<usize> = HashSet::new();
        visited.insert(*root_idx);
        let mut frontier: VecDeque<(usize, usize)> = VecDeque::new();
        frontier.push_back((*root_idx, 0));

        while let Some((idx, depth)) = frontier.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let caller = &nodes[idx];
            let id = caller.id.as_str();

            for &ei in out_resolved.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                let e = &edges[ei];
                let Some(&ti) = by_id.get(e.to_id.as_str()) else {
                    continue;
                };
                if !visited.insert(ti) {
                    continue;
                }
                let callee = &nodes[ti];
                entries.push(CallChainEntry {
                    depth: depth + 1,
                    caller_name: caller.name.clone(),
                    callee_name: callee.name.clone(),
                    caller_file: caller.file_path.clone(),
                    callee_file: callee.file_path.clone(),
                });
                frontier.push_back((ti, depth + 1));
            }

            if include_ambiguous {
                for &ei in out_ambiguous.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    let e = &edges[ei];
                    ambiguous.push(AmbiguousCall {
                        depth: depth + 1,
                        caller_name: caller.name.clone(),
                        caller_file: caller.file_path.clone(),
                        to_name: e.to_name.clone(),
                        to_type: e.to_type.clone(),
                        candidates: candidate_qualified_names(e, &by_id, nodes),
                    });
                }
            }
        }

        entries.sort_by_key(|e| e.depth);
        groups.push(CallChainGroup {
            root_qualified_name: root.qualified_name.clone(),
            root_file: root.file_path.clone(),
            entries,
            ambiguous,
        });
    }

    let name_ambiguous = groups.len() > 1;
    CallChainResult { groups, name_ambiguous }
}

fn candidate_qualified_names(
    e: &QueryEdge,
    by_id: &HashMap<&str, usize>,
    nodes: &[ResolverNode],
) -> Vec<String> {
    e.candidates
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).map(|&i| nodes[i].qualified_name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, file: &str, qn: &str) -> ResolverNode {
        ResolverNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: "function".to_string(),
            language: "rust".to_string(),
            file_path: file.to_string(),
            qualified_name: qn.to_string(),
        }
    }

    fn edge(confidence: &str, from: &str, to_id: &str, to_name: &str, candidates: Vec<&str>) -> QueryEdge {
        QueryEdge {
            from_id: from.to_string(),
            to_id: to_id.to_string(),
            to_name: to_name.to_string(),
            to_type: "function".to_string(),
            edge_type: "calls".to_string(),
            confidence: confidence.to_string(),
            candidates: candidates.into_iter().map(str::to_string).collect(),
        }
    }

    /// D1: a RESOLVED `calls` edge (real `to_id`) is walked and reported.
    #[test]
    fn walks_resolved_edges_and_sorts_by_depth() {
        let nodes = vec![
            node("main", "main", "main.rs", "main"),
            node("a", "a", "a.rs", "a"),
            node("b", "b", "b.rs", "b"),
        ];
        let edges = vec![
            edge("RESOLVED", "main", "a", "", vec![]),
            edge("RESOLVED", "a", "b", "", vec![]),
        ];
        let result = compute_call_chains(&nodes, &edges, "main", 5, false);
        assert_eq!(result.groups.len(), 1);
        let entries = &result.groups[0].entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].depth, 1);
        assert_eq!(entries[1].depth, 2);
        assert_eq!(entries[1].callee_name, "b");
    }

    /// D1 regression guard: an UNRESOLVED edge (`to_id` empty) is never
    /// walked or counted, matching real pre-resolver behavior for external/
    /// stdlib calls.
    #[test]
    fn unresolved_edges_are_never_walked() {
        let nodes = vec![node("main", "main", "main.rs", "main")];
        let edges = vec![edge("UNRESOLVED", "main", "", "println", vec![])];
        let result = compute_call_chains(&nodes, &edges, "main", 5, false);
        assert!(result.groups[0].entries.is_empty());
    }

    /// D3: a bare name matching two distinct functions produces two
    /// independent groups, not one blended traversal.
    #[test]
    fn ambiguous_start_name_produces_one_group_per_symbol() {
        let nodes = vec![
            node("a1", "helper", "alpha.rs", "alpha::helper"),
            node("a2", "helper", "beta.rs", "beta::helper"),
        ];
        let result = compute_call_chains(&nodes, &[], "helper", 3, false);
        assert_eq!(result.groups.len(), 2);
        assert!(result.name_ambiguous);
        let mut roots: Vec<&str> = result
            .groups
            .iter()
            .map(|g| g.root_qualified_name.as_str())
            .collect();
        roots.sort_unstable();
        assert_eq!(roots, vec!["alpha::helper", "beta::helper"]);
    }

    /// AMBIGUOUS calls are hidden by default, shown (with candidates) only
    /// with `include_ambiguous`, and never advance the BFS frontier.
    #[test]
    fn ambiguous_edges_gated_by_flag_and_never_traversed() {
        let nodes = vec![
            node("main", "main", "main.rs", "main"),
            node("a1", "helper", "alpha.rs", "alpha::helper"),
            node("a2", "helper", "beta.rs", "beta::helper"),
        ];
        let edges = vec![edge("AMBIGUOUS", "main", "", "helper", vec!["a1", "a2"])];

        let hidden = compute_call_chains(&nodes, &edges, "main", 3, false);
        assert!(hidden.groups[0].entries.is_empty());
        assert!(hidden.groups[0].ambiguous.is_empty());

        let shown = compute_call_chains(&nodes, &edges, "main", 3, true);
        assert!(shown.groups[0].entries.is_empty(), "ambiguous edges must never be traversed");
        assert_eq!(shown.groups[0].ambiguous.len(), 1);
        let mut candidates = shown.groups[0].ambiguous[0].candidates.clone();
        candidates.sort_unstable();
        assert_eq!(candidates, vec!["alpha::helper".to_string(), "beta::helper".to_string()]);
    }

    #[test]
    fn no_matching_function_returns_no_groups() {
        let nodes = vec![node("main", "main", "main.rs", "main")];
        let result = compute_call_chains(&nodes, &[], "nonexistent", 3, false);
        assert!(result.groups.is_empty());
        assert!(!result.name_ambiguous);
    }
}
