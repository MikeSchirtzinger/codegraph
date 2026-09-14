//! Gate-facing facade — the only surface an external caller (brevity's
//! structural gate) links against. Hides `Surreal`, `project_id` plumbing,
//! and the `DependencyResult` → `StructuralVerdict` projection behind a
//! small, typed, `Store`-scoped API. See `specs/o-spine-step2-design.md`
//! §2/§3 — this module's types and signatures are the frozen contract two
//! other agents build against concurrently; do not drift from it without
//! updating the design doc first.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::graph::dependencies::{self, DependencyResult};
use crate::graph::explain::{self, Chain, DeletedSymbol, ExplainGraph, Membership};
use crate::graph::NAME_EDGE_TYPES;

/// Traversal depth every facade query uses. Not exposed as a parameter (§3.1
/// design note): the facade's whole point is one canonical shape the gate,
/// the CLI's `--json`, and the demo all agree on — not a caller-tunable
/// knob. Matches the CLI's own default (`cli.rs`'s `depth` arg default).
const DEFAULT_DEPTH: usize = 3;

// ============================================================================
// StructuralVerdict — the shared verdict type (design doc §2)
// ============================================================================

/// The structural impact of touching one symbol or a set of files, projected
/// from codegraph's resolved reverse-dependency graph. Aggregated across
/// every matched root symbol (a file may define many; a bare name may
/// collide, D3). Plain serde struct — no SurrealDB types in its fields, so
/// it crosses the brevity path-dependency boundary cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StructuralVerdict {
    /// What was queried. `Symbol("foo")` or `Paths(["src/a.rs", ...])`.
    pub target: VerdictTarget,

    /// RESOLVED reverse-dependents — the blast radius. INFORMATIONAL: a
    /// non-empty blast radius is not a rejection, it is context.
    pub resolved_dependents: Vec<Dependent>,

    /// AMBIGUOUS edges touching a matched symbol — the resolver's explicit
    /// refusal to guess. A rejection reason for the gate.
    pub ambiguous_refusals: Vec<AmbiguousRefusal>,

    /// Project-wide UNRESOLVED edges whose target bare-tails to a matched
    /// symbol — incomplete-rename callers. THE kill-test signal, a
    /// rejection reason for the gate.
    pub stale_references: Vec<StaleReference>,

    /// True when a queried bare name matched >1 distinct live symbol.
    pub name_ambiguous: bool,

    /// Distinct root symbols this verdict aggregates (0 when a queried name
    /// / file set matched nothing live).
    pub matched_symbols: Vec<MatchedSymbol>,

    /// True IFF the project has zero indexed nodes — the ONLY graph-state
    /// that maps to a gate skip. Distinct from "matched nothing": a
    /// populated graph where the touched files define no symbols is a real,
    /// empty-but-valid verdict, not a skip.
    pub graph_empty: bool,

    /// Machine-checkable evidence chains for the findings above, present
    /// only when explain was asked for (`specs/explain-v1.md`). `None` is
    /// "not requested"; `Some([])` is "requested, and there was nothing to
    /// explain" — a distinction a consumer needs, which is why this is an
    /// `Option` rather than a bare `Vec`.
    ///
    /// Skipped entirely when absent, so a verdict produced with explain off
    /// serializes to exactly the bytes it did before this field existed.
    /// `tests/explain.rs::json_shape_is_unchanged_when_explain_is_off`
    /// pins that against the pre-change output, byte for byte.
    ///
    /// Explanations never change a verdict. The decision is the same pure
    /// function of the same findings it was before; this field only shows
    /// the derivation behind findings that were already there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanations: Option<Vec<Chain>>,
}

impl StructuralVerdict {
    pub fn is_clean(&self) -> bool {
        self.ambiguous_refusals.is_empty() && self.stale_references.is_empty()
    }
    pub fn resolved_dependent_count(&self) -> usize {
        self.resolved_dependents.len()
    }
    pub fn ambiguous_count(&self) -> usize {
        self.ambiguous_refusals.len()
    }
    pub fn stale_count(&self) -> usize {
        self.stale_references.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VerdictTarget {
    Symbol(String),
    Paths(Vec<String>),
}

/// `VerdictTarget` has no single "natural" default; `Paths([])` (an empty
/// file set) is the closest to a neutral value, and is what
/// `impact_verdict_for_paths(&[])` actually targets.
impl Default for VerdictTarget {
    fn default() -> Self {
        VerdictTarget::Paths(Vec::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedSymbol {
    pub node_id: String,
    pub qualified_name: String,
    pub name: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependent {
    pub node_id: String,
    pub name: String,
    pub qualified_name: String,
    pub node_type: String,
    pub file_path: String,
    pub depth: usize,
    /// qualified_name of the matched root this dependent hangs off.
    pub of_symbol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousRefusal {
    pub from_name: String,
    pub from_file: String,
    pub to_name: String,
    pub to_type: String,
    pub candidates: Vec<String>,
    pub of_symbol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleReference {
    pub from_name: String,
    pub from_file: String,
    pub to_name: String,
    pub to_type: String,
}

// ============================================================================
// Store + facade functions (design doc §3)
// ============================================================================

/// A long-lived, cheap-to-clone handle: one connection + a project scope.
/// Mirrors `CodegraphServer { db, project_id }` (`src/mcp/server.rs:23`).
#[derive(Clone)]
pub struct Store {
    db: Arc<Surreal<Any>>,
    project_id: String,
}

#[derive(Debug)]
pub enum FacadeError {
    /// Store unreachable / connection lost / auth. The gate maps this to a
    /// skip (loud), NOT to a pass and NOT to a hard abort.
    Connection(String),
    /// A query executed but failed (schema drift, malformed data). Surfaced
    /// as a hard error so the gate can log it distinctly; the gate still
    /// degrades to a skip rather than passing.
    Query(String),
}

impl std::fmt::Display for FacadeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FacadeError::Connection(msg) => write!(f, "codegraph store unavailable: {msg}"),
            FacadeError::Query(msg) => write!(f, "codegraph query failed: {msg}"),
        }
    }
}

impl std::error::Error for FacadeError {}

/// Open (or attach to) a codegraph store. `url` takes the same forms as
/// `db::connect` (`src/db.rs:21`): `surrealkv://path`, `ws://…`, or an empty
/// string (equivalent to `None`) for the embedded default / `SURREALDB_URL`
/// env fallback. Held for the caller's whole lifetime — expected to be
/// called once at orchestrator construction (`Store` is `Clone`, cheap,
/// threads freely).
///
/// Also runs the (idempotent) schema DDL, exactly as `codegraph index` and
/// `codegraph resolve` already do on connect — this makes the facade
/// self-sufficient against a store that was created but never indexed: a
/// truly empty store still answers `is_indexed` cleanly instead of erroring
/// on an undefined table.
pub async fn open_store(url: &str, project_id: &str) -> Result<Store, FacadeError> {
    let url_opt = if url.is_empty() { None } else { Some(url) };
    let db = crate::db::connect(url_opt)
        .await
        .map_err(|e| FacadeError::Connection(e.to_string()))?;
    crate::db::init_schema(&db)
        .await
        .map_err(|e| FacadeError::Connection(e.to_string()))?;
    Ok(Store {
        db,
        project_id: project_id.to_string(),
    })
}

/// True IFF the project has ≥1 indexed `code_node`. The gate calls this to
/// distinguish an unavailable/empty graph from a real verdict.
pub async fn is_indexed(store: &Store) -> Result<bool, FacadeError> {
    is_indexed_conn(&store.db, &store.project_id).await
}

/// Same check as [`is_indexed`], for a caller that already holds its own
/// `Surreal` connection and doesn't want (or, for an embedded engine, can't
/// safely open — see `tests/facade_integration.rs`'s module docs on
/// surrealkv's single-writer lock) a second one just to open a `Store`.
/// The CLI's `--json` path (`main.rs`) uses this directly.
pub async fn is_indexed_conn(db: &Surreal<Any>, project_id: &str) -> Result<bool, FacadeError> {
    let mut resp = db
        .query("SELECT node_id FROM code_node WHERE project_id = $pid LIMIT 1")
        .bind(("pid", project_id.to_string()))
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;
    let rows: Vec<surrealdb_types::Value> =
        resp.take(0).map_err(|e| FacadeError::Query(e.to_string()))?;
    Ok(!rows.is_empty())
}

/// Impact of touching one symbol by (bare) name. Wraps
/// `get_reverse_dependencies` over `NAME_EDGE_TYPES` with
/// `include_ambiguous=true` (the gate always wants refusals surfaced),
/// projects to `StructuralVerdict`.
pub async fn impact_verdict(store: &Store, symbol: &str) -> Result<StructuralVerdict, FacadeError> {
    let graph_empty = !is_indexed(store).await?;

    let dep_result = dependencies::get_reverse_dependencies(
        &store.db,
        &store.project_id,
        symbol,
        &NAME_EDGE_TYPES,
        DEFAULT_DEPTH,
        true,
    )
    .await
    .map_err(|e| FacadeError::Query(e.to_string()))?;

    Ok(project_dependency_result(
        VerdictTarget::Symbol(symbol.to_string()),
        symbol,
        &dep_result,
        graph_empty,
    ))
}

/// Gate-facing entry: impact of touching a set of files. Resolves the files
/// to the symbols they define, runs the same reverse-dep + stale scan as
/// `impact_verdict` per symbol, and unions the results into one
/// `StructuralVerdict` (target = `Paths`). THIS is what brevity calls,
/// because the plan carries files, not symbol names.
pub async fn impact_verdict_for_paths(
    store: &Store,
    paths: &[String],
) -> Result<StructuralVerdict, FacadeError> {
    let graph_empty = !is_indexed(store).await?;

    if paths.is_empty() {
        // No target ≠ no graph — a real, clean, empty verdict.
        return Ok(StructuralVerdict {
            target: VerdictTarget::Paths(Vec::new()),
            graph_empty,
            ..Default::default()
        });
    }

    let touched = touched_symbol_names(&store.db, &store.project_id, paths).await?;

    // Deletion-tracking union (design doc §10 option 1, closing killtest-6's
    // incomplete-rename blind spot): names recorded as deleted *from these
    // files* by an incremental re-index are no longer defined in them — the
    // live-symbol query above can't produce them by definition — yet they are
    // exactly the names whose stale callers the gate exists to catch. Union
    // them into the query set; `find_stale_references` (which runs on every
    // reverse query, live roots or not) does the rest. Purely additive
    // recall: a deleted name with zero remaining UNRESOLVED references
    // contributes nothing, so a since-fixed rename stays clean.
    let deleted = deleted_symbol_names(&store.db, &store.project_id, paths, &touched).await?;

    if touched.is_empty() && deleted.is_empty() {
        // Graph populated (or not) but zero files matched a live symbol —
        // still a valid, clean verdict, not a skip (design doc §3.3 step 4).
        return Ok(StructuralVerdict {
            target: VerdictTarget::Paths(paths.to_vec()),
            graph_empty,
            ..Default::default()
        });
    }

    let mut matched_symbols = Vec::new();
    let mut resolved_dependents: Vec<Dependent> = Vec::new();
    let mut ambiguous_refusals: Vec<AmbiguousRefusal> = Vec::new();
    let mut stale_references: Vec<StaleReference> = Vec::new();
    let mut name_ambiguous = false;

    let mut seen_dependents = std::collections::HashSet::new();
    let mut seen_ambiguous = std::collections::HashSet::new();
    let mut seen_stale = std::collections::HashSet::new();

    // Deleted names ride the exact same per-name machinery as live ones. A
    // deleted name usually matches no live root (groups empty → no
    // matched_symbols entry) and only contributes stale_references; when its
    // bare name IS still defined elsewhere (move, or a legal crate-root
    // collision), the live survivor's group shows up as informational
    // context — truthful, and never a rejection by itself.
    for name in touched.iter().chain(deleted.iter()) {
        let dep_result = dependencies::get_reverse_dependencies(
            &store.db,
            &store.project_id,
            name,
            &NAME_EDGE_TYPES,
            DEFAULT_DEPTH,
            true,
        )
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;

        name_ambiguous |= dep_result.name_ambiguous;

        for g in &dep_result.groups {
            matched_symbols.push(MatchedSymbol {
                node_id: g.root_node_id.clone(),
                qualified_name: g.root_qualified_name.clone(),
                name: name.clone(),
                file_path: g.root_file.clone(),
            });
            for item in &g.items {
                if seen_dependents.insert(item.node_id.clone()) {
                    resolved_dependents.push(Dependent {
                        node_id: item.node_id.clone(),
                        name: item.name.clone(),
                        qualified_name: item.qualified_name.clone(),
                        node_type: item.node_type.clone(),
                        file_path: item.file_path.clone(),
                        depth: item.depth,
                        of_symbol: g.root_qualified_name.clone(),
                    });
                }
            }
            for a in &g.ambiguous {
                let key = (a.from_file.clone(), a.from_name.clone(), a.to_name.clone());
                if seen_ambiguous.insert(key) {
                    ambiguous_refusals.push(AmbiguousRefusal {
                        from_name: a.from_name.clone(),
                        from_file: a.from_file.clone(),
                        to_name: a.to_name.clone(),
                        to_type: a.to_type.clone(),
                        candidates: a.candidates.clone(),
                        of_symbol: g.root_qualified_name.clone(),
                    });
                }
            }
        }

        for u in &dep_result.stale_references {
            let key = (u.from_file.clone(), u.from_name.clone(), u.to_name.clone());
            if seen_stale.insert(key) {
                stale_references.push(StaleReference {
                    from_name: u.from_name.clone(),
                    from_file: u.from_file.clone(),
                    to_name: u.to_name.clone(),
                    to_type: u.to_type.clone(),
                });
            }
        }
    }

    Ok(StructuralVerdict {
        target: VerdictTarget::Paths(paths.to_vec()),
        resolved_dependents,
        ambiguous_refusals,
        stale_references,
        name_ambiguous,
        matched_symbols,
        graph_empty,
        explanations: None,
    })
}

// ============================================================================
// explain-v1 — evidence chains beside the verdict (ADDITIVE ONLY; nothing
// above this line changed except the optional `explanations` field, which
// is skipped when absent)
// ============================================================================

/// [`impact_verdict_for_paths`], with a machine-checkable evidence chain
/// behind every finding (`specs/explain-v1.md`).
///
/// The verdict half is produced by calling `impact_verdict_for_paths`
/// itself, unmodified, so explain cannot drift from the decision it
/// explains: same query, same findings, same booleans. Chains are then
/// attached for the same symbol set that verdict was built from, which is
/// what makes "a chain per finding" true by construction rather than by
/// coincidence.
///
/// Every chain is verifiable with
/// [`crate::graph::explain::verify_chain`] against an
/// [`ExplainGraph`] loaded from this same store. Callers handing a report
/// to someone who should not have to trust them are expected to say so.
pub async fn explain_verdict_for_paths(
    store: &Store,
    paths: &[String],
) -> Result<StructuralVerdict, FacadeError> {
    let mut verdict = impact_verdict_for_paths(store, paths).await?;

    let graph = ExplainGraph::load(&store.db, &store.project_id)
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;

    // The same two name sets `impact_verdict_for_paths` queried, recovered
    // the same way, so a chain exists for exactly the symbols it looked at.
    let touched = touched_symbol_names(&store.db, &store.project_id, paths).await?;
    let deleted = deleted_symbol_rows(&store.db, &store.project_id, paths, &touched).await?;

    let mut chains = Vec::new();
    for name in &touched {
        chains.extend(explain::explain_symbol(
            &graph,
            &store.project_id,
            name,
            &Membership::ExplicitSymbol,
        ));
    }
    for row in &deleted {
        // A deleted name's membership link is its deletion record, not a
        // literal argument — that is the whole point of S.1.
        chains.extend(explain::explain_symbol(
            &graph,
            &store.project_id,
            &row.name,
            &Membership::Deleted(row.clone()),
        ));
    }

    verdict.explanations = Some(chains);
    Ok(verdict)
}

/// [`impact_verdict`], with evidence chains. The symbol was named
/// literally by the caller, so every chain's S.1 link is
/// [`Membership::ExplicitSymbol`].
pub async fn explain_verdict(
    store: &Store,
    symbol: &str,
) -> Result<StructuralVerdict, FacadeError> {
    let mut verdict = impact_verdict(store, symbol).await?;
    verdict.explanations =
        Some(explain_chains_for_symbol(&store.db, &store.project_id, symbol).await?);
    Ok(verdict)
}

/// Chains for one symbol, for a caller that already holds its own
/// connection and does not want a second one.
///
/// Exists for the same reason [`is_indexed_conn`] and
/// [`project_dependency_result`] do: the CLI's `--json` path runs in the
/// binary crate with `client` already open, and opening a second connection
/// to the same embedded surrealkv store deadlocks on its single-writer file
/// lock. This is the function `codegraph query --kind rdeps --explain`
/// calls.
pub async fn explain_chains_for_symbol(
    db: &Surreal<Any>,
    project_id: &str,
    symbol: &str,
) -> Result<Vec<Chain>, FacadeError> {
    let graph = ExplainGraph::load(db, project_id)
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;
    Ok(explain::explain_symbol(
        &graph,
        project_id,
        symbol,
        &Membership::ExplicitSymbol,
    ))
}

/// [`deleted_symbol_names`], keeping the whole row rather than just the
/// name. A chain's S.1 link is the deletion record itself — qualified name,
/// node type and file path, all three re-checkable against
/// `deleted_symbol` — so the name alone is not enough to build one.
async fn deleted_symbol_rows(
    db: &Surreal<Any>,
    project_id: &str,
    paths: &[String],
    touched: &[String],
) -> Result<Vec<DeletedSymbol>, FacadeError> {
    let mut resp = db
        .query(
            "SELECT name, qualified_name, node_type, file_path FROM deleted_symbol \
             WHERE project_id = $pid AND file_path IN $paths",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("paths", paths.to_vec()))
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;
    let rows: Vec<surrealdb_types::Value> =
        resp.take(0).map_err(|e| FacadeError::Query(e.to_string()))?;

    let live: std::collections::HashSet<&str> = touched.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in &rows {
        let surrealdb_types::Value::Object(obj) = v else {
            continue;
        };
        let name = crate::graph::get_str(obj, "name");
        if name.is_empty() || live.contains(name.as_str()) {
            continue;
        }
        let row = DeletedSymbol {
            name,
            qualified_name: crate::graph::get_str(obj, "qualified_name"),
            node_type: crate::graph::get_str(obj, "node_type"),
            file_path: crate::graph::get_str(obj, "file_path"),
        };
        if seen.insert(row.clone()) {
            out.push(row);
        }
    }
    // `deleted_symbol` rows carry no useful intrinsic order, and chains are
    // a diffable artifact — sort so two runs agree.
    out.sort_by(|a, b| {
        (&a.name, &a.qualified_name, &a.file_path).cmp(&(&b.name, &b.qualified_name, &b.file_path))
    });
    Ok(out)
}

/// Design doc §3.3 step 1: the touched symbols for a set of (index-relative)
/// file paths, deduped to distinct names in first-seen order (determinism —
/// two files defining a same-named symbol don't double-query it).
async fn touched_symbol_names(
    db: &Surreal<Any>,
    project_id: &str,
    paths: &[String],
) -> Result<Vec<String>, FacadeError> {
    let mut resp = db
        .query(
            "SELECT name FROM code_node \
             WHERE project_id = $pid AND file_path IN $paths AND node_type != 'import'",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("paths", paths.to_vec()))
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;
    let rows: Vec<surrealdb_types::Value> =
        resp.take(0).map_err(|e| FacadeError::Query(e.to_string()))?;

    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    for v in &rows {
        let surrealdb_types::Value::Object(obj) = v else {
            continue;
        };
        let name = crate::graph::get_str(obj, "name");
        if !name.is_empty() && seen.insert(name.clone()) {
            names.push(name);
        }
    }
    Ok(names)
}

/// Names recorded by `index::index_project`'s deletion tracking as having
/// disappeared from any of `paths` (see `deleted_symbol` in
/// `src/schema.surql`) — minus names already in the live `touched` set (a
/// crate-root collision can leave a same-named live symbol in another
/// touched file; querying it twice would only duplicate work). Sorted for
/// determinism: `deleted_symbol` rows carry no useful intrinsic order.
async fn deleted_symbol_names(
    db: &Surreal<Any>,
    project_id: &str,
    paths: &[String],
    touched: &[String],
) -> Result<Vec<String>, FacadeError> {
    let mut resp = db
        .query(
            "SELECT name FROM deleted_symbol \
             WHERE project_id = $pid AND file_path IN $paths",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("paths", paths.to_vec()))
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;
    let rows: Vec<surrealdb_types::Value> =
        resp.take(0).map_err(|e| FacadeError::Query(e.to_string()))?;

    let live: std::collections::HashSet<&str> = touched.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    for v in &rows {
        let surrealdb_types::Value::Object(obj) = v else {
            continue;
        };
        let name = crate::graph::get_str(obj, "name");
        if !name.is_empty() && !live.contains(name.as_str()) && seen.insert(name.clone()) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Pure projection: `DependencyResult` → `StructuralVerdict` (design doc §2
/// projection rule). `queried_name` is the bare name every group's root
/// matched on (`dependencies::matching_roots` filters by exact `name`
/// equality, so it's the same for every group in one `DependencyResult`) —
/// threaded through explicitly rather than derived from `qualified_name`
/// since qualifier conventions differ across languages.
///
/// Exposed (not `pub(crate)`) so the CLI's `--json` path
/// (`main.rs`, a separate binary crate) can build the exact same
/// `StructuralVerdict` shape from a `DependencyResult` it already has in
/// hand, without opening a second `Store`/connection just to match the
/// gate's output.
pub fn project_dependency_result(
    target: VerdictTarget,
    queried_name: &str,
    dep_result: &DependencyResult,
    graph_empty: bool,
) -> StructuralVerdict {
    let mut matched_symbols = Vec::new();
    let mut resolved_dependents = Vec::new();
    let mut ambiguous_refusals = Vec::new();

    for g in &dep_result.groups {
        matched_symbols.push(MatchedSymbol {
            node_id: g.root_node_id.clone(),
            qualified_name: g.root_qualified_name.clone(),
            name: queried_name.to_string(),
            file_path: g.root_file.clone(),
        });
        for item in &g.items {
            resolved_dependents.push(Dependent {
                node_id: item.node_id.clone(),
                name: item.name.clone(),
                qualified_name: item.qualified_name.clone(),
                node_type: item.node_type.clone(),
                file_path: item.file_path.clone(),
                depth: item.depth,
                of_symbol: g.root_qualified_name.clone(),
            });
        }
        for a in &g.ambiguous {
            ambiguous_refusals.push(AmbiguousRefusal {
                from_name: a.from_name.clone(),
                from_file: a.from_file.clone(),
                to_name: a.to_name.clone(),
                to_type: a.to_type.clone(),
                candidates: a.candidates.clone(),
                of_symbol: g.root_qualified_name.clone(),
            });
        }
    }

    let stale_references = dep_result
        .stale_references
        .iter()
        .map(|u| StaleReference {
            from_name: u.from_name.clone(),
            from_file: u.from_file.clone(),
            to_name: u.to_name.clone(),
            to_type: u.to_type.clone(),
        })
        .collect();

    StructuralVerdict {
        target,
        resolved_dependents,
        ambiguous_refusals,
        stale_references,
        name_ambiguous: dep_result.name_ambiguous,
        matched_symbols,
        graph_empty,
        explanations: None,
    }
}

// ============================================================================
// StructuralDelta — refactor-neutrality evidence (lane 3; ADDITIVE ONLY,
// nothing above this line changed)
// ============================================================================

/// Per-symbol structure-preservation evidence for a set of touched files:
/// current-generation vs previous-generation fingerprints (see
/// `index::fingerprint` — one prior generation is retained per symbol, so
/// this compares the last two index runs). Upgrades a gate verdict from
/// "blast radius list" to "structure-preservation proof": a pure rename
/// refactor classifies every symbol `preserved` (the renamed one pairs by
/// identical fingerprint) and [`StructuralDelta::is_structure_neutral`]
/// holds. Plain serde struct, no SurrealDB types — same boundary contract
/// as `StructuralVerdict`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StructuralDelta {
    /// The touched (index-relative) file paths queried.
    pub paths: Vec<String>,

    /// Fingerprint identical across the last two generations — the change
    /// was structure-neutral for this symbol. Includes rename-paired
    /// symbols (see `DeltaSymbol::renamed_from`).
    pub preserved: Vec<DeltaSymbol>,

    /// Fingerprint moved: the symbol's 1-hop structure changed.
    pub changed: Vec<DeltaSymbol>,

    /// No previous-generation fingerprint (new symbol — or first index /
    /// post-`--force`, where history starts empty and everything is new).
    pub added: Vec<DeltaSymbol>,

    /// Symbol no longer defined (one-generation tombstone; queryable until
    /// the next re-index shifts it out).
    pub removed: Vec<DeltaSymbol>,

    /// Same semantics as `StructuralVerdict::graph_empty`: zero indexed
    /// nodes — the only state that maps to a gate skip.
    pub graph_empty: bool,
}

impl StructuralDelta {
    /// True IFF the last re-index changed NO symbol structure in the
    /// touched files: nothing changed, added, or removed (a pure rename
    /// pairs into `preserved` and stays neutral). An empty match set is
    /// trivially neutral — combine with `graph_empty` / `preserved.len()`
    /// when "we checked nothing" must be distinguished from "all clear".
    pub fn is_structure_neutral(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
    pub fn preserved_count(&self) -> usize {
        self.preserved.len()
    }
    pub fn changed_count(&self) -> usize {
        self.changed.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaSymbol {
    pub qualified_name: String,
    pub file_path: String,
    pub node_type: String,
    /// Current-generation fingerprint (hex). `None` on `removed` entries.
    pub fingerprint: Option<String>,
    /// Previous-generation fingerprint (hex). `None` on `added` entries.
    pub previous_fingerprint: Option<String>,
    /// Set only on rename-paired `preserved` entries: the qualified name
    /// this exact structure previously lived under.
    pub renamed_from: Option<String>,
}

/// One raw `fingerprint`-table row for classification — the pure core's
/// input shape, split out so the pairing rules are unit-testable without a
/// store.
#[derive(Debug, Clone)]
struct DeltaRow {
    qualified_name: String,
    file_path: String,
    node_type: String,
    hash: Option<String>,
    prev_hash: Option<String>,
}

/// Compare current vs previous generation fingerprints for symbols in the
/// touched files. THE gate-facing entry for refactor-neutrality: call after
/// a re-index, with the same paths the plan touched.
pub async fn structural_delta_for_paths(
    store: &Store,
    paths: &[String],
) -> Result<StructuralDelta, FacadeError> {
    let graph_empty = !is_indexed(store).await?;
    if paths.is_empty() {
        // No target ≠ no graph — a real, trivially-neutral, empty delta.
        return Ok(StructuralDelta { graph_empty, ..Default::default() });
    }

    let mut resp = store
        .db
        .query(
            "SELECT qualified_name, file_path, node_type, hash, prev_hash \
             FROM fingerprint WHERE project_id = $pid AND file_path IN $paths",
        )
        .bind(("pid", store.project_id.clone()))
        .bind(("paths", paths.to_vec()))
        .await
        .map_err(|e| FacadeError::Query(e.to_string()))?;
    let raw: Vec<surrealdb_types::Value> =
        resp.take(0).map_err(|e| FacadeError::Query(e.to_string()))?;

    let rows: Vec<DeltaRow> = raw
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let opt = |s: String| if s.is_empty() { None } else { Some(s) };
            Some(DeltaRow {
                qualified_name: crate::graph::get_str(obj, "qualified_name"),
                file_path: crate::graph::get_str(obj, "file_path"),
                node_type: crate::graph::get_str(obj, "node_type"),
                hash: opt(crate::graph::get_str(obj, "hash")),
                prev_hash: opt(crate::graph::get_str(obj, "prev_hash")),
            })
        })
        .collect();

    let mut delta = classify_delta(rows);
    delta.paths = paths.to_vec();
    delta.graph_empty = graph_empty;
    Ok(delta)
}

/// Pure classification + rename pairing.
///
/// Base classes come straight off the row shape (`hash` = current
/// generation, `prev_hash` = the one retained prior): both set and equal →
/// preserved; both set and different → changed; current only → added;
/// previous only (tombstone) → removed.
///
/// RENAME PAIRING: a pure rename makes the old key a `removed` candidate
/// and the new key an `added` candidate with the IDENTICAL fingerprint —
/// names never enter the hash. When a fingerprint value appears on exactly
/// ONE added and exactly ONE removed candidate, that is positive structural
/// evidence of a rename: the pair collapses into a single `preserved` entry
/// under the new name, `renamed_from` recording the old one. Any ambiguity
/// (the same fingerprint on several adds or several removes) refuses to
/// guess and leaves the candidates as honest added/removed — the same
/// no-guessing posture as the resolver's AMBIGUOUS class.
fn classify_delta(rows: Vec<DeltaRow>) -> StructuralDelta {
    use std::collections::HashMap;

    let mut delta = StructuralDelta::default();
    let mut added: Vec<DeltaRow> = Vec::new();
    let mut removed: Vec<DeltaRow> = Vec::new();

    for row in rows {
        match (&row.hash, &row.prev_hash) {
            (Some(h), Some(p)) if h == p => delta.preserved.push(symbol(&row, None)),
            (Some(_), Some(_)) => delta.changed.push(symbol(&row, None)),
            (Some(_), None) => added.push(row),
            (None, Some(_)) => removed.push(row),
            (None, None) => {} // unreachable by the writer's self-clean rule
        }
    }

    let mut add_by_hash: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, row) in added.iter().enumerate() {
        add_by_hash.entry(row.hash.clone().expect("added has hash")).or_default().push(i);
    }
    let mut rem_by_hash: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, row) in removed.iter().enumerate() {
        rem_by_hash.entry(row.prev_hash.clone().expect("removed has prev")).or_default().push(i);
    }

    let mut paired_add = vec![false; added.len()];
    let mut paired_rem = vec![false; removed.len()];
    for (hash, adds) in &add_by_hash {
        if let Some(rems) = rem_by_hash.get(hash) {
            if adds.len() == 1 && rems.len() == 1 {
                let a = &added[adds[0]];
                let r = &removed[rems[0]];
                paired_add[adds[0]] = true;
                paired_rem[rems[0]] = true;
                delta.preserved.push(DeltaSymbol {
                    qualified_name: a.qualified_name.clone(),
                    file_path: a.file_path.clone(),
                    node_type: a.node_type.clone(),
                    fingerprint: a.hash.clone(),
                    previous_fingerprint: r.prev_hash.clone(),
                    renamed_from: Some(r.qualified_name.clone()),
                });
            }
        }
    }
    for (i, row) in added.iter().enumerate() {
        if !paired_add[i] {
            delta.added.push(symbol(row, None));
        }
    }
    for (i, row) in removed.iter().enumerate() {
        if !paired_rem[i] {
            delta.removed.push(symbol(row, None));
        }
    }

    let key = |s: &DeltaSymbol| (s.file_path.clone(), s.qualified_name.clone());
    delta.preserved.sort_by_key(key);
    delta.changed.sort_by_key(key);
    delta.added.sort_by_key(key);
    delta.removed.sort_by_key(key);
    delta
}

fn symbol(row: &DeltaRow, renamed_from: Option<String>) -> DeltaSymbol {
    DeltaSymbol {
        qualified_name: row.qualified_name.clone(),
        file_path: row.file_path.clone(),
        node_type: row.node_type.clone(),
        fingerprint: row.hash.clone(),
        previous_fingerprint: row.prev_hash.clone(),
        renamed_from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::dependencies::{AmbiguousRef, DependencyGroup, DependencyNode, UnresolvedRef};

    fn dep_node(node_id: &str, name: &str, qn: &str, node_type: &str, file: &str, depth: usize) -> DependencyNode {
        DependencyNode {
            node_id: node_id.to_string(),
            name: name.to_string(),
            qualified_name: qn.to_string(),
            node_type: node_type.to_string(),
            file_path: file.to_string(),
            depth,
        }
    }

    /// The clean-verdict case: one matched root, one resolved dependent, no
    /// ambiguous/stale — `is_clean()` must be true and every field must
    /// carry over from the `DependencyResult` shape untouched.
    #[test]
    fn projects_a_clean_verdict() {
        let dep_result = DependencyResult {
            groups: vec![DependencyGroup {
                root_node_id: "root1".to_string(),
                root_qualified_name: "crate::foo".to_string(),
                root_file: "src/foo.rs".to_string(),
                items: vec![dep_node("caller1", "caller", "crate::caller", "function", "src/caller.rs", 1)],
                unresolved: vec![],
                ambiguous: vec![],
            }],
            name_ambiguous: false,
            stale_references: vec![],
            live_definitions: 1,
        };

        let verdict = project_dependency_result(VerdictTarget::Symbol("foo".to_string()), "foo", &dep_result, false);

        assert!(verdict.is_clean());
        assert_eq!(verdict.resolved_dependent_count(), 1);
        assert_eq!(verdict.ambiguous_count(), 0);
        assert_eq!(verdict.stale_count(), 0);
        assert!(!verdict.graph_empty);
        assert_eq!(verdict.matched_symbols.len(), 1);
        assert_eq!(verdict.matched_symbols[0].node_id, "root1");
        assert_eq!(verdict.matched_symbols[0].qualified_name, "crate::foo");
        assert_eq!(verdict.matched_symbols[0].name, "foo");
        assert_eq!(verdict.resolved_dependents[0].of_symbol, "crate::foo");
        assert_eq!(verdict.resolved_dependents[0].qualified_name, "crate::caller");
        match verdict.target {
            VerdictTarget::Symbol(ref s) => assert_eq!(s, "foo"),
            VerdictTarget::Paths(_) => panic!("expected Symbol target"),
        }
    }

    /// A dirty verdict: stale references and ambiguous refusals both make
    /// `is_clean()` false, and each source list projects into its dedicated
    /// output field without cross-contamination.
    #[test]
    fn projects_stale_and_ambiguous_as_dirty() {
        let dep_result = DependencyResult {
            groups: vec![DependencyGroup {
                root_node_id: "root1".to_string(),
                root_qualified_name: "crate::helper".to_string(),
                root_file: "src/helper.rs".to_string(),
                items: vec![],
                unresolved: vec![UnresolvedRef {
                    from_name: "ignored_on_rdeps".to_string(),
                    from_file: "src/x.rs".to_string(),
                    to_name: "helper".to_string(),
                    to_type: "function".to_string(),
                    depth: 1,
                }],
                ambiguous: vec![AmbiguousRef {
                    from_name: "ambig_caller".to_string(),
                    from_file: "src/y.rs".to_string(),
                    to_name: "helper".to_string(),
                    to_type: "function".to_string(),
                    candidates: vec!["crate::helper".to_string(), "other::helper".to_string()],
                    depth: 1,
                }],
            }],
            name_ambiguous: false,
            stale_references: vec![UnresolvedRef {
                from_name: "use_stale".to_string(),
                from_file: "src/stale_caller.rs".to_string(),
                to_name: "target::helper".to_string(),
                to_type: "function".to_string(),
                depth: 0,
            }],
            live_definitions: 1,
        };

        let verdict =
            project_dependency_result(VerdictTarget::Symbol("helper".to_string()), "helper", &dep_result, false);

        assert!(!verdict.is_clean());
        assert_eq!(verdict.stale_count(), 1);
        assert_eq!(verdict.stale_references[0].from_name, "use_stale");
        assert_eq!(verdict.ambiguous_count(), 1);
        assert_eq!(verdict.ambiguous_refusals[0].of_symbol, "crate::helper");
        assert_eq!(verdict.ambiguous_refusals[0].candidates.len(), 2);
        // The group's own `unresolved` (forward-only field) must never leak
        // into the verdict's dependents/ambiguous/stale — it isn't part of
        // the projection rule at all.
        assert!(verdict.resolved_dependents.is_empty());
    }

    /// `graph_empty` is a caller-supplied flag, not derived from an empty
    /// `groups` list — matching "matched nothing" (0 groups, real graph)
    /// must NOT be conflated with "no graph at all".
    #[test]
    fn graph_empty_is_independent_of_matched_symbols() {
        let empty_but_real = DependencyResult::default();
        let verdict = project_dependency_result(
            VerdictTarget::Symbol("nothing_named_this".to_string()),
            "nothing_named_this",
            &empty_but_real,
            false,
        );
        assert!(!verdict.graph_empty, "a populated graph that matched nothing is still not graph_empty");
        assert!(verdict.matched_symbols.is_empty());
        assert!(verdict.is_clean());

        let verdict_empty_graph = project_dependency_result(
            VerdictTarget::Symbol("x".to_string()),
            "x",
            &DependencyResult::default(),
            true,
        );
        assert!(verdict_empty_graph.graph_empty);
    }

    // ---- StructuralDelta classification (lane 3) ---------------------------

    fn row(qn: &str, hash: Option<&str>, prev: Option<&str>) -> DeltaRow {
        DeltaRow {
            qualified_name: qn.to_string(),
            file_path: "src/a.rs".to_string(),
            node_type: "function".to_string(),
            hash: hash.map(str::to_string),
            prev_hash: prev.map(str::to_string),
        }
    }

    /// The four base classes, straight off the row shape.
    #[test]
    fn delta_classifies_base_cases() {
        let delta = classify_delta(vec![
            row("a::same", Some("h1"), Some("h1")),
            row("a::moved", Some("h2"), Some("h3")),
            row("a::fresh", Some("h4"), None),
            row("a::gone", None, Some("h5")),
        ]);
        assert_eq!(delta.preserved.len(), 1);
        assert_eq!(delta.preserved[0].qualified_name, "a::same");
        assert_eq!(delta.changed.len(), 1);
        assert_eq!(delta.changed[0].qualified_name, "a::moved");
        assert_eq!(delta.added.len(), 1, "h4 pairs with nothing");
        assert_eq!(delta.removed.len(), 1, "h5 pairs with nothing");
        assert!(!delta.is_structure_neutral());
    }

    /// A pure rename: old key removed + new key added with the IDENTICAL
    /// fingerprint → one preserved entry with the provenance recorded.
    /// This is what makes a rename refactor PROVABLY neutral.
    #[test]
    fn delta_pairs_unique_rename_into_preserved() {
        let delta = classify_delta(vec![
            row("a::new_name", Some("h1"), None),
            row("a::old_name", None, Some("h1")),
            row("a::caller", Some("h9"), Some("h9")),
        ]);
        assert!(delta.is_structure_neutral(), "a pure rename is neutral, got {delta:?}");
        assert!(delta.added.is_empty() && delta.removed.is_empty());
        let renamed = delta
            .preserved
            .iter()
            .find(|s| s.renamed_from.is_some())
            .expect("the rename pair must surface");
        assert_eq!(renamed.qualified_name, "a::new_name");
        assert_eq!(renamed.renamed_from.as_deref(), Some("a::old_name"));
    }

    /// Ambiguous pairing (one removed hash matching TWO added ones) must
    /// refuse to guess — candidates stay honest added/removed, and the
    /// delta is NOT neutral. Same posture as the resolver's AMBIGUOUS.
    #[test]
    fn delta_refuses_ambiguous_rename_pairing() {
        let delta = classify_delta(vec![
            row("a::twin_one", Some("h1"), None),
            row("a::twin_two", Some("h1"), None),
            row("a::old", None, Some("h1")),
        ]);
        assert!(delta.preserved.is_empty(), "no guessing, got {delta:?}");
        assert_eq!(delta.added.len(), 2);
        assert_eq!(delta.removed.len(), 1);
        assert!(!delta.is_structure_neutral());
    }
}
