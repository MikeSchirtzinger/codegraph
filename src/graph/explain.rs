//! Machine-checkable evidence chains for structural findings
//! (`specs/explain-v1.md`).
//!
//! A finding on its own is an assertion: "this reference is stale", "the
//! resolver refused to guess here", "this symbol has 12 dependents". A
//! *chain* is the derivation behind it, decomposed into links that a second
//! program can re-check against the graph without trusting whatever
//! produced them.
//!
//! Two rules give that claim its teeth, and both are enforced by the types
//! rather than by convention:
//!
//! 1. **Every step is a [`Fact`], and every `Fact` has a checker.** There is
//!    no free-text step. A chain carries no sentence that a reader has to
//!    take on faith, because there is nowhere to put one: rendering prose
//!    for human display happens at print time, derived *from* the facts (see
//!    [`Fact::describe`]), never stored alongside them.
//! 2. **Checking re-derives; it does not compare against a copy.**
//!    [`verify_chain`] re-runs the real resolver cascade
//!    ([`resolve::resolve_one_traced`]) and recomputes candidate pools and
//!    live-definition sets from the node set. A chain that agrees with a
//!    tampered recording still fails, because the recording is not what it
//!    is checked against.
//!
//! That second rule is the difference between a receipt and a certificate,
//! and it is why [`ExplainGraph`] holds the resolver's own `Indices`: the
//! verifier has to be able to ask the cascade what it does, not what some
//! row says it did.
//!
//! **S.1 is a link, not a preamble.** Both chain shapes that start from a
//! symbol begin by pinning *how that symbol entered the query*, either as
//! an explicit literal argument or as a `deleted_symbol` record. A chain
//! whose every later link is exact but whose first step was a fuzzy match
//! derives a clean-looking path from a possibly wrong origin, and nothing
//! downstream can detect it. [`Membership`] has exactly two variants and
//! neither admits a similarity match; see `specs/explain-v1.md` §7 for why
//! that is the load-bearing difference from the nearest prior art.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use tokio::sync::Mutex;

use crate::index::resolve::{
    self, bare_name, language_family, normalize_separators, Indices, ResolverNode, UnresolvedEdge,
};

use super::dependencies::{self, ReversePath};
use super::QueryEdge;

/// Bumped when the chain wire format changes in a way a consumer would
/// notice. Carried on every chain so a stored chain can be read back by a
/// later version that knows what it is looking at.
///
/// **2** (2026-09-14): the [`Fact`] discriminator moved from `fact` to
/// `kind`. A v1 chain does not deserialize into a v2 `Fact` at all, so
/// without the bump a saved chain would come back as a serde
/// missing-field error and a reader would have no way to tell "this
/// evidence is forged" from "this tool is newer than the report". Those two
/// must never be confused: an audit deliverable is meant to be re-verified
/// by a client months later, and telling them their evidence failed when
/// the real answer is a version skew would be a false accusation dressed as
/// a machine check. [`verify_chain_json`] reads the version before
/// attempting the parse for exactly that reason.
///
/// **1**: initial format.
pub const EXPLAIN_VERSION: u32 = 2;

/// Traversal depth for chain shape D, fixed to match the facade's
/// `DEFAULT_DEPTH` (`specs/explain-v1.md` §6: no depth parameter).
pub const EXPLAIN_DEPTH: usize = 3;

// ============================================================================
// The graph a chain is checked against
// ============================================================================

/// A `code_node` row plus `start_line`, which chains need and
/// [`ResolverNode`] does not carry (the cascade never looks at line
/// numbers, so the resolver's projection has no reason to load them; a
/// finding a human has to go fix does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainNode {
    pub node_id: String,
    pub name: String,
    pub node_type: String,
    pub language: String,
    pub file_path: String,
    pub qualified_name: String,
    pub start_line: Option<i64>,
}

/// A `code_edge` name-edge row including the resolver's full attribution.
///
/// Deliberately a separate projection from [`QueryEdge`] rather than extra
/// fields on it: `QueryEdge` is constructed literally in `index::fingerprint`
/// (a frozen module) and consumed by concurrently-developed lanes, so
/// widening it would break code this lane must not touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainEdge {
    pub from_id: String,
    pub to_id: String,
    pub to_name: String,
    pub to_type: String,
    pub edge_type: String,
    pub confidence: String,
    pub resolved_by: String,
    pub candidates: Vec<String>,
    pub attempted_rules: Vec<String>,
    pub resolution_outcome: String,
}

impl ExplainNode {
    /// Compact one-line rendering for a verdict reason.
    ///
    /// Verdict reasons are wire-facing: they go out in JSON, an agent reads
    /// them, and a client re-verifying an audit may see them. A Rust `Debug`
    /// dump of this struct in that position leaks field syntax into a
    /// sentence a person is meant to read, so reasons render through here
    /// instead.
    fn describe(&self) -> String {
        match self.start_line {
            Some(line) => format!(
                "{} ({}) at {}:{}",
                self.qualified_name, self.node_type, self.file_path, line
            ),
            None => format!(
                "{} ({}) in {}",
                self.qualified_name, self.node_type, self.file_path
            ),
        }
    }
}

impl EdgeKey {
    /// Compact one-line rendering, for the same reason as
    /// [`ExplainNode::describe`].
    fn describe(&self) -> String {
        format!(
            "{} -> \"{}\" ({}, {})",
            self.from_id, self.to_name, self.to_type, self.edge_type
        )
    }
}

impl ExplainEdge {
    /// The four fields that identify this row, matching the resolver's own
    /// write-back key (`resolve::write_updates`'s WHERE clause).
    fn key(&self) -> EdgeKey {
        EdgeKey {
            from_id: self.from_id.clone(),
            to_name: self.to_name.clone(),
            to_type: self.to_type.clone(),
            edge_type: self.edge_type.clone(),
        }
    }

    fn as_query_edge(&self) -> QueryEdge {
        QueryEdge {
            from_id: self.from_id.clone(),
            to_id: self.to_id.clone(),
            to_name: self.to_name.clone(),
            to_type: self.to_type.clone(),
            edge_type: self.edge_type.clone(),
            confidence: self.confidence.clone(),
            candidates: self.candidates.clone(),
        }
    }
}

/// A `deleted_symbol` row — the only non-literal way a name is allowed to
/// enter a query set (see the module docs on S.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeletedSymbol {
    pub name: String,
    pub qualified_name: String,
    pub node_type: String,
    pub file_path: String,
}

/// Identifies one `code_edge` name-edge row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EdgeKey {
    pub from_id: String,
    pub to_name: String,
    pub to_type: String,
    pub edge_type: String,
}

/// One project's graph, loaded once, with everything a chain builder or a
/// chain verifier needs. Holds the resolver's own [`Indices`] so
/// verification can re-run the cascade rather than compare against a
/// recording of it.
pub struct ExplainGraph {
    nodes: Vec<ExplainNode>,
    edges: Vec<ExplainEdge>,
    deleted: Vec<DeletedSymbol>,
    /// The same nodes projected to what the cascade reads, in the same
    /// order, so an index into one is an index into the other.
    resolver_nodes: Vec<ResolverNode>,
    indices: Indices,
    node_by_id: HashMap<String, usize>,
    edge_by_key: HashMap<EdgeKey, Vec<usize>>,
}

impl ExplainGraph {
    /// Build from already-loaded rows. Public so a caller that holds the
    /// data (a test, or a caller that loaded it for another purpose) need
    /// not round-trip the database again.
    pub fn from_parts(
        nodes: Vec<ExplainNode>,
        edges: Vec<ExplainEdge>,
        deleted: Vec<DeletedSymbol>,
    ) -> Self {
        let resolver_nodes: Vec<ResolverNode> = nodes
            .iter()
            .map(|n| ResolverNode {
                id: n.node_id.clone(),
                name: n.name.clone(),
                node_type: n.node_type.clone(),
                language: n.language.clone(),
                file_path: n.file_path.clone(),
                qualified_name: n.qualified_name.clone(),
            })
            .collect();
        let indices = resolve::build_indices(&resolver_nodes);

        let node_by_id = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.node_id.clone(), i))
            .collect();

        let mut edge_by_key: HashMap<EdgeKey, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            edge_by_key.entry(e.key()).or_default().push(i);
        }

        Self {
            nodes,
            edges,
            deleted,
            resolver_nodes,
            indices,
            node_by_id,
            edge_by_key,
        }
    }

    /// Load one project's nodes, name-edges and deletion records.
    pub async fn load(db: &Surreal<Any>, project_id: &str) -> Result<Self> {
        let nodes = load_explain_nodes(db, project_id).await?;
        let edges = load_explain_edges(db, project_id).await?;
        let deleted = load_deleted_symbols(db, project_id).await?;
        Ok(Self::from_parts(nodes, edges, deleted))
    }

    pub fn node(&self, node_id: &str) -> Option<&ExplainNode> {
        self.node_by_id.get(node_id).map(|&i| &self.nodes[i])
    }

    pub fn nodes(&self) -> &[ExplainNode] {
        &self.nodes
    }

    pub fn edges(&self) -> &[ExplainEdge] {
        &self.edges
    }

    /// Exactly one edge for this key, or `None`. A key matching several
    /// rows is a corrupt index, not an ambiguity to pick from, so this
    /// refuses rather than choosing.
    pub fn edge(&self, key: &EdgeKey) -> Option<&ExplainEdge> {
        match self.edge_by_key.get(key) {
            Some(idxs) if idxs.len() == 1 => Some(&self.edges[idxs[0]]),
            _ => None,
        }
    }

    /// Every live (non-`import`) definition of a bare name, as node ids,
    /// sorted. The same set `dependencies::find_stale_references` matches
    /// against and `matching_roots` groups by.
    pub fn live_definitions(&self, name: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| n.name == name && n.node_type != "import")
            .map(|n| n.node_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Re-run the cascade for one edge and return its binding and trace.
    /// This is the verifier's engine: it calls the real resolver, so a
    /// chain cannot claim a rule did something the rule does not do.
    fn rerun(&self, key: &EdgeKey) -> (resolve::Binding, resolve::ResolutionTrace) {
        let edge = UnresolvedEdge {
            from_id: key.from_id.clone(),
            to_name: key.to_name.clone(),
            to_type: key.to_type.clone(),
            edge_type: key.edge_type.clone(),
        };
        resolve::resolve_one_traced(&edge, &self.resolver_nodes, &self.indices)
    }

    fn ids_of(&self, idxs: &[usize]) -> Vec<String> {
        let mut ids: Vec<String> = idxs
            .iter()
            .map(|&i| self.resolver_nodes[i].id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// The candidate pool for one capture, as sorted node ids.
    fn pool_ids(&self, to_name: &str, to_type: &str, from_language: &str) -> Vec<String> {
        let normalized = normalize_separators(to_name);
        let bare = bare_name(&normalized);
        let family = language_family(from_language);
        self.ids_of(resolve::candidate_pool(&self.indices, bare, to_type, family))
    }
}

// ============================================================================
// Caching (RF-6)
// ============================================================================

/// `project_registry.last_indexed_at` for one project, or `None` when the
/// project has never been indexed. Written unconditionally at the end of
/// every `index` run, full or incremental, changed or not (`index::mod`'s
/// `update_project_registry` call), which is what makes it a correct,
/// if conservative, cache key: any index run at all moves it, even a no-op
/// one, so a cache keyed on it never serves a graph older than the last
/// completed index. A handful of point-lookup fields, not the join this
/// module does for `code_node`/`code_edge`/`deleted_symbol`, so it costs a
/// small fraction of a percent of a full [`ExplainGraph::load`] (measured
/// on a 17k-node/47k-edge store: about 0.3ms versus about 350ms; see
/// `specs/receipts/chain-scope-20260914.md`).
///
/// Returned as the raw [`surrealdb_types::Value`] rather than converted to a
/// `String`: the field is a SurrealDB `datetime`, and comparing the raw
/// value is both simpler and exact, whereas a `datetime`-to-`String`
/// conversion through the typed `SurrealValue` derive rejects the value
/// outright (measured: `Failed to convert to none | string: Expected
/// string, got datetime`) rather than formatting it.
async fn current_watermark(db: &Surreal<Any>, project_id: &str) -> Result<Option<surrealdb_types::Value>> {
    let mut resp = db
        .query("SELECT last_indexed_at FROM project_registry WHERE project_id = $pid")
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading the index watermark failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;
    Ok(rows.into_iter().next().and_then(|v| match v {
        surrealdb_types::Value::Object(obj) => obj.get("last_indexed_at").cloned(),
        _ => None,
    }))
}

/// Caches one project's [`ExplainGraph`], reloading only when
/// `project_registry.last_indexed_at` has moved since the cached copy was
/// built.
///
/// `codegraph_verify_chain` is called once per chain, and an audit
/// re-verifies many chains against one index that is not moving between
/// calls (RF-6): a fresh
/// [`ExplainGraph::load`] per call pays the whole project's load cost on
/// every single chain, which is linear in the *project* when the workload's
/// real axis is the number of *chains*. Measured on a 936-file, 17k-node,
/// 47k-edge store built from this repo plus a shallow `tokio-rs/tokio`
/// clone: load was 99.7% of a `load`-then-`verify` call
/// (`specs/receipts/chain-scope-20260914.md`).
///
/// This holds the *complete* graph [`ExplainGraph::load`] would have built,
/// not a graph scoped to any one chain. A per-chain scoped load was the
/// other shape this item named, and it does not hold up under inspection:
/// every re-check that matters (`Fact::LiveDefinitions`,
/// `Fact::CandidatePool`, `Fact::RuleApplication`, `Fact::CascadeOutcome`)
/// re-derives its answer from indices built over the *whole* project's
/// matching nodes (`index::resolve::build_indices`, keyed by
/// `(name, node_type, language_family)` project-wide), not from the ids one
/// chain happens to mention. Scoping the load to "the ids the chain names"
/// would load exactly the rows the chain already claims and nothing else,
/// so recomputing a pool or a live-definition set from that scope would
/// just replay the chain's own claim back at it, silently vacuous in
/// precisely the two tamper classes (`the_verifier_recomputes_the_candidate_pool_rather_than_trusting_it`-style
/// candidate and candidate-pool tampering) this module's own tests exist to
/// catch. A correct scoped load is possible (query by the specific
/// `(bare, to_type, language_family)` and `name` keys a chain's facts
/// name, not by the ids in its `NodeExists`/`CandidatePool` steps), but it
/// is a materially larger and riskier change than caching the exact
/// full-project answer, for a fix this item says should be justified by a
/// measurement rather than by which shape sounded cheaper.
#[derive(Clone)]
pub struct ExplainGraphCache {
    project_id: String,
    entry: Arc<Mutex<Option<(Option<surrealdb_types::Value>, Arc<ExplainGraph>)>>>,
}

impl ExplainGraphCache {
    pub fn new(project_id: String) -> Self {
        Self {
            project_id,
            entry: Arc::new(Mutex::new(None)),
        }
    }

    /// The cached graph if the project's index watermark has not moved
    /// since it was built, otherwise a fresh [`ExplainGraph::load`] that
    /// becomes the new cached copy, unless that load itself straddled an
    /// index run (see the mid-load race note below), in which case it is
    /// returned for this call only and the cache is left untouched.
    ///
    /// **The mid-load race this guards against.** `ExplainGraph::load` runs
    /// three separate queries (nodes, then name-edges, then deletion
    /// records), and `project_registry.last_indexed_at` is written last, as
    /// the final step of an `index` run (`index::mod`'s
    /// `update_project_registry`). If a cold cache or a miss starts loading
    /// while a run is mid-write, the watermark read *before* the load still
    /// reads the *previous* run's value (the current run has not finished),
    /// so the load's three queries can each see a different amount of the
    /// run in progress, some new rows, some not yet, and the result would
    /// get cached under that previous-run watermark. Nothing would then
    /// invalidate it until the *next* run completes, so a torn snapshot
    /// would be served for the rest of the run in progress. The fix is
    /// read-load-read: read the watermark again after the load, and cache
    /// the result only if it did not move. A caller in this narrow window
    /// still gets a graph back (never an error), just not one that gets
    /// remembered; the following call reads a watermark that has settled
    /// and reloads cleanly.
    ///
    /// **Which stores can actually hit this.** An embedded `surrealkv://`
    /// store takes a single-writer file lock; a second process cannot even
    /// open a connection to it (`facade::explain_chains_for_symbol`'s docs
    /// note the same lock causing a second connection to deadlock), so two
    /// *separate processes* sharing one embedded store cannot race here at
    /// all, indexing from a second process would fail to connect long
    /// before it could write anything mid-load. The window is real for a
    /// server-backed store (`ws://`/`wss://`), where a separate client can
    /// index while this process serves `codegraph_verify_chain`, or for a
    /// single process that runs an indexer and this cache against the same
    /// connection (an in-process watch-and-reindex mode, which does not
    /// exist in this codebase today).
    pub async fn get(&self, db: &Surreal<Any>) -> Result<Arc<ExplainGraph>> {
        let mut guard = self.entry.lock().await;
        let watermark_before = current_watermark(db, &self.project_id).await?;
        if let Some((cached_watermark, graph)) = guard.as_ref() {
            if *cached_watermark == watermark_before {
                return Ok(Arc::clone(graph));
            }
        }

        let graph = Arc::new(ExplainGraph::load(db, &self.project_id).await?);
        let watermark_after = current_watermark(db, &self.project_id).await?;
        if should_cache(&watermark_before, &watermark_after) {
            *guard = Some((watermark_after, Arc::clone(&graph)));
        }
        Ok(graph)
    }
}

/// Whether a just-completed load is safe to remember: only when the
/// watermark read before the load matches the one read after it, meaning no
/// index run's `project_registry` write landed while the load's own
/// queries were in flight. A pure function of the two readings so the
/// decision itself is unit-testable without a database or real concurrency
/// (see the `mod tests` below); [`ExplainGraphCache::get`] is the only
/// caller.
fn should_cache(before: &Option<surrealdb_types::Value>, after: &Option<surrealdb_types::Value>) -> bool {
    before == after
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(s: &str) -> Option<surrealdb_types::Value> {
        Some(surrealdb_types::Value::String(s.to_string()))
    }

    #[test]
    fn should_cache_when_the_watermark_held_steady_across_the_load() {
        assert!(should_cache(&mark("t1"), &mark("t1")));
    }

    #[test]
    fn should_cache_when_the_project_has_never_been_indexed_on_either_read() {
        assert!(should_cache(&None, &None));
    }

    #[test]
    fn should_not_cache_when_an_index_run_completed_during_the_load() {
        assert!(!should_cache(&mark("t1"), &mark("t2")));
    }

    #[test]
    fn should_not_cache_when_the_first_index_run_ever_completed_during_the_load() {
        // Before: the project had never been indexed (no project_registry
        // row at all). After: the first run's row now exists. This is the
        // sharpest case, since a naive `Option` comparison that treated
        // "no row" as equal to "some row" would defeat the whole check.
        assert!(!should_cache(&None, &mark("t1")));
    }
}

// ============================================================================
// Chains
// ============================================================================

/// Which finding a chain explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// S — a surviving reference to a symbol the resolver could not bind.
    StaleReference,
    /// A — the resolver saw several candidates and refused to choose.
    AmbiguousRefusal,
    /// D — a resolved reverse-dependency path, hop by hop.
    Dependent,
}

impl FindingKind {
    pub fn as_tag(self) -> &'static str {
        match self {
            FindingKind::StaleReference => "stale_reference",
            FindingKind::AmbiguousRefusal => "ambiguous_refusal",
            FindingKind::Dependent => "dependent",
        }
    }
}

/// How the subject symbol entered the query set. Two variants, both exact,
/// neither a similarity match — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    /// The caller named this symbol literally.
    ExplicitSymbol,
    /// The name came from a `deleted_symbol` record: an earlier index run
    /// watched this definition disappear from this file.
    Deleted(DeletedSymbol),
}

/// One re-checkable claim. Every variant names the concrete graph rows it
/// rests on, and every variant has an arm in [`check_fact`].
/// The discriminator is `kind`, deliberately NOT `fact`. [`Step`] holds its
/// payload in a field named `fact`, so tagging this enum `fact` too made a
/// step serialize as `{"index": 1, "fact": {"fact": "node_exists", ...}}`,
/// with the same word meaning two different things one level apart. That is
/// not merely ugly: a consumer reaching for `step["fact"]["node_id"]` and a
/// consumer reaching for `step["node_id"]` both look right, and serde
/// silently ignores unknown keys on the way back in, so writing to the wrong
/// level is a no-op that raises no error. A tamper test built on the wrong
/// level therefore passes while tampering with nothing, which is the one
/// failure a verifier's own test suite cannot afford. Renaming the tag makes
/// the two levels impossible to confuse: `{"index": 1, "fact": {"kind":
/// "node_exists", ...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    /// S.1 / A.1 — the subject was supplied as a literal symbol argument.
    /// Checked against the chain's own subject: the point of the link is
    /// that the entry point is a literal, so the check is that it is one.
    ExplicitSymbolQuery { name: String },

    /// S.1 — the subject entered via this exact deletion record.
    DeletionRecord(DeletedSymbol),

    /// The complete set of live definitions of a bare name. On an S chain
    /// this is the link that shows the pool is empty of live definitions.
    LiveDefinitions { name: String, node_ids: Vec<String> },

    /// A node exists with exactly these attributes.
    NodeExists {
        node_id: String,
        name: String,
        node_type: String,
        language: String,
        file_path: String,
        qualified_name: String,
        start_line: Option<i64>,
    },

    /// An edge exists with this capture and this stored verdict.
    EdgeExists {
        key: EdgeKey,
        confidence: String,
        resolved_by: String,
        to_id: String,
        candidates: Vec<String>,
    },

    /// The candidate pool the cascade scoped this capture to, recomputed
    /// from the node set rather than read back.
    CandidatePool {
        bare: String,
        to_type: String,
        language_family: String,
        node_ids: Vec<String>,
    },

    /// One cascade rule, re-applied: these nodes survived its filter, with
    /// this outcome. `skipped` means the rule's precondition did not hold,
    /// which is how "the discriminator was absent" is stated as a fact.
    RuleApplication {
        key: EdgeKey,
        rule: String,
        hit_ids: Vec<String>,
        outcome: String,
    },

    /// Why the cascade ended where it did, and every rule it reached.
    CascadeOutcome {
        key: EdgeKey,
        outcome: String,
        attempted_rules: Vec<String>,
    },

    /// One hop of a resolved reverse-dependency path.
    ResolvedHop {
        key: EdgeKey,
        to_id: String,
        resolved_by: String,
        depth: usize,
    },
}

impl Fact {
    /// The variant tag, used in verdicts and rendering.
    pub fn tag(&self) -> &'static str {
        match self {
            Fact::ExplicitSymbolQuery { .. } => "explicit_symbol_query",
            Fact::DeletionRecord(_) => "deletion_record",
            Fact::LiveDefinitions { .. } => "live_definitions",
            Fact::NodeExists { .. } => "node_exists",
            Fact::EdgeExists { .. } => "edge_exists",
            Fact::CandidatePool { .. } => "candidate_pool",
            Fact::RuleApplication { .. } => "rule_application",
            Fact::CascadeOutcome { .. } => "cascade_outcome",
            Fact::ResolvedHop { .. } => "resolved_hop",
        }
    }

    /// Human-readable rendering, derived from the fact at print time. Kept
    /// as a function rather than a stored string on purpose: a stored
    /// sentence could disagree with the fact beside it, and a reader would
    /// have no way to tell which one the verifier checked.
    pub fn describe(&self) -> String {
        match self {
            Fact::ExplicitSymbolQuery { name } => {
                format!("the symbol {name:?} was queried by name, literally")
            }
            Fact::DeletionRecord(d) => format!(
                "an index run recorded {:?} ({}) disappearing from {}",
                d.qualified_name, d.node_type, d.file_path
            ),
            Fact::LiveDefinitions { name, node_ids } if node_ids.is_empty() => {
                format!("no live definition of {name:?} exists in this project")
            }
            Fact::LiveDefinitions { name, node_ids } => format!(
                "{} live definition(s) of {name:?}: {}",
                node_ids.len(),
                node_ids.join(", ")
            ),
            Fact::NodeExists {
                qualified_name,
                node_type,
                file_path,
                start_line,
                ..
            } => match start_line {
                Some(line) => {
                    format!("{qualified_name} ({node_type}) is defined at {file_path}:{line}")
                }
                None => format!("{qualified_name} ({node_type}) is defined in {file_path}"),
            },
            Fact::EdgeExists {
                key, confidence, ..
            } => format!(
                "{} references {:?} ({}), and the resolver left it {confidence}",
                key.from_id, key.to_name, key.to_type
            ),
            Fact::CandidatePool {
                bare,
                to_type,
                language_family,
                node_ids,
            } => format!(
                "the candidate pool for ({bare:?}, {to_type}, {language_family}) holds {} node(s)",
                node_ids.len()
            ),
            Fact::RuleApplication {
                rule,
                hit_ids,
                outcome,
                ..
            } => match outcome.as_str() {
                "skipped" => format!("{rule} did not run: its precondition did not hold"),
                "no_match" => format!("{rule} ran and admitted nothing"),
                "not_unique" => {
                    format!("{rule} ran and admitted {} nodes, so it could not bind", hit_ids.len())
                }
                _ => format!("{rule} admitted exactly one node, {}", hit_ids.join("")),
            },
            Fact::CascadeOutcome {
                outcome,
                attempted_rules,
                ..
            } => format!(
                "the cascade tried {} and ended: {outcome}",
                attempted_rules.join(", ")
            ),
            Fact::ResolvedHop {
                key,
                to_id,
                resolved_by,
                depth,
            } => format!(
                "at depth {depth}, {} reaches {to_id} through {:?}, bound by {resolved_by}",
                key.from_id, key.to_name
            ),
        }
    }
}

/// One link in a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Step {
    /// Position in the chain, from 1. Carried explicitly so a verdict can
    /// name a failing step unambiguously even out of context.
    pub index: usize,
    pub fact: Fact,
}

/// The derivation behind one finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Chain {
    pub explain_version: u32,
    pub finding: FindingKind,
    /// The symbol this chain is about.
    pub subject: String,
    pub steps: Vec<Step>,
    /// Exact commands that re-derive this chain against the same index, for
    /// a reader who would rather run it than read it. Not a step: it is an
    /// instruction to a human, carries no claim, and is never verified.
    pub replay: Vec<String>,
}

impl Chain {
    fn new(finding: FindingKind, subject: &str, facts: Vec<Fact>, replay: Vec<String>) -> Self {
        Self {
            explain_version: EXPLAIN_VERSION,
            finding,
            subject: subject.to_string(),
            steps: facts
                .into_iter()
                .enumerate()
                .map(|(i, fact)| Step { index: i + 1, fact })
                .collect(),
            replay,
        }
    }

    /// Indented human rendering, derived entirely from the facts.
    pub fn render(&self) -> String {
        let mut out = format!("{}: {}\n", self.finding.as_tag(), self.subject);
        for step in &self.steps {
            out.push_str(&format!("  {}. {}\n", step.index, step.fact.describe()));
        }
        if !self.replay.is_empty() {
            out.push_str("  replay:\n");
            for cmd in &self.replay {
                out.push_str(&format!("    $ {cmd}\n"));
            }
        }
        out
    }
}

// ============================================================================
// Verification
// ============================================================================

/// One step's re-check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StepVerdict {
    pub index: usize,
    pub fact: String,
    pub ok: bool,
    /// Present iff `ok` is false: what the graph says instead.
    pub reason: Option<String>,
}

/// The result of re-checking a whole chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChainVerdict {
    pub ok: bool,
    /// Set, with the chain's own version, when the chain could not be
    /// checked at all because it predates this build's format. Absent
    /// otherwise.
    ///
    /// This is deliberately not just another failing step. "Your evidence
    /// does not hold up" and "I am too new to read your evidence" are
    /// different messages to send a client, and collapsing them into one
    /// boolean would let a version skew read as a forgery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_version: Option<u32>,
    pub steps: Vec<StepVerdict>,
}

impl ChainVerdict {
    /// The steps that failed, for a caller that only wants the damage.
    pub fn failures(&self) -> Vec<&StepVerdict> {
        self.steps.iter().filter(|s| !s.ok).collect()
    }
}

/// Re-check every step of a chain against the live graph.
///
/// Every step is checked, including steps after the first failure: a
/// tampered chain should reveal everything wrong with it in one pass, not
/// one defect per run.
pub fn verify_chain(chain: &Chain, graph: &ExplainGraph) -> ChainVerdict {
    // Version first. A chain from another format may still deserialize far
    // enough to reach here (a future v3 that only adds a variant, say), and
    // checking its steps against v2 semantics would produce confident
    // nonsense in either direction.
    if chain.explain_version != EXPLAIN_VERSION {
        return unsupported_version_verdict(chain.explain_version);
    }

    let steps: Vec<StepVerdict> = chain
        .steps
        .iter()
        .map(|step| {
            let reason = check_fact(&step.fact, chain, graph);
            StepVerdict {
                index: step.index,
                fact: step.fact.tag().to_string(),
                ok: reason.is_none(),
                reason,
            }
        })
        .collect();
    ChainVerdict {
        ok: steps.iter().all(|s| s.ok),
        unsupported_version: None,
        steps,
    }
}

fn unsupported_version_verdict(found: u32) -> ChainVerdict {
    ChainVerdict {
        ok: false,
        unsupported_version: Some(found),
        steps: Vec::new(),
    }
}

/// Just enough of a chain to learn its version, for a document this build
/// may not be able to parse in full.
#[derive(Deserialize)]
struct ChainVersionProbe {
    explain_version: u32,
}

/// Read a chain from JSON and verify it, reporting a version mismatch as a
/// verdict instead of as a parse failure.
///
/// [`verify_chain`] cannot do this on its own: a v1 chain tags its facts
/// `fact` rather than `kind`, so it does not deserialize into a v2 [`Chain`]
/// at all and `verify_chain` never sees it. A caller that went straight to
/// `serde_json::from_str` would surface "missing field `kind`" to somebody
/// re-checking an audit report, which reads as "your evidence is malformed"
/// when the truth is "this tool moved on". Read the version first, decide,
/// and only then parse.
pub fn verify_chain_json(json: &str, graph: &ExplainGraph) -> ChainVerdict {
    if let Ok(probe) = serde_json::from_str::<ChainVersionProbe>(json) {
        if probe.explain_version != EXPLAIN_VERSION {
            return unsupported_version_verdict(probe.explain_version);
        }
    }
    match serde_json::from_str::<Chain>(json) {
        Ok(chain) => verify_chain(&chain, graph),
        Err(e) => ChainVerdict {
            ok: false,
            unsupported_version: None,
            // Index 0 is unused by real steps, which start at 1, so a
            // consumer can tell a document-level failure from a step
            // failure without string matching.
            steps: vec![StepVerdict {
                index: 0,
                fact: "chain_document".to_string(),
                ok: false,
                reason: Some(format!("chain could not be read: {e}")),
            }],
        },
    }
}

/// `None` when the fact holds; `Some(reason)` naming what the graph says
/// instead when it does not.
fn check_fact(fact: &Fact, chain: &Chain, graph: &ExplainGraph) -> Option<String> {
    match fact {
        Fact::ExplicitSymbolQuery { name } => {
            if name == &chain.subject {
                None
            } else {
                Some(format!(
                    "entry names {name:?} but the chain's subject is {:?}",
                    chain.subject
                ))
            }
        }

        Fact::DeletionRecord(d) => {
            if graph.deleted.contains(d) {
                None
            } else {
                Some(format!(
                    "no deleted_symbol row matches {:?} in {}",
                    d.qualified_name, d.file_path
                ))
            }
        }

        Fact::LiveDefinitions { name, node_ids } => {
            let actual = graph.live_definitions(name);
            if &actual == node_ids {
                None
            } else {
                Some(format!(
                    "live definitions of {name:?} are {actual:?}, chain claims {node_ids:?}"
                ))
            }
        }

        Fact::NodeExists {
            node_id,
            name,
            node_type,
            language,
            file_path,
            qualified_name,
            start_line,
        } => {
            let Some(n) = graph.node(node_id) else {
                return Some(format!("no node with id {node_id:?}"));
            };
            let claimed = ExplainNode {
                node_id: node_id.clone(),
                name: name.clone(),
                node_type: node_type.clone(),
                language: language.clone(),
                file_path: file_path.clone(),
                qualified_name: qualified_name.clone(),
                start_line: *start_line,
            };
            if n == &claimed {
                None
            } else {
                Some(format!(
                    "node {node_id} is {}, chain claims {}",
                    n.describe(),
                    claimed.describe()
                ))
            }
        }

        Fact::EdgeExists {
            key,
            confidence,
            resolved_by,
            to_id,
            candidates,
        } => {
            let Some(e) = graph.edge(key) else {
                return Some(format!("no unique edge for {}", key.describe()));
            };
            let mut actual_candidates = e.candidates.clone();
            actual_candidates.sort();
            if &e.confidence != confidence {
                return Some(format!(
                    "edge confidence is {:?}, chain claims {confidence:?}",
                    e.confidence
                ));
            }
            if &e.resolved_by != resolved_by {
                return Some(format!(
                    "edge resolved_by is {:?}, chain claims {resolved_by:?}",
                    e.resolved_by
                ));
            }
            if &e.to_id != to_id {
                return Some(format!("edge to_id is {:?}, chain claims {to_id:?}", e.to_id));
            }
            if &actual_candidates != candidates {
                return Some(format!(
                    "edge candidates are {actual_candidates:?}, chain claims {candidates:?}"
                ));
            }
            None
        }

        Fact::CandidatePool {
            bare,
            to_type,
            language_family,
            node_ids,
        } => {
            let actual =
                graph.ids_of(resolve::candidate_pool(&graph.indices, bare, to_type, language_family));
            if &actual == node_ids {
                None
            } else {
                Some(format!(
                    "pool for ({bare:?}, {to_type}, {language_family}) is {actual:?}, \
                     chain claims {node_ids:?}"
                ))
            }
        }

        Fact::RuleApplication {
            key,
            rule,
            hit_ids,
            outcome,
        } => {
            let (_, trace) = graph.rerun(key);
            let Some(attempt) = trace.attempts.iter().find(|a| a.rule == rule.as_str()) else {
                return Some(format!(
                    "the cascade never reached {rule} for {}; it tried {}",
                    key.describe(),
                    trace.attempted_rules().join(", ")
                ));
            };
            let actual_hits = graph.ids_of(&attempt.hits);
            if attempt.outcome.as_tag() != outcome {
                return Some(format!(
                    "{rule} outcome is {:?}, chain claims {outcome:?}",
                    attempt.outcome.as_tag()
                ));
            }
            if &actual_hits != hit_ids {
                return Some(format!(
                    "{rule} admits {actual_hits:?}, chain claims {hit_ids:?}"
                ));
            }
            None
        }

        Fact::CascadeOutcome {
            key,
            outcome,
            attempted_rules,
        } => {
            let (_, trace) = graph.rerun(key);
            if trace.outcome.as_tag() != outcome {
                return Some(format!(
                    "cascade outcome is {:?}, chain claims {outcome:?}",
                    trace.outcome.as_tag()
                ));
            }
            let actual = trace.attempted_rules();
            if &actual != attempted_rules {
                return Some(format!(
                    "cascade tried {actual:?}, chain claims {attempted_rules:?}"
                ));
            }
            None
        }

        Fact::ResolvedHop {
            key,
            to_id,
            resolved_by,
            depth: _,
        } => {
            let Some(e) = graph.edge(key) else {
                return Some(format!("no unique edge for hop {}", key.describe()));
            };
            if e.confidence != "RESOLVED" {
                return Some(format!(
                    "hop edge is {:?}, not RESOLVED, so it cannot carry a walk",
                    e.confidence
                ));
            }
            if &e.to_id != to_id {
                return Some(format!("hop lands on {:?}, chain claims {to_id:?}", e.to_id));
            }
            if &e.resolved_by != resolved_by {
                return Some(format!(
                    "hop was bound by {:?}, chain claims {resolved_by:?}",
                    e.resolved_by
                ));
            }
            if graph.node(to_id).is_none() {
                return Some(format!("hop target {to_id:?} is not a node"));
            }
            None
        }
    }
}

// ============================================================================
// Chain construction
// ============================================================================

fn node_fact(n: &ExplainNode) -> Fact {
    Fact::NodeExists {
        node_id: n.node_id.clone(),
        name: n.name.clone(),
        node_type: n.node_type.clone(),
        language: n.language.clone(),
        file_path: n.file_path.clone(),
        qualified_name: n.qualified_name.clone(),
        start_line: n.start_line,
    }
}

fn edge_fact(e: &ExplainEdge) -> Fact {
    let mut candidates = e.candidates.clone();
    candidates.sort();
    Fact::EdgeExists {
        key: e.key(),
        confidence: e.confidence.clone(),
        resolved_by: e.resolved_by.clone(),
        to_id: e.to_id.clone(),
        candidates,
    }
}

fn membership_fact(subject: &str, membership: &Membership) -> Fact {
    match membership {
        Membership::ExplicitSymbol => Fact::ExplicitSymbolQuery {
            name: subject.to_string(),
        },
        Membership::Deleted(d) => Fact::DeletionRecord(d.clone()),
    }
}

/// Every rule the cascade reached for one edge, as facts, followed by the
/// terminal outcome. Recomputed, so these facts describe the cascade as it
/// runs now rather than as some row remembers it.
fn cascade_facts(graph: &ExplainGraph, key: &EdgeKey) -> Vec<Fact> {
    let (_, trace) = graph.rerun(key);
    let mut facts: Vec<Fact> = trace
        .attempts
        .iter()
        .map(|a| Fact::RuleApplication {
            key: key.clone(),
            rule: a.rule.to_string(),
            hit_ids: graph.ids_of(&a.hits),
            outcome: a.outcome.as_tag().to_string(),
        })
        .collect();
    facts.push(Fact::CascadeOutcome {
        key: key.clone(),
        outcome: trace.outcome.as_tag().to_string(),
        attempted_rules: trace.attempted_rules(),
    });
    facts
}

fn replay_for(project_id: &str, symbol: &str) -> Vec<String> {
    vec![
        format!("codegraph query --kind rdeps --name {symbol} --project-id {project_id} --explain --json"),
        format!("codegraph query --kind rdeps --name {symbol} --project-id {project_id} --json"),
    ]
}

/// **S — stale reference.** A surviving reference whose target the resolver
/// could not bind, explained: how the name entered the query, the capture
/// itself, the pool the cascade scoped, every rule it tried, and why the
/// last one produced nothing.
///
/// Returns `None` when the finding cannot be located as a unique edge in
/// this graph. That is deliberate: a chain that cannot be built is reported
/// as absent, never as an unverifiable placeholder.
pub fn explain_stale_reference(
    graph: &ExplainGraph,
    project_id: &str,
    subject: &str,
    membership: &Membership,
    key: &EdgeKey,
) -> Option<Chain> {
    let edge = graph.edge(key)?;
    let from = graph.node(&edge.from_id)?;

    let mut facts = vec![
        // S.1 — query membership, exact by construction.
        membership_fact(subject, membership),
        // The live-definition set the capture was matched against. On the
        // renamed-away path this is empty, and that emptiness is the
        // finding: nothing answers to the name any more.
        Fact::LiveDefinitions {
            name: subject.to_string(),
            node_ids: graph.live_definitions(subject),
        },
        // S.2 — the capture, and the node holding it (file and line).
        node_fact(from),
        edge_fact(edge),
        // S.3 — the pool, then the rules.
        Fact::CandidatePool {
            bare: bare_name(&normalize_separators(&edge.to_name)).to_string(),
            to_type: edge.to_type.clone(),
            language_family: language_family(&from.language).to_string(),
            node_ids: graph.pool_ids(&edge.to_name, &edge.to_type, &from.language),
        },
    ];
    facts.extend(cascade_facts(graph, key));

    Some(Chain::new(
        FindingKind::StaleReference,
        subject,
        facts,
        replay_for(project_id, subject),
    ))
}

/// **A — ambiguous refusal.** The resolver saw more than one admissible
/// candidate and declined to choose. The chain shows the capture, the full
/// candidate set as real nodes, every rule that ran, and which
/// discriminator was absent: an `r4` step with outcome `skipped` is the
/// import-informed tier having nothing to work with, which is the common
/// cause outside Rust.
pub fn explain_ambiguous_refusal(
    graph: &ExplainGraph,
    project_id: &str,
    subject: &str,
    key: &EdgeKey,
) -> Option<Chain> {
    let edge = graph.edge(key)?;
    let from = graph.node(&edge.from_id)?;

    let mut facts = vec![
        Fact::ExplicitSymbolQuery {
            name: subject.to_string(),
        },
        node_fact(from),
        edge_fact(edge),
        Fact::CandidatePool {
            bare: bare_name(&normalize_separators(&edge.to_name)).to_string(),
            to_type: edge.to_type.clone(),
            language_family: language_family(&from.language).to_string(),
            node_ids: graph.pool_ids(&edge.to_name, &edge.to_type, &from.language),
        },
    ];
    // Each surviving candidate is a real node, named. Without this the
    // candidate list is just strings.
    for cand in &edge.candidates {
        facts.push(node_fact(graph.node(cand)?));
    }
    facts.extend(cascade_facts(graph, key));

    Some(Chain::new(
        FindingKind::AmbiguousRefusal,
        subject,
        facts,
        replay_for(project_id, subject),
    ))
}

/// **D — dependent.** The resolved path from a root symbol out to one
/// reverse-dependent, hop by hop, each hop carrying the cascade rule that
/// bound it.
pub fn explain_dependent(
    graph: &ExplainGraph,
    project_id: &str,
    subject: &str,
    path: &ReversePath,
) -> Option<Chain> {
    let root = graph.node(&path.root_node_id)?;
    let mut facts = vec![
        Fact::ExplicitSymbolQuery {
            name: subject.to_string(),
        },
        node_fact(root),
    ];

    // Hops run root-outward; each edge points *back* toward the root, so
    // the node this hop reveals is the edge's source.
    for (i, &_ei) in path.hops.iter().enumerate() {
        let key = path_hop_key(graph, path, i)?;
        let edge = graph.edge(&key)?;
        facts.push(Fact::ResolvedHop {
            key: key.clone(),
            to_id: edge.to_id.clone(),
            resolved_by: edge.resolved_by.clone(),
            depth: i + 1,
        });
        facts.push(node_fact(graph.node(&edge.from_id)?));
    }

    Some(Chain::new(
        FindingKind::Dependent,
        subject,
        facts,
        replay_for(project_id, subject),
    ))
}

/// The hop's edge key. `ReversePath::hops` indexes the edge slice the
/// traversal ran over; this module rebuilds that slice from its own edges
/// in the same order, so the index is shared.
fn path_hop_key(graph: &ExplainGraph, path: &ReversePath, hop: usize) -> Option<EdgeKey> {
    let ei = *path.hops.get(hop)?;
    graph.edges.get(ei).map(ExplainEdge::key)
}

/// Every chain for one symbol: the S chains for its stale references, the A
/// chains for the ambiguous refusals touching it, and the D chains for its
/// resolved reverse-dependents.
///
/// Findings are derived here from the same rules the query layer applies,
/// so a chain exists for a finding exactly when the query reports it.
pub fn explain_symbol(
    graph: &ExplainGraph,
    project_id: &str,
    subject: &str,
    membership: &Membership,
) -> Vec<Chain> {
    let mut chains = Vec::new();

    // S: project-wide UNRESOLVED captures that still name this symbol,
    // under the query layer's own matching rule.
    let live = graph.live_definitions(subject);
    let queried_bare = bare_name(&normalize_separators(subject)).to_string();
    let live_nodes: Vec<&ResolverNode> = live
        .iter()
        .filter_map(|id| {
            graph
                .node_by_id
                .get(id.as_str())
                .map(|&i| &graph.resolver_nodes[i])
        })
        .collect();

    let mut stale_keys: Vec<EdgeKey> = graph
        .edges
        .iter()
        .filter(|e| e.confidence == "UNRESOLVED")
        .filter(|e| {
            let Some(from) = graph.node(&e.from_id) else {
                return false;
            };
            dependencies::is_stale_candidate_for_explain(
                &e.to_name,
                &e.to_type,
                &from.language,
                &queried_bare,
                &live_nodes,
            )
        })
        .map(ExplainEdge::key)
        .collect();
    stale_keys.sort_by(|a, b| {
        (&a.from_id, &a.to_name, &a.to_type, &a.edge_type)
            .cmp(&(&b.from_id, &b.to_name, &b.to_type, &b.edge_type))
    });
    for key in &stale_keys {
        if let Some(c) = explain_stale_reference(graph, project_id, subject, membership, key) {
            chains.push(c);
        }
    }

    // A: AMBIGUOUS edges whose candidate set includes a live definition of
    // this symbol — the same incidence the reverse walk surfaces.
    let live_set: HashSet<&str> = live.iter().map(String::as_str).collect();
    let mut ambiguous_keys: Vec<EdgeKey> = graph
        .edges
        .iter()
        .filter(|e| e.confidence == "AMBIGUOUS")
        .filter(|e| e.candidates.iter().any(|c| live_set.contains(c.as_str())))
        .map(ExplainEdge::key)
        .collect();
    ambiguous_keys.sort_by(|a, b| {
        (&a.from_id, &a.to_name, &a.to_type, &a.edge_type)
            .cmp(&(&b.from_id, &b.to_name, &b.to_type, &b.edge_type))
    });
    for key in &ambiguous_keys {
        if let Some(c) = explain_ambiguous_refusal(graph, project_id, subject, key) {
            chains.push(c);
        }
    }

    // D: resolved reverse-dependency paths.
    let query_edges: Vec<QueryEdge> = graph.edges.iter().map(ExplainEdge::as_query_edge).collect();
    let paths = dependencies::reverse_dependency_paths(
        &graph.resolver_nodes,
        &query_edges,
        subject,
        EXPLAIN_DEPTH,
    );
    for path in &paths {
        if let Some(c) = explain_dependent(graph, project_id, subject, path) {
            chains.push(c);
        }
    }

    chains
}

// ============================================================================
// Row loading
// ============================================================================

async fn load_explain_nodes(db: &Surreal<Any>, project_id: &str) -> Result<Vec<ExplainNode>> {
    let mut resp = db
        .query(
            "SELECT node_id, name, node_type, language, file_path, qualified_name, start_line \
             FROM code_node WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading nodes for explain failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let node_id = super::get_str(obj, "node_id");
            if node_id.is_empty() {
                return None;
            }
            Some(ExplainNode {
                node_id,
                name: super::get_str(obj, "name"),
                node_type: super::get_str(obj, "node_type"),
                language: super::get_str(obj, "language"),
                file_path: super::get_str(obj, "file_path"),
                qualified_name: super::get_str(obj, "qualified_name"),
                start_line: get_i64(obj, "start_line"),
            })
        })
        .collect())
}

async fn load_explain_edges(db: &Surreal<Any>, project_id: &str) -> Result<Vec<ExplainEdge>> {
    let etypes: Vec<String> = super::NAME_EDGE_TYPES.iter().map(|s| s.to_string()).collect();
    let mut resp = db
        .query(
            "SELECT from_id, to_id, to_name, to_type, edge_type, confidence, resolved_by, \
             candidates, attempted_rules, resolution_outcome FROM code_edge \
             WHERE project_id = $pid AND edge_type IN $etypes",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("etypes", etypes))
        .await
        .context("loading edges for explain failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let from_id = super::get_str(obj, "from_id");
            if from_id.is_empty() {
                return None;
            }
            Some(ExplainEdge {
                from_id,
                to_id: super::get_str(obj, "to_id"),
                to_name: super::get_str(obj, "to_name"),
                to_type: super::get_str(obj, "to_type"),
                edge_type: super::get_str(obj, "edge_type"),
                confidence: super::get_str(obj, "confidence"),
                resolved_by: super::get_str(obj, "resolved_by"),
                candidates: super::get_str_array(obj, "candidates"),
                attempted_rules: super::get_str_array(obj, "attempted_rules"),
                resolution_outcome: super::get_str(obj, "resolution_outcome"),
            })
        })
        .collect())
}

async fn load_deleted_symbols(db: &Surreal<Any>, project_id: &str) -> Result<Vec<DeletedSymbol>> {
    let mut resp = db
        .query(
            "SELECT name, qualified_name, node_type, file_path FROM deleted_symbol \
             WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading deletion records for explain failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            Some(DeletedSymbol {
                name: super::get_str(obj, "name"),
                qualified_name: super::get_str(obj, "qualified_name"),
                node_type: super::get_str(obj, "node_type"),
                file_path: super::get_str(obj, "file_path"),
            })
        })
        .collect())
}

fn get_i64(obj: &surrealdb_types::Object, key: &str) -> Option<i64> {
    obj.get(key).and_then(|v| match v {
        surrealdb_types::Value::Number(n) => n.to_int(),
        _ => None,
    })
}
