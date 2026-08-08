//! Transitive dependency traversal — BFS following resolved edges of given
//! types, forward (`deps`) or backward (`rdeps`), from every node matching
//! a bare name. Like `call_chain`, split into a pure BFS core and a thin
//! SurrealDB adapter.
//!
//! Confidence policy (uniform with `call_chain`/`circular` — R2):
//! - `RESOLVED` edges drive the walk (default, always shown).
//! - `AMBIGUOUS` edges are surfaced — never walked further — only with
//!   `include_ambiguous`, labeled with their candidate sets (D3).
//! - A **forward** root's *own* direct `UNRESOLVED` edges are always shown,
//!   regardless of `include_ambiguous`: a caller whose call target vanished
//!   (e.g. a rename that missed a call site) is exactly the signal `impact`
//!   exists to catch — see `specs/resolution-layer-v1.md`'s "gate
//!   kill-test" and `tests/fixtures/rename-refactor/`. Silently dropping it
//!   would defeat the entire point. Deliberately scoped to the root itself,
//!   not every transitively-reached node: every non-trivial function calls
//!   several stdlib/external things, so surfacing *their* unresolved edges
//!   too would bury the one signal this exists for — empirically confirmed
//!   self-indexing this very repo (`deps --name index_project` at depth 2
//!   produced 800+ lines of stdlib noise before this was scoped down).
//! - A **reverse** query additionally checks, project-wide, for
//!   `UNRESOLVED` edges whose raw `to_name` still names the queried symbol
//!   under `resolve_one`'s own matching rule — bare tail for a bare
//!   capture, full qualified-suffix compatibility for a qualified one (see
//!   [`is_stale_candidate`]) — reported as `stale_references` on
//!   [`DependencyResult`]. This is the only way a
//!   renamed-away target's stale callers are ever visible: an `UNRESOLVED`
//!   edge has zero candidates and an empty `to_id` by definition (R6, zero
//!   survivors), so it can never be attributed to *any* node id — including
//!   the very id a `rdeps`/`impact` query on the *new* name would otherwise
//!   walk from. Querying by the *old* (renamed-away) name is the only way
//!   to see it, which is why this check runs unconditionally rather than
//!   waiting to be asked for.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::index::resolve::{bare_name, language_family, normalize_separators, ResolverNode};

use super::QueryEdge;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyNode {
    pub node_id: String,
    pub name: String,
    /// Added for the facade's `Dependent` projection (`src/facade.rs`),
    /// which needs a stable cross-file identity for each dependent, not
    /// just its bare name.
    pub qualified_name: String,
    pub node_type: String,
    pub file_path: String,
    pub depth: usize,
}

/// An `UNRESOLVED` edge — always surfaced (see module docs), never gated by
/// `include_ambiguous`. On [`DependencyGroup::unresolved`] this is scoped to
/// the group's own root (see module docs on why); on
/// [`DependencyResult::stale_references`] it's a project-wide bare-tail
/// text match instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedRef {
    pub from_name: String,
    pub from_file: String,
    pub to_name: String,
    pub to_type: String,
    /// Traversal depth at which this was found. `0` for
    /// [`DependencyResult::stale_references`], which is a flat, project-wide
    /// text scan rather than a BFS result — depth isn't meaningful there.
    pub depth: usize,
}

/// An `AMBIGUOUS` edge reached during traversal — surfaced only with
/// `include_ambiguous`, and never walked further (R6's "never guessed"
/// applies to traversal too).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousRef {
    pub from_name: String,
    pub from_file: String,
    pub to_name: String,
    pub to_type: String,
    pub candidates: Vec<String>,
    pub depth: usize,
}

/// One matched symbol's dependency (or reverse-dependency) subgraph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyGroup {
    /// Added for the facade's `MatchedSymbol` projection (`src/facade.rs`),
    /// which needs a stable id for the root, not just its qualified name.
    pub root_node_id: String,
    pub root_qualified_name: String,
    pub root_file: String,
    pub items: Vec<DependencyNode>,
    pub unresolved: Vec<UnresolvedRef>,
    pub ambiguous: Vec<AmbiguousRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DependencyResult {
    pub groups: Vec<DependencyGroup>,
    /// True when the queried name matched more than one distinct symbol
    /// (D3) — each got its own group and its own independent traversal
    /// rather than a blended one.
    pub name_ambiguous: bool,
    /// See module docs — populated only by `get_reverse_dependencies`.
    pub stale_references: Vec<UnresolvedRef>,
    /// How many live (non-import) definitions answered to the queried bare
    /// name when the stale scan ran — the exact `live` set
    /// [`find_stale_references`] matched against (which can differ from
    /// `groups`: roots match the raw queried string, the scan matches its
    /// bare tail). `0` is the renamed-away/deleted regime, where
    /// `stale_references` signal an incomplete rename; `> 0` means the
    /// captures were compatible with a still-live definition that the
    /// resolver nevertheless could not bind. Printers phrase the two
    /// regimes differently — the "no live symbol currently has this name"
    /// wording was once emitted unconditionally (disclosed in 833c17f).
    /// Populated only by `get_reverse_dependencies`, like
    /// `stale_references`.
    pub live_definitions: usize,
}

/// Find all transitive dependencies of every node named `start_name`
/// (forward edges), grouped per distinct symbol if the name collides (D3).
pub async fn get_dependencies(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    start_name: &str,
    edge_types: &[&str],
    max_depth: usize,
    include_ambiguous: bool,
) -> Result<DependencyResult> {
    let nodes = super::load_project_nodes(db, project_id).await?;
    let edges = super::load_project_edges(db, project_id, edge_types).await?;

    Ok(compute_dependencies(&nodes, &edges, start_name, max_depth, include_ambiguous))
}

/// Find all reverse dependencies (what depends ON every node named
/// `target_name`) — same grouping/confidence policy as [`get_dependencies`],
/// walked backward, plus the project-wide stale-reference check described
/// in the module docs.
pub async fn get_reverse_dependencies(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    target_name: &str,
    edge_types: &[&str],
    max_depth: usize,
    include_ambiguous: bool,
) -> Result<DependencyResult> {
    let nodes = super::load_project_nodes(db, project_id).await?;
    let edges = super::load_project_edges(db, project_id, edge_types).await?;

    Ok(compute_reverse_dependencies(
        &nodes,
        &edges,
        target_name,
        max_depth,
        include_ambiguous,
    ))
}

fn index_nodes(nodes: &[ResolverNode]) -> HashMap<&str, usize> {
    nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect()
}

/// Every node named `name` (excluding structural `import` nodes) — the D3
/// lookup every name-anchored traversal groups its results by, rather than
/// picking one match and blending or silently dropping the rest.
fn matching_roots(nodes: &[ResolverNode], name: &str) -> Vec<usize> {
    nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| (n.name == name && n.node_type != "import").then_some(i))
        .collect()
}

/// Pure BFS core for [`get_dependencies`] — no DB access, unit-testable.
fn compute_dependencies(
    nodes: &[ResolverNode],
    edges: &[QueryEdge],
    start_name: &str,
    max_depth: usize,
    include_ambiguous: bool,
) -> DependencyResult {
    let by_id = index_nodes(nodes);

    let mut out_resolved: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut out_ambiguous: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut out_unresolved: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        match e.confidence.as_str() {
            "RESOLVED" if !e.to_id.is_empty() => {
                out_resolved.entry(e.from_id.as_str()).or_default().push(i)
            }
            "AMBIGUOUS" => out_ambiguous.entry(e.from_id.as_str()).or_default().push(i),
            "UNRESOLVED" => out_unresolved.entry(e.from_id.as_str()).or_default().push(i),
            _ => {}
        }
    }

    let roots = matching_roots(nodes, start_name);
    let mut groups = Vec::with_capacity(roots.len());

    for root_idx in &roots {
        let root = &nodes[*root_idx];
        let mut items = Vec::new();
        let mut unresolved = Vec::new();
        let mut ambiguous = Vec::new();
        let mut visited: HashSet<usize> = HashSet::new();
        visited.insert(*root_idx);
        let mut frontier: VecDeque<(usize, usize)> = VecDeque::new();
        frontier.push_back((*root_idx, 0));

        while let Some((idx, depth)) = frontier.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let from = &nodes[idx];
            let id = from.id.as_str();

            for &ei in out_resolved.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                let e = &edges[ei];
                let Some(&ti) = by_id.get(e.to_id.as_str()) else {
                    continue;
                };
                if !visited.insert(ti) {
                    continue;
                }
                let to = &nodes[ti];
                items.push(DependencyNode {
                    node_id: to.id.clone(),
                    name: to.name.clone(),
                    qualified_name: to.qualified_name.clone(),
                    node_type: to.node_type.clone(),
                    file_path: to.file_path.clone(),
                    depth: depth + 1,
                });
                frontier.push_back((ti, depth + 1));
            }

            // Only the queried root's *own* direct unresolved edges — not
            // every transitively-reached node's (every non-trivial function
            // calls several stdlib/external things; surfacing those too
            // would bury the one signal this exists for under hundreds of
            // irrelevant "unresolved: HashMap::new" entries, empirically
            // confirmed self-indexing this very repo). The kill-test only
            // needs the specific symbol you asked about, not everything it
            // transitively touches.
            if idx == *root_idx {
                for &ei in out_unresolved.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    let e = &edges[ei];
                    unresolved.push(UnresolvedRef {
                        from_name: from.name.clone(),
                        from_file: from.file_path.clone(),
                        to_name: e.to_name.clone(),
                        to_type: e.to_type.clone(),
                        depth: depth + 1,
                    });
                }
            }

            if include_ambiguous {
                for &ei in out_ambiguous.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    let e = &edges[ei];
                    ambiguous.push(AmbiguousRef {
                        from_name: from.name.clone(),
                        from_file: from.file_path.clone(),
                        to_name: e.to_name.clone(),
                        to_type: e.to_type.clone(),
                        candidates: candidate_qualified_names(e, &by_id, nodes),
                        depth: depth + 1,
                    });
                }
            }
        }

        items.sort_by_key(|d| d.depth);
        groups.push(DependencyGroup {
            root_node_id: root.id.clone(),
            root_qualified_name: root.qualified_name.clone(),
            root_file: root.file_path.clone(),
            items,
            unresolved,
            ambiguous,
        });
    }

    DependencyResult {
        name_ambiguous: groups.len() > 1,
        groups,
        stale_references: Vec::new(),
        live_definitions: 0,
    }
}

/// Pure BFS core for [`get_reverse_dependencies`] — no DB access,
/// unit-testable.
fn compute_reverse_dependencies(
    nodes: &[ResolverNode],
    edges: &[QueryEdge],
    target_name: &str,
    max_depth: usize,
    include_ambiguous: bool,
) -> DependencyResult {
    let by_id = index_nodes(nodes);

    let mut in_resolved: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut in_ambiguous: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        match e.confidence.as_str() {
            "RESOLVED" if !e.to_id.is_empty() => {
                in_resolved.entry(e.to_id.as_str()).or_default().push(i)
            }
            "AMBIGUOUS" => {
                for c in &e.candidates {
                    in_ambiguous.entry(c.as_str()).or_default().push(i);
                }
            }
            _ => {}
        }
    }

    let roots = matching_roots(nodes, target_name);
    let mut groups = Vec::with_capacity(roots.len());

    for root_idx in &roots {
        let root = &nodes[*root_idx];
        let mut items = Vec::new();
        let mut ambiguous = Vec::new();
        let mut visited: HashSet<usize> = HashSet::new();
        visited.insert(*root_idx);
        let mut frontier: VecDeque<(usize, usize)> = VecDeque::new();
        frontier.push_back((*root_idx, 0));

        while let Some((idx, depth)) = frontier.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let target = &nodes[idx];
            let id = target.id.as_str();

            for &ei in in_resolved.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                let e = &edges[ei];
                let Some(&si) = by_id.get(e.from_id.as_str()) else {
                    continue;
                };
                if !visited.insert(si) {
                    continue;
                }
                let src = &nodes[si];
                items.push(DependencyNode {
                    node_id: src.id.clone(),
                    name: src.name.clone(),
                    qualified_name: src.qualified_name.clone(),
                    node_type: src.node_type.clone(),
                    file_path: src.file_path.clone(),
                    depth: depth + 1,
                });
                frontier.push_back((si, depth + 1));
            }

            if include_ambiguous {
                for &ei in in_ambiguous.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    let e = &edges[ei];
                    let Some(&si) = by_id.get(e.from_id.as_str()) else {
                        continue;
                    };
                    let src = &nodes[si];
                    ambiguous.push(AmbiguousRef {
                        from_name: src.name.clone(),
                        from_file: src.file_path.clone(),
                        to_name: e.to_name.clone(),
                        to_type: e.to_type.clone(),
                        candidates: candidate_qualified_names(e, &by_id, nodes),
                        depth: depth + 1,
                    });
                }
            }
        }

        items.sort_by_key(|d| d.depth);
        groups.push(DependencyGroup {
            root_node_id: root.id.clone(),
            root_qualified_name: root.qualified_name.clone(),
            root_file: root.file_path.clone(),
            items,
            unresolved: Vec::new(),
            ambiguous,
        });
    }

    let (stale_references, live_definitions) =
        find_stale_references(nodes, &by_id, edges, target_name);

    DependencyResult {
        name_ambiguous: groups.len() > 1,
        groups,
        stale_references,
        live_definitions,
    }
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

/// `UNRESOLVED` edges anywhere in the project that plausibly still refer to
/// `queried` — see module docs for why this is the only way a renamed-away
/// target's stale callers are ever visible.
///
/// The match rule mirrors `resolve::resolve_one`'s qualified-vs-bare split
/// exactly (see [`is_stale_candidate`]); it must, or the scan reports
/// "stale" for edges the resolver would never have bound to this symbol in
/// the first place. Measured before that alignment: a self-index of this
/// repo reported 38 stale references against an *unmodified*
/// `src/graph/dependencies.rs` — 37 of them the tree-sitter method call
/// `cursor.node()` bare-tailing onto this module's own `fn node()` test
/// helper, which is precisely the false binding `resolve_one`'s doc comment
/// cites as the reason a qualified name never degrades to its bare tail.
fn find_stale_references(
    nodes: &[ResolverNode],
    by_id: &HashMap<&str, usize>,
    edges: &[QueryEdge],
    queried: &str,
) -> (Vec<UnresolvedRef>, usize) {
    let queried_bare = bare_name(&normalize_separators(queried)).to_string();

    // Every live definition answering to the queried bare name — the same
    // set `matching_roots` groups by (imports excluded there too: an import
    // statement is a reference, not a definition). These are what a capture
    // has to be compatible with while the symbol still exists.
    let live: Vec<&ResolverNode> = nodes
        .iter()
        .filter(|n| n.name == queried_bare && n.node_type != "import")
        .collect();

    let stale = edges
        .iter()
        .filter(|e| e.confidence == "UNRESOLVED")
        .filter_map(|e| {
            let &i = by_id.get(e.from_id.as_str())?;
            let from = &nodes[i];
            if !is_stale_candidate(&e.to_name, &e.to_type, &from.language, &queried_bare, &live) {
                return None;
            }
            Some(UnresolvedRef {
                from_name: from.name.clone(),
                from_file: from.file_path.clone(),
                to_name: e.to_name.clone(),
                to_type: e.to_type.clone(),
                depth: 0,
            })
        })
        .collect();
    (stale, live.len())
}

/// Does one `UNRESOLVED` capture still plausibly refer to the symbol
/// `queried_bare` names? `raw_to_name`/`to_type` are the edge's verbatim
/// capture, `from_language` its source node's language (the cascade scopes
/// every candidate pool by the *calling* edge's language family, not the
/// target's), and `live` is every current definition of that bare name.
///
/// The rule is `resolve::resolve_one`'s, restated for a target that may no
/// longer exist. While the queried name still has live definitions, a
/// capture is only *about* one of them if the cascade would have routed it
/// there — same candidate-pool key (bare name, `to_type`, language family),
/// then:
/// - A **bare** capture is R3/R4/R5's territory, and every one of those
///   rules keys on the bare name alone — so a pool match is the whole test.
///   The pool key is not a formality: `Lcg { .. }`/`McpClient(...)` capture
///   as `to_type = "function"` while the live definition is a struct/class,
///   which is exactly why the cascade left them UNRESOLVED — they were
///   never routed to that symbol and are not stale references to it.
/// - A **qualified** capture is R1/R2's territory exclusively, and there it
///   only ever binds a node whose `qualified_name` *is* the capture or ends
///   with `::` + the capture. Bare-tailing it is the one fallback
///   `resolve_one` explicitly forbids, so this scan must forbid it too:
///   `cursor.node` (normalized `cursor::node`) is not a reference to
///   `graph::dependencies::tests::node`, and reporting it as a stale one
///   rejects a file nobody touched. The direction matters too — the capture
///   must be a *suffix* of a live qualified name (module-elided call), never
///   the other way round, or a crate-prefixed capture like
///   `codegraph::graph::dependencies::get_reverse_dependencies` would report
///   the perfectly healthy symbol it names as stale.
///
/// When the queried name has **no live definition at all** — the
/// renamed-away/deleted case this whole scan exists for (the gate reaches it
/// through `deleted_symbol`; see `facade::impact_verdict_for_paths`) — there
/// is nothing left to compare against, and the bare tail is the only signal
/// that survives the target's disappearance. That is the kill-test's shape
/// (`target::helper` queried as `helper`) and it stays matched
/// unconditionally.
///
/// Residual, deliberate: a rename whose bare name is *also* still defined
/// somewhere else in the project takes the strict branch, so a *qualified*
/// stale capture pointing at the vanished one can be missed. Closing that
/// needs the deleted symbol's own `qualified_name` threaded down here (it is
/// recorded — `deleted_symbol.qualified_name`), which is a signature change
/// through four call sites outside this module; not taken here. Bare
/// captures — what an incomplete rename inside one crate actually produces,
/// and what every deletion-tracking test asserts — are unaffected either
/// way.
fn is_stale_candidate(
    raw_to_name: &str,
    to_type: &str,
    from_language: &str,
    queried_bare: &str,
    live: &[&ResolverNode],
) -> bool {
    let normalized = normalize_separators(raw_to_name);
    if bare_name(&normalized) != queried_bare {
        return false;
    }
    if live.is_empty() {
        return true;
    }

    let family = language_family(from_language);
    let qualified = normalized.contains("::");
    let suffix = format!("::{normalized}");
    live.iter().any(|n| {
        n.node_type == to_type
            && language_family(&n.language) == family
            && (!qualified || n.qualified_name == normalized || n.qualified_name.ends_with(&suffix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, node_type: &str, file: &str, qn: &str) -> ResolverNode {
        ResolverNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: node_type.to_string(),
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

    /// D2: a resolved (qualified-call) edge is walked forward.
    #[test]
    fn forward_walks_resolved_edges() {
        let nodes = vec![
            node("caller", "use_stale", "function", "caller.rs", "caller::use_stale"),
            node("target", "helper", "function", "target.rs", "target::helper"),
        ];
        let edges = vec![edge("RESOLVED", "caller", "target", "", vec![])];
        let result = compute_dependencies(&nodes, &edges, "use_stale", 3, false);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].items.len(), 1);
        assert_eq!(result.groups[0].items[0].name, "helper");
    }

    /// The rename kill-test, at the unit level: a forward query on the
    /// caller must show its UNRESOLVED edge, unconditionally.
    #[test]
    fn forward_always_surfaces_unresolved_even_without_the_flag() {
        let nodes = vec![node("caller", "use_stale", "function", "caller.rs", "caller::use_stale")];
        let edges = vec![edge("UNRESOLVED", "caller", "", "target::helper", vec![])];

        let result = compute_dependencies(&nodes, &edges, "use_stale", 3, false);
        assert_eq!(result.groups[0].unresolved.len(), 1);
        assert_eq!(result.groups[0].unresolved[0].to_name, "target::helper");
        assert!(result.groups[0].items.is_empty());
    }

    /// D3: rdeps on a bare name matching multiple unrelated symbols returns
    /// one group per symbol, never a blend.
    #[test]
    fn reverse_groups_per_symbol_on_collision() {
        let nodes = vec![
            node("a1", "walk_calls", "function", "a.rs", "a::walk_calls"),
            node("a2", "walk_calls", "function", "b.rs", "b::walk_calls"),
            node("caller_a", "caller_a", "function", "ca.rs", "ca::caller_a"),
            node("caller_b", "caller_b", "function", "cb.rs", "cb::caller_b"),
        ];
        let edges = vec![
            edge("RESOLVED", "caller_a", "a1", "", vec![]),
            edge("RESOLVED", "caller_b", "a2", "", vec![]),
        ];
        let result = compute_reverse_dependencies(&nodes, &edges, "walk_calls", 3, false);
        assert_eq!(result.groups.len(), 2);
        assert!(result.name_ambiguous);
        for g in &result.groups {
            assert_eq!(g.items.len(), 1, "each symbol's group must only contain its own caller");
        }
    }

    /// AMBIGUOUS reverse edges are found via `candidates`, gated by the
    /// flag, and never advance the frontier.
    #[test]
    fn reverse_ambiguous_gated_by_flag() {
        let nodes = vec![
            node("target", "helper", "function", "t.rs", "t::helper"),
            node("other", "helper", "function", "o.rs", "o::helper"),
            node("caller", "caller", "function", "c.rs", "c::caller"),
        ];
        let edges = vec![edge("AMBIGUOUS", "caller", "", "helper", vec!["target", "other"])];

        let hidden = compute_reverse_dependencies(&nodes, &edges, "helper", 3, false);
        assert!(hidden.groups.iter().all(|g| g.ambiguous.is_empty() && g.items.is_empty()));

        let shown = compute_reverse_dependencies(&nodes, &edges, "helper", 3, true);
        let target_group = shown
            .groups
            .iter()
            .find(|g| g.root_qualified_name == "t::helper")
            .expect("target group present");
        assert_eq!(target_group.ambiguous.len(), 1);
        assert!(target_group.items.is_empty(), "ambiguous edges must never be traversed");
    }

    /// THE kill-test: after a rename, no live node answers to the old bare
    /// name, so `roots` is empty — but the stale caller's UNRESOLVED edge
    /// must still surface via the project-wide bare-tail scan.
    #[test]
    fn stale_reference_surfaces_when_target_renamed_away() {
        let nodes = vec![node(
            "stale_caller",
            "use_stale",
            "function",
            "stale_caller.rs",
            "stale_caller::use_stale",
        )];
        // helper_v2 exists now; the stale caller's edge still says "helper".
        let edges = vec![edge("UNRESOLVED", "stale_caller", "", "target::helper", vec![])];

        let result = compute_reverse_dependencies(&nodes, &edges, "helper", 3, false);
        assert!(result.groups.is_empty(), "no live symbol named 'helper' should mean zero groups");
        assert_eq!(result.stale_references.len(), 1);
        assert_eq!(result.stale_references[0].from_name, "use_stale");
        assert_eq!(result.stale_references[0].to_name, "target::helper");
        assert_eq!(
            result.live_definitions, 0,
            "the renamed-away regime must report zero live definitions — the printers key \
             the incomplete-rename wording on exactly this"
        );

        // Querying by the *new* name must NOT spuriously pick up the old
        // reference (it's a different bare tail).
        let miss = compute_reverse_dependencies(&nodes, &edges, "helper_v2", 3, false);
        assert!(miss.stale_references.is_empty());
    }

    /// The printer-bug regime (disclosed in 833c17f): a stale reference can
    /// coexist with live definitions of the same name — a bare capture whose
    /// candidate pool matches a live symbol but that the cascade still left
    /// UNRESOLVED. `live_definitions` must say so, because the old
    /// unconditional "no live symbol currently has this name" message was
    /// false exactly here.
    #[test]
    fn live_definitions_reported_alongside_stale_references() {
        let nodes = vec![
            node("target", "helper", "function", "target.rs", "target::helper"),
            node("caller", "use_stale", "function", "caller.rs", "caller::use_stale"),
        ];
        let edges = vec![edge("UNRESOLVED", "caller", "", "helper", vec![])];

        let result = compute_reverse_dependencies(&nodes, &edges, "helper", 3, false);
        assert_eq!(
            result.stale_references.len(),
            1,
            "a bare pool-matching capture is a stale candidate while the symbol lives"
        );
        assert_eq!(
            result.live_definitions, 1,
            "the live definition must be counted so the printer never claims no live \
             symbol has this name"
        );
    }

    /// The specificity half of the kill-test, at the unit level: a
    /// *qualified* capture must not bare-tail onto a live symbol that shares
    /// only its last segment. Production shape, and the exact false positive
    /// this scan shipped with: the tree-sitter method call `cursor.node()`
    /// (this module's own extractor walks a `TreeCursor`) reported as a
    /// stale reference to `graph::dependencies::tests::node` — 37 of the 38
    /// stale references a self-index produced against an *unmodified*
    /// `src/graph/dependencies.rs`. `resolve_one` refuses that same fallback
    /// (see its doc comment, which cites this very call site).
    #[test]
    fn qualified_capture_never_bare_tails_onto_a_live_symbol() {
        let nodes = vec![
            node("walker", "walk_calls", "function", "src/index/extractors/rust.rs", "extractors::rust::walk_calls"),
            node("helper", "node", "function", "src/graph/dependencies.rs", "graph::dependencies::tests::node"),
        ];
        let edges = vec![edge("UNRESOLVED", "walker", "", "cursor.node", vec![])];

        let result = compute_reverse_dependencies(&nodes, &edges, "node", 3, false);
        assert!(
            result.stale_references.is_empty(),
            "`cursor.node` is a method call on an external receiver, not a reference to \
             `graph::dependencies::tests::node`, got {:?}",
            result.stale_references
        );
    }

    /// The other direction of the same rule: a capture that spells the crate
    /// name out (`codegraph::graph::…`) is a *superset* of the live symbol's
    /// crate-relative `qualified_name`, which R1/R2 never match either — so
    /// it is an unresolved reference to a perfectly healthy symbol, not a
    /// stale one. Production shape: `src/main.rs` calls
    /// `codegraph::graph::dependencies::get_reverse_dependencies` fully
    /// qualified; that was stale reference #38.
    #[test]
    fn crate_prefixed_capture_of_a_live_symbol_is_not_stale() {
        let nodes = vec![
            node("caller", "run_query", "function", "src/main.rs", "run_query"),
            node(
                "target",
                "get_reverse_dependencies",
                "function",
                "src/graph/dependencies.rs",
                "graph::dependencies::get_reverse_dependencies",
            ),
        ];
        let edges = vec![edge(
            "UNRESOLVED",
            "caller",
            "",
            "codegraph::graph::dependencies::get_reverse_dependencies",
            vec![],
        )];

        let result = compute_reverse_dependencies(&nodes, &edges, "get_reverse_dependencies", 3, false);
        assert!(
            result.stale_references.is_empty(),
            "the symbol this capture names is right there and unrenamed, got {:?}",
            result.stale_references
        );
    }

    /// The candidate-pool key is part of the rule, not decoration: a
    /// struct-literal/constructor capture arrives as `to_type = "function"`,
    /// so the cascade's pool (bare name + `to_type` + language family) never
    /// contained the live *struct* of that name — which is why the edge is
    /// UNRESOLVED in the first place. Production shape: `Lcg { .. }` in
    /// `src/canon.rs`.
    #[test]
    fn pool_key_mismatch_is_not_a_stale_reference() {
        let nodes = vec![
            node("caller", "seeded", "function", "src/canon.rs", "canon::seeded"),
            node("lcg", "Lcg", "struct", "src/canon.rs", "canon::Lcg"),
        ];
        let edges = vec![edge("UNRESOLVED", "caller", "", "Lcg", vec![])];

        let result = compute_reverse_dependencies(&nodes, &edges, "Lcg", 3, false);
        assert!(
            result.stale_references.is_empty(),
            "a `function`-typed capture was never routed to a live `struct`, got {:?}",
            result.stale_references
        );

        // Sensitivity is unchanged for the pool that *does* match: once the
        // struct is gone, the same capture surfaces (nothing left to compare
        // against — the renamed-away branch).
        let renamed = vec![nodes[0].clone()];
        let gone = compute_reverse_dependencies(&renamed, &edges, "Lcg", 3, false);
        assert_eq!(gone.stale_references.len(), 1);
    }

    /// A cross-language bare collision is not a stale reference either:
    /// `language_family`'s closed table (Python and Go never share a
    /// namespace) is what keeps a Python `connect()` call from being
    /// reported as a dangling reference to a Go `connect`.
    #[test]
    fn cross_language_bare_collision_is_not_a_stale_reference() {
        let mut py_caller = node("py", "main", "function", "app.py", "app::main");
        py_caller.language = "python".to_string();
        let go_target = node("go", "connect", "function", "main.go", "main::connect");

        let nodes = vec![py_caller, go_target];
        let edges = vec![edge("UNRESOLVED", "py", "", "connect", vec![])];

        let result = compute_reverse_dependencies(&nodes, &edges, "connect", 3, false);
        assert!(
            result.stale_references.is_empty(),
            "no call syntax in either language crosses that boundary, got {:?}",
            result.stale_references
        );
    }
}
