//! MCP server implementation using rmcp.
//!
//! Tools:
//! - codegraph_search: Find code entities by name
//! - codegraph_impact: Analyze change impact (reverse deps), optionally with
//!   machine-checkable evidence chains
//! - codegraph_verify_chain: Re-check one evidence chain against the live graph
//! - codegraph_architecture: Get project structure overview, optionally scoped
//!   to part of the tree
//! - codegraph_quality: Find coupling hotspots and circular deps
//! - codegraph_clones: Group symbols with identical structure, names ignored
//! - codegraph_plan_touching / _list / _show / _collisions / _stale / _blast:
//!   the roadmap as hyperedges over the code graph, read-only
//! - codegraph_plan_sync: re-ingest the roadmap file. The only tool that writes
//!
//! Two invariants this file keeps deliberately, both covered by
//! `tests/mcp_tools.rs`:
//!
//! 1. **An unasked-for feature changes nothing.** `impact` with `explain`
//!    off runs exactly the body it ran before explain existed
//!    ([`CodegraphServer::impact_report`]), and `architecture` with no
//!    `file_filter` runs exactly the body it ran before filtering existed.
//!    The new behaviour is appended or branched to, never woven through, so
//!    byte-identity is structural rather than only tested.
//! 2. **Nothing here reimplements a query.** Every tool calls the `graph::`
//!    entry point the CLI calls, with the CLI's arguments. Two places need
//!    that stated precisely rather than taken on trust:
//!    * the file-scoped summary has no library equivalent, because the
//!      aggregate `graph::search::project_summary` returns carries no file
//!      paths and so cannot be narrowed after the fact. It is built here
//!      from raw rows ([`scoped_summary`]), and only when a filter is
//!      actually supplied.
//!    * the explain section calls `facade::explain_chains_for_symbol`,
//!      the same function the CLI's `--explain` path calls.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::graph::explain::{self, Chain, ChainVerdict, ExplainGraph};

/// How many evidence chains `codegraph_impact` renders before it truncates.
///
/// A hub symbol can carry hundreds of dependent chains, and an MCP response
/// lands directly in an agent's context window. `explain_symbol` returns
/// stale references first, then ambiguous refusals, then dependents, so a
/// prefix cut keeps the two findings that actually gate a change and drops
/// the long tail. The count of what was dropped is always reported.
const DEFAULT_EXPLAIN_LIMIT: usize = 10;

/// MCP server backed by the codegraph SurrealDB.
#[derive(Clone)]
pub struct CodegraphServer {
    db: Arc<Surreal<Any>>,
    project_id: String,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_handler]
impl ServerHandler for CodegraphServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(format!(
            "Codegraph MCP server for project '{}'. Query code structure, dependencies, \
             structural clones, and quality metrics.\n\n\
             Every dependency answer carries one of three confidence levels and they are \
             never blended. RESOLVED: the resolver bound a reference to exactly one \
             definition. AMBIGUOUS: several definitions were admissible and it declined to \
             choose, so the candidates are named and no edge is asserted. UNRESOLVED: \
             nothing was bound. A refusal to guess is reported as a refusal, not as an \
             answer, so an empty result and an uncertain one look different here.\n\n\
             Findings from codegraph_impact can be produced with evidence chains \
             (explain=true) and any chain can then be re-checked independently with \
             codegraph_verify_chain.",
            self.project_id
        ));
        info
    }
}

// --- Tool parameter types ---

/// Parameters for searching code entities.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Search query, matched as a substring against entity names
    pub query: String,
    /// Optional: filter by node type (function, struct, trait, class, enum, interface, module)
    pub node_type: Option<String>,
    /// Maximum results to return (default: 20)
    pub limit: Option<usize>,
}

/// Parameters for impact analysis.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ImpactParams {
    /// Name of the function, struct, or type to analyze
    pub name: String,
    /// Maximum traversal depth (default: 3)
    pub depth: Option<usize>,
    /// Also surface AMBIGUOUS dependents (candidates shown, never blended
    /// into resolved results). Default: false.
    pub include_ambiguous: Option<bool>,
    /// Attach a machine-checkable evidence chain behind every finding, and a
    /// JSON block of those chains that codegraph_verify_chain accepts
    /// verbatim. Default: false, in which case the response is exactly what
    /// it is without this parameter.
    pub explain: Option<bool>,
    /// Maximum evidence chains to render when explain is true (default: 10).
    /// Stale references and ambiguous refusals come first, so the default
    /// keeps the findings that gate a change. The total is always reported,
    /// whether or not it was truncated.
    pub explain_limit: Option<usize>,
}

/// Parameters for chain verification.
///
/// `chain` is the typed [`Chain`], not a loose `serde_json::Value`, so the
/// input schema an agent reads over MCP is the real chain shape rather than
/// "any JSON". That became possible when `7e8827e` added `JsonSchema` to
/// the explain types; before it, this parameter could only be described in
/// prose.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct VerifyChainParams {
    /// One chain object, exactly as codegraph_impact emitted it under
    /// explain=true: an object with explain_version, finding, subject, steps
    /// and replay.
    pub chain: ChainArg,
}

/// A chain argument, as an object or as a JSON string holding that object.
///
/// Untagged, so the published schema is `anyOf [the real Chain shape, a
/// string]` rather than "any JSON". The string arm is not a loosening: a
/// client that builds arguments from a template routinely stringifies a
/// nested object, and that is a transport habit rather than a broken chain.
/// What neither arm accepts is something that is not a chain, which is the
/// property that matters, since a verifier that silently defaults its input
/// would pass everything.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ChainArg {
    /// The chain object itself, which is what codegraph_impact emits.
    Object(Box<Chain>),
    /// The same object serialized to a JSON string.
    Text(String),
}

/// Parameters for architecture overview.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ArchitectureParams {
    /// Optional: report only the part of the tree whose file paths match.
    /// A value containing * or ? is matched as a glob (* stops at a path
    /// separator, ** crosses it, ? is one character); any other value is
    /// matched as a plain substring. Nodes count when their own file
    /// matches; an edge counts when the file holding its source matches.
    pub file_filter: Option<String>,
    /// Maximum hub nodes to return (default: 15)
    pub limit: Option<usize>,
}

/// Parameters for quality analysis.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct QualityParams {
    /// Analysis type: "coupling" for file coupling, "circular" for circular deps, "hubs" for hub nodes
    pub analysis: String,
    /// Maximum results (default: 20)
    pub limit: Option<usize>,
}

/// Parameters for structural clone detection.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClonesParams {
    /// Minimum neighborhood size (total incident structural edges) for a
    /// symbol to participate (default: 3). Below this, "identical structure"
    /// is trivially true of unrelated code and carries no clone signal.
    pub min_edges: Option<usize>,
}

// --- Planes tool parameters ---

/// Parameters for the inverse incidence lookup.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlanTouchingParams {
    /// A repository-relative file path (src/canon.rs) or a symbol name.
    pub target: String,
}

/// Parameters for the roadmap listing. Every field is an AND, and omitting
/// one means no constraint on it.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlanListParams {
    /// Only this plane id.
    pub plane: Option<String>,
    /// Only items in this state: planned, active, done, abandoned.
    pub status: Option<String>,
    /// Only planes at this horizon: now, next, later.
    pub horizon: Option<String>,
}

impl PlanListParams {
    /// Parse the two enum filters, refusing an unknown value with the list
    /// of accepted ones rather than silently ignoring it. A filter that is
    /// quietly dropped turns "no items are active" into "here is
    /// everything", which is the wrong answer delivered confidently.
    fn to_filter(&self) -> Result<crate::plan::ops::ListFilter, String> {
        let status = match &self.status {
            None => None,
            Some(s) => Some(s.parse::<crate::plan::model::Status>().map_err(|e| {
                format!("{e}. Accepted: {}", crate::plan::model::Status::accepted())
            })?),
        };
        let horizon = match &self.horizon {
            None => None,
            Some(h) => Some(h.parse::<crate::plan::model::Horizon>().map_err(|e| {
                format!("{e}. Accepted: {}", crate::plan::model::Horizon::accepted())
            })?),
        };
        Ok(crate::plan::ops::ListFilter {
            plane: self.plane.clone(),
            status,
            horizon,
        })
    }
}

/// Parameters for showing one work item.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlanShowParams {
    /// Work item id, as written in the roadmap file.
    pub item_id: String,
}

/// Parameters for a planned change's downstream reach.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlanBlastParams {
    /// Work item id, as written in the roadmap file.
    pub item_id: String,
    /// Reverse-dependency hops from the seed set (default: 3).
    pub depth: Option<usize>,
}

/// Parameters for re-ingesting the roadmap.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlanSyncParams {
    /// Path to the roadmap file. Defaults to .codegraph/planes.yaml in the
    /// repository the server was started from.
    pub path: Option<String>,
}

impl PlanSyncParams {
    /// Where to read the roadmap from.
    ///
    /// `ops::sync` takes a path, and this server holds only a connection and
    /// a project id, so the default has to be derived rather than carried.
    /// It is derived the same way the CLI derives it, from the repository
    /// root of the working directory, and the resolved path is echoed in
    /// the report's `source` so a reader never has to guess which file was
    /// read.
    fn resolve_path(&self) -> Result<std::path::PathBuf, String> {
        if let Some(p) = &self.path {
            return Ok(std::path::PathBuf::from(p));
        }
        let cwd = std::env::current_dir()
            .map_err(|e| format!("no working directory to resolve the roadmap from: {e}"))?;
        Ok(crate::plan::default_planes_path(&crate::config::repo_root(
            &cwd,
        )))
    }
}

// --- Tool implementations ---

#[tool_router(vis = "pub")]
impl CodegraphServer {
    /// Create a new server instance.
    pub fn new(db: Arc<Surreal<Any>>, project_id: String) -> Self {
        Self {
            db,
            project_id,
            tool_router: Self::tool_router(),
        }
    }

    /// Search for code entities (functions, structs, traits, classes) by name.
    #[tool(
        name = "codegraph_search",
        description = "Answers: does a symbol with this name exist in the indexed project, and where is it defined? \
Matches the query as a substring against entity names and returns functions, structs, traits, classes, enums, \
interfaces and modules, each with its file and line.\n\n\
This tool reports definitions, not references, so the RESOLVED / AMBIGUOUS / UNRESOLVED confidence vocabulary does \
not appear in its output: a definition is either in the index or it is not. There is no include_ambiguous setting \
here for the same reason. Use codegraph_impact when the question is what depends on a symbol and how confident the \
index is about each of those dependencies.\n\n\
Several definitions sharing one name come back as several rows. That is the signal that a later bare-name query \
will be ambiguous."
    )]
    pub async fn search(&self, params: Parameters<SearchParams>) -> String {
        let p = params.0;
        let limit = p.limit.unwrap_or(20);

        match crate::graph::search::search_nodes(
            &self.db,
            &self.project_id,
            &p.query,
            p.node_type.as_deref(),
            limit,
        )
        .await
        {
            Ok(nodes) => {
                if nodes.is_empty() {
                    return format!("No results found for '{}'", p.query);
                }
                let mut out = format!("Found {} results for '{}':\n\n", nodes.len(), p.query);
                for n in &nodes {
                    out.push_str(&format!(
                        "- {} ({}) in {}:{}\n",
                        n.name,
                        n.node_type,
                        n.file_path,
                        n.start_line.unwrap_or(0)
                    ));
                }
                out
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Analyze change impact, showing what depends on a given entity.
    #[tool(
        name = "codegraph_impact",
        description = "Answers: if I change this symbol, what else is affected, and what will the index not vouch for? \
Walks reverse dependencies out from every live definition of the name, up to depth hops.\n\n\
Three confidence levels, never blended. RESOLVED means the resolver bound a reference to exactly one definition; \
these are the dependents listed by default and they are the real blast radius. AMBIGUOUS means several definitions \
were admissible and the resolver declined to choose; the candidates are named and no edge is asserted. UNRESOLVED \
means nothing was bound at all; these appear under 'unresolved reference(s) still named'.\n\n\
Set include_ambiguous=true when you are about to rename, move or delete the symbol, or when you are reviewing a \
change for safety. An AMBIGUOUS edge is a place the index will not stand behind and a human or a wider search has \
to settle it. Leave it off when you only want the dependents the index can stand behind.\n\n\
An UNRESOLVED reference is NOT automatically an incomplete rename. Two different outcomes both read as unresolved. \
resolution_outcome=no_candidates means nothing in the project answers to that name any more, which is the \
incomplete-rename signal. resolution_outcome=no_rule_matched means a definition by that name does exist but no \
resolution rule admitted it, which is ordinary for calls into a standard library or a third-party crate, for \
example std::env::args. Read resolution_outcome in the evidence chain before reporting a rename defect.\n\n\
Set explain=true for an evidence chain behind every finding. A chain is an ordered list of typed facts, each a \
claim about specific rows: which node holds the reference, which edge was captured, which candidates were pooled, \
which rules ran and what each admitted, and how the cascade ended. No step is free text. Every step is \
independently re-checkable: pass any chain object from the JSON block to codegraph_verify_chain, which re-derives \
each step against the live index instead of comparing it to a stored copy, so a chain that was edited fails."
    )]
    pub async fn impact(&self, params: Parameters<ImpactParams>) -> String {
        let p = params.0;
        let mut out = self.impact_report(&p).await;
        if p.explain.unwrap_or(false) {
            let limit = p.explain_limit.unwrap_or(DEFAULT_EXPLAIN_LIMIT);
            out.push_str(&self.explain_section(&p.name, limit).await);
        }
        out
    }

    /// Re-check one evidence chain against the live graph.
    #[tool(
        name = "codegraph_verify_chain",
        description = "Answers: is this evidence chain actually true of the index right now? Takes one chain object \
exactly as codegraph_impact emitted it under explain=true and re-derives every step against the live graph. Node \
rows are looked up again, the candidate pool is recomputed rather than trusted, and the resolver cascade is re-run. \
Returns a per-step verdict, an overall ok flag, and for every step that failed, what the graph says instead.\n\n\
This is the check that makes 'machine-checkable evidence' mean something. A chain that was tampered with, copied \
from another project, or built against an older index fails here, and the verdict names the step that caught it. \
An agent that did not produce a chain can therefore act on it without trusting whoever did.\n\n\
The RESOLVED / AMBIGUOUS / UNRESOLVED vocabulary is carried inside the chain rather than produced here: an \
edge_exists step records the confidence the resolver left on that edge, and verification checks the record still \
matches the graph. There is no include_ambiguous setting, because this tool verifies whatever chain it is given."
    )]
    pub async fn verify_chain(&self, params: Parameters<VerifyChainParams>) -> String {
        let chain = match decode_chain(params.0.chain) {
            Ok(c) => c,
            Err(e) => return format!("Error: {e}"),
        };

        let graph = match ExplainGraph::load(&self.db, &self.project_id).await {
            Ok(g) => g,
            Err(e) => return format!("Error: loading the graph to verify against failed: {e}"),
        };

        let verdict = explain::verify_chain(&chain, &graph);
        render_verdict(&chain, &verdict)
    }

    /// Get project architecture overview.
    #[tool(
        name = "codegraph_architecture",
        description = "Answers: what is in this project, and what does everything hang off? Returns the node-type, \
language and edge-type distribution, then the highest-degree entities, which are the ones most other code points \
at.\n\n\
file_filter narrows every number to one part of the tree. A value containing * or ? is matched as a glob against \
the file path (* stops at a path separator, ** crosses it, ? is one character); any other value is matched as a \
plain substring. Under a filter a node counts when its own file matches, and an edge counts when the file holding \
its source matches, so the numbers describe what that scope does rather than what is done to it. The response \
states how many files matched, and says so plainly when the answer is none.\n\n\
Degrees are counted only over edges that carry a target name, which is the calls, member_of and implements kinds. \
Purely structural edges such as containment are excluded, so a hub score means 'how much code refers to this', not \
'how deeply is this nested'. In-degree is keyed by name and type, so two symbols sharing a name share an aggregate \
in-degree.\n\n\
This tool reports structure, not resolution: its counts include edges at every confidence level, so it shows no \
RESOLVED / AMBIGUOUS / UNRESOLVED breakdown and has no include_ambiguous setting. Use codegraph_impact when you \
need to know how confident the index is about one specific dependency."
    )]
    pub async fn architecture(&self, params: Parameters<ArchitectureParams>) -> String {
        let p = params.0;
        let hub_limit = p.limit.unwrap_or(15);

        let Some(raw_filter) = p.file_filter.as_deref().filter(|f| !f.is_empty()) else {
            // No filter: the exact body this tool had before filtering
            // existed, so an unfiltered call is unchanged by that work.
            return self.architecture_report(hub_limit).await;
        };
        self.architecture_report_filtered(hub_limit, &FileFilter::new(raw_filter))
            .await
    }

    /// Code quality analysis.
    #[tool(
        name = "codegraph_quality",
        description = "Answers: where is this codebase structurally risky? Pick one of three analyses with the \
analysis parameter.\n\n\
'coupling' ranks files by afferent coupling Ca (how many files depend on this one), efferent coupling Ce (how many \
it depends on) and instability I = Ce / (Ca + Ce), where I near 1 means the file leans on much and little leans on \
it. It counts every name-based edge regardless of confidence and maps each target by a name-and-type lookup that \
takes the first match, so a name defined in several files is attributed to one of them. Read it as a structural \
estimate, not a resolved-graph measurement.\n\n\
'circular' lists file pairs that reference each other in both directions, with the edge kinds behind each \
direction. This one is resolver-backed: it uses RESOLVED name-edges plus the derived cross-file reference graph, so \
an AMBIGUOUS or UNRESOLVED reference never manufactures a cycle.\n\n\
'hubs' ranks the highest-degree entities, on the same name-based degree count codegraph_architecture uses.\n\n\
There is no include_ambiguous setting on this tool. When you need to see what the resolver refused to bind, run \
codegraph_impact with include_ambiguous=true on the specific symbol."
    )]
    pub async fn quality(&self, params: Parameters<QualityParams>) -> String {
        let p = params.0;
        let limit = p.limit.unwrap_or(20);

        match p.analysis.as_str() {
            "coupling" => {
                match crate::graph::coupling::calculate_file_coupling(
                    &self.db,
                    &self.project_id,
                    limit,
                )
                .await
                {
                    Ok(coupling) => {
                        let mut out = format!("## File Coupling (top {})\n\nCa=afferent, Ce=efferent, I=instability\n\n", limit);
                        for c in &coupling {
                            out.push_str(&format!(
                                "- {} (Ca:{} Ce:{} I:{:.2}, {} nodes)\n",
                                c.file_path, c.afferent, c.efferent, c.instability, c.node_count
                            ));
                        }
                        out
                    }
                    Err(e) => format!("Error: {e}"),
                }
            }
            "circular" => {
                match crate::graph::circular::detect_circular_deps(&self.db, &self.project_id).await
                {
                    Ok(circular) => {
                        if circular.is_empty() {
                            return "No circular dependencies detected.".to_string();
                        }
                        let mut out =
                            format!("## Circular Dependencies ({} found)\n\n", circular.len());
                        for c in &circular {
                            out.push_str(&format!(
                                "- {} ↔ {} (A→B:{}, B→A:{}) via {}\n",
                                c.file_a,
                                c.file_b,
                                c.a_to_b_edges,
                                c.b_to_a_edges,
                                c.via.join("+")
                            ));
                        }
                        out
                    }
                    Err(e) => format!("Error: {e}"),
                }
            }
            "hubs" => {
                match crate::graph::hub_nodes::find_hub_nodes(&self.db, &self.project_id, limit)
                    .await
                {
                    Ok(hubs) => {
                        let mut out = format!("## Hub Nodes (top {})\n\n", limit);
                        for h in &hubs {
                            out.push_str(&format!(
                                "- {} ({}): degree:{} in:{} out:{}, in {}\n",
                                h.name,
                                h.node_type,
                                h.total_degree,
                                h.in_degree,
                                h.out_degree,
                                h.file_path
                            ));
                        }
                        out
                    }
                    Err(e) => format!("Error: {e}"),
                }
            }
            other => format!("Unknown analysis: '{other}'. Use 'coupling', 'circular', or 'hubs'."),
        }
    }

    /// Structural clone detection over persisted fingerprints.
    #[tool(
        name = "codegraph_clones",
        description = "Answers: which symbols have the same shape as each other, whatever they are called? Groups \
symbols whose rooted 1-hop neighborhood certificate hashes identically: same edge types, same directions, same \
neighbor kinds, same multiplicities. Names are ignored entirely, so this finds structure that was copied and then \
renamed, which a text-similarity search misses.\n\n\
min_edges is the minimum neighborhood size a symbol needs to take part, default 3. It guards against trivial \
matches: a leaf function with one caller is structurally identical to every other leaf function with one caller, so \
below a minimum amount of structure 'identical certificate' carries no clone signal. Raise it for fewer, stronger \
groups; lower it and the output fills with noise.\n\n\
Three more things are excluded by design, and the tool says so rather than quietly dropping them: tombstoned rows, \
which are not live symbols; symbols whose neighborhood is pure containment, since a module holding N children of \
one kind matches every other such module and that is arity rather than behavior; and groups with one member, since \
a clone needs a counterpart.\n\n\
The RESOLVED / AMBIGUOUS / UNRESOLVED vocabulary does not apply here and there is no include_ambiguous setting. A \
group is exact hash equality over persisted fingerprints, not a resolver binding. Two members can still be false \
friends up to a 64-bit hash collision, so treat a group as a lead to read, not a proven duplicate."
    )]
    pub async fn clones(&self, params: Parameters<ClonesParams>) -> String {
        let min_edges = params.0.min_edges.unwrap_or(3);

        let groups =
            match crate::graph::clones::find_clone_groups(&self.db, &self.project_id, min_edges)
                .await
            {
                Ok(g) => g,
                Err(e) => return format!("Error: {e}"),
            };

        if groups.is_empty() {
            // "No clones" and "nothing to compare" are different answers, and
            // an agent acting on the first when the second is true would be
            // wrong. Cost one extra query only on the empty path.
            let fingerprints = count_fingerprints(&self.db, &self.project_id).await;
            return match fingerprints {
                Ok(0) => "No structural clones: this project has no fingerprint rows at all, so \
                          nothing was compared. Re-index it (codegraph index) to populate them.\n"
                    .to_string(),
                Ok(n) => format!(
                    "No structural clones among {n} fingerprinted symbol(s) at min_edges={min_edges}. \
                     Lowering min_edges widens the search, at the cost of grouping shapes that are \
                     trivially alike.\n"
                ),
                Err(e) => format!(
                    "No structural clones at min_edges={min_edges}. (Could not check how many \
                     fingerprints exist, so this may instead mean nothing was compared: {e})\n"
                ),
            };
        }

        let members: usize = groups.iter().map(|g| g.members.len()).sum();
        let mut out = format!(
            "## Structural clones: {} group(s), {} member(s), min_edges={}\n\n\
             Names are ignored entirely. Members of a group share an identical 1-hop neighborhood \
             certificate: same edge types, same directions, same neighbor kinds, same \
             multiplicities.\n\n",
            groups.len(),
            members,
            min_edges
        );
        for (i, g) in groups.iter().enumerate() {
            out.push_str(&format!(
                "group {} ({} members, {} edges, fingerprint {}):\n",
                i + 1,
                g.members.len(),
                g.edge_count,
                g.fingerprint
            ));
            for m in &g.members {
                out.push_str(&format!(
                    "  - {} ({}) in {}:{}\n",
                    if m.name.is_empty() {
                        m.qualified_name.as_str()
                    } else {
                        m.name.as_str()
                    },
                    m.node_type,
                    m.file_path,
                    m.start_line.unwrap_or(0)
                ));
            }
        }
        out
    }

    // ========================================================================
    // Planes: the roadmap as hyperedges over the code graph
    //
    // Every one of these calls `plan::ops`, the same function the matching
    // `codegraph plan ...` subcommand calls, and serializes the typed report
    // it returns. Nothing about the roadmap is computed here.
    // ========================================================================

    /// Inverse incidence: what planned work covers this file or symbol.
    #[tool(
        name = "codegraph_plan_touching",
        description = "Answers: what planned work already covers this file or symbol? Ask this BEFORE you start \
editing, because the answer tells you whether someone else's work item already owns the code you are about to \
change. Pass a repository-relative path such as src/canon.rs, or a symbol name.\n\n\
Each hit names the work item, its plane and status, the touch that matched, and how it matched. The how is the part \
to read. 'bound' means the touch is RESOLVED and points at exactly what you asked about, which is the only kind \
that is evidence of real coverage. 'candidate' means the touch is AMBIGUOUS and your target is one of several \
things it might have meant, so the plan may or may not be about your code. 'literal' means the touch is UNRESOLVED, naming your \
string verbatim without binding to anything, which happens when a plan points at a path that no longer resolves; \
those are reported rather than hidden, because a stale plan about this file is still a plan about this file, and \
codegraph_plan_stale will say why it did not bind.\n\n\
target_ids tells an empty result apart from a meaningless one: if it is empty, nothing in the index answers to that \
name, so the absence of planned work says nothing. There is no include_ambiguous setting, because AMBIGUOUS touches \
are always shown and always labelled rather than being folded into the resolved ones."
    )]
    pub async fn plan_touching(&self, params: Parameters<PlanTouchingParams>) -> String {
        let target = params.0.target;
        match crate::plan::ops::touching(&self.db, &self.project_id, &target).await {
            Ok(report) => {
                let summary = if report.hits.is_empty() && report.target_ids.is_empty() {
                    format!(
                        "No planned work touches '{target}', and nothing in the index answers to \
                         that name either, so this is not evidence that the code is unclaimed."
                    )
                } else if report.hits.is_empty() {
                    format!(
                        "No planned work touches '{target}' ({} indexed node(s) match that name).",
                        report.target_ids.len()
                    )
                } else {
                    format!(
                        "{} work item(s) already cover '{target}'. Read matched_by before you treat \
                         one as a claim on this code.",
                        report.hits.len()
                    )
                };
                report_response(&summary, &report)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// The roadmap, filtered.
    #[tool(
        name = "codegraph_plan_list",
        description = "Answers: what is on the roadmap right now? Returns planes and the work items they hold, \
each with its touch tallies. Every filter is optional and they combine with AND: plane is a plane id, status is one \
of planned, active, done or abandoned, and horizon is one of now, next or later.\n\n\
Each item reports touch_count, which is the size of its hyperedge in incidences, alongside resolved, ambiguous and \
unresolved. Those three use the resolver's own vocabulary, exactly as the code graph does. RESOLVED means the \
selector bound to one node. AMBIGUOUS means several candidates survived and none was silently chosen. UNRESOLVED \
means nothing matched, and any value above zero there means the plan points at code that is not where it says, so \
use codegraph_plan_stale to see which touches and why.\n\n\
selector_count is the entries written in the file before glob expansion, so a glob matching four files counts once \
there and four times in touch_count. There is no include_ambiguous setting: the ambiguous tally is always present \
and never merged into the resolved one."
    )]
    pub async fn plan_list(&self, params: Parameters<PlanListParams>) -> String {
        let p = params.0;
        let filter = match p.to_filter() {
            Ok(f) => f,
            Err(e) => return format!("Error: {e}"),
        };
        match crate::plan::ops::list(&self.db, &self.project_id, &filter).await {
            Ok(report) => {
                let items: usize = report.planes.iter().map(|pl| pl.items.len()).sum();
                let summary = if report.planes.is_empty() {
                    "No planes matched. If the roadmap has never been ingested, run \
                     codegraph_plan_sync first."
                        .to_string()
                } else {
                    format!("{} plane(s), {items} work item(s).", report.planes.len())
                };
                report_response(&summary, &report)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// One work item in full.
    #[tool(
        name = "codegraph_plan_show",
        description = "Answers: what exactly does this work item cover, what is it waiting on, and how far does it \
reach? Takes a work item id as written in the roadmap file.\n\n\
Returns the item's whole incidence list with a confidence on every touch, its dependencies both ways, its spec and \
notes, and a blast summary. depends_on is what this item waits for; blocks is the inverse, computed so you can see \
what finishing this unblocks without reading the file.\n\n\
Touch confidence is the resolver's vocabulary. RESOLVED means the selector bound to exactly one node. AMBIGUOUS \
means several candidates survived and the resolver refused to pick, so the candidates are listed and nothing is \
asserted. UNRESOLVED means nothing matched, and the unbound list gives the reason for each one, which is worth \
reading before concluding the plan is stale: a touch naming a specification document is unresolvable forever and \
needs no action, while a touch naming a renamed function does. There is no include_ambiguous setting, because every \
touch is shown with its confidence regardless."
    )]
    pub async fn plan_show(&self, params: Parameters<PlanShowParams>) -> String {
        let item_id = params.0.item_id;
        match crate::plan::ops::show(&self.db, &self.project_id, &item_id).await {
            Ok(report) => {
                let summary = format!(
                    "{} ({}, {}): {} touch(es), {} unresolved, reaches {} node(s) at depth {}.",
                    report.item.item_id,
                    report.item.kind,
                    report.item.status,
                    report.item.touch_count,
                    report.item.unresolved,
                    report.blast.reached_count,
                    report.blast.depth
                );
                report_response(&summary, &report)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Active items whose touch sets intersect.
    #[tool(
        name = "codegraph_plan_collisions",
        description = "Answers: are two pieces of planned work about to touch the same code? Returns every pair of \
ACTIVE work items whose touch sets intersect, each pair reported once, with the node ids and the raw selectors that \
overlap.\n\n\
This is the tool that stops two agents editing the same thing without knowing. A pair here is not a conflict yet, \
it is a warning that the two items share code and that whoever lands second will be rebasing onto the other.\n\n\
Only RESOLVED touches contribute to an overlap. An AMBIGUOUS touch names several possible targets and an \
UNRESOLVED one names nothing at all, and neither is evidence that two items really meet, so counting them would \
manufacture collisions that are not there. That also means this report can understate: if an item's touches did not \
bind, its real overlap is invisible here, so check codegraph_plan_stale alongside it. active_items says how many \
items were eligible, which is why an empty report is empty. There is no include_ambiguous setting, for the reason \
above."
    )]
    pub async fn plan_collisions(&self) -> String {
        match crate::plan::ops::collisions(&self.db, &self.project_id).await {
            Ok(report) => {
                let summary = if report.collisions.is_empty() {
                    format!(
                        "No collisions among {} active item(s).",
                        report.active_items
                    )
                } else {
                    format!(
                        "{} pair(s) of active items share code, out of {} active item(s).",
                        report.collisions.len(),
                        report.active_items
                    )
                };
                report_response(&summary, &report)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Items whose plan points at code that is not there.
    #[tool(
        name = "codegraph_plan_stale",
        description = "Answers: which plans point at code that is no longer there? Returns every PLANNED or ACTIVE \
work item carrying an UNRESOLVED touch, with the reason each one failed to bind. Items that are done or abandoned \
are skipped: a finished plan pointing at code that has since moved is history, not rot.\n\n\
A plan whose touches no longer bind is exactly as serious as a stale code reference, and it is reported in the same \
RESOLVED, AMBIGUOUS, UNRESOLVED vocabulary so the same judgment applies. Read the reason before acting: \
no_such_file, no_such_symbol and no_glob_match are different problems with different fixes, and by_reason tallies \
them heaviest first.\n\n\
actionable counts unresolved TOUCHES, not items, so it is at least the number of items and usually more, because \
one item can point at several missing things. Every reason is actionable now. A file that exists but holds no \
indexable symbols binds RESOLVED with indexed false and never reaches this report at all, so nothing here is a \
finding you can do nothing about.\n\n\
There is no include_ambiguous setting. AMBIGUOUS touches are a different finding and are not stale: they matched \
several things rather than nothing, and they show up in codegraph_plan_show and codegraph_plan_list instead."
    )]
    pub async fn plan_stale(&self) -> String {
        match crate::plan::ops::stale(&self.db, &self.project_id).await {
            Ok(report) => {
                let summary = if report.items.is_empty() {
                    "No item points at missing code.".to_string()
                } else {
                    format!(
                        "{} item(s) carry {} unresolved touch(es) needing action.",
                        report.items.len(),
                        report.actionable
                    )
                };
                report_response(&summary, &report)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// How far a planned change reaches.
    #[tool(
        name = "codegraph_plan_blast",
        description = "Answers: if this planned work lands, what else is downstream of it? Takes the union of the \
item's touch set as seeds and walks reverse dependencies out from them, to depth hops (default 3).\n\n\
A file or glob touch contributes every symbol defined in the file it matched, because a path has no node of its own \
to walk from. Reached nodes exclude the seeds themselves, and reached_files says how many distinct files they live \
in.\n\n\
The number is a floor, not a ceiling, and the report says why. Only RESOLVED touches can be seeds: an AMBIGUOUS \
touch names several possible targets and an UNRESOLVED one names nothing, so neither can be walked from. Those are \
listed in unbound_touches rather than dropped, because a blast radius computed over an incomplete seed set is an \
underestimate and you have to know that before you trust it as a bound on the work. There is no include_ambiguous \
setting: an ambiguous seed would mean guessing which candidate the plan meant, and guessing is the thing this graph \
does not do."
    )]
    pub async fn plan_blast(&self, params: Parameters<PlanBlastParams>) -> String {
        let p = params.0;
        let depth = p.depth.unwrap_or(3);
        match crate::plan::ops::blast(&self.db, &self.project_id, &p.item_id, depth).await {
            Ok(report) => {
                let summary = format!(
                    "{} reaches {} node(s) in {} file(s) from {} seed(s) at depth {}{}.",
                    report.item_id,
                    report.reached.len(),
                    report.reached_files,
                    report.seeds.len(),
                    report.depth,
                    if report.unbound_touches.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", an underestimate: {} touch(es) never bound and could not be walked \
                             from",
                            report.unbound_touches.len()
                        )
                    }
                );
                report_response(&summary, &report)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Re-ingest the roadmap file. The only tool here that writes.
    #[tool(
        name = "codegraph_plan_sync",
        description = "Answers: the roadmap file changed, so make the graph match it. THIS IS THE ONLY TOOL ON \
THIS SERVER THAT WRITES. It re-reads .codegraph/planes.yaml, re-resolves every touch against the current index, and \
replaces the stored planes, items and touches for this project. Planes and items that are no longer in the file are \
dropped, and the report says how many.\n\n\
It refuses on a validation error. A file that breaks a rule stops the whole run before anything is written, and the \
error lists every violation rather than the first, so nothing is half applied. The roadmap file is the source of \
truth; this tool never edits it, it only indexes it.\n\n\
The report counts touches in the resolver's vocabulary. RESOLVED bound to exactly one node, AMBIGUOUS had several \
candidates and picked none, UNRESOLVED matched nothing. Check indexed_files first: if it is zero, the project has \
never been indexed and every touch will be UNRESOLVED for a reason that has nothing to do with the roadmap. \
unresolved_by_reason says which of the failures are worth acting on. There is no include_ambiguous setting, because \
both the ambiguous and the unresolved entries are listed in full.\n\n\
By default the file is found at .codegraph/planes.yaml in the repository this server was started from. Pass path to \
read a different file."
    )]
    pub async fn plan_sync(&self, params: Parameters<PlanSyncParams>) -> String {
        let planes_path = match params.0.resolve_path() {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };
        match crate::plan::ops::sync(&self.db, &self.project_id, &planes_path).await {
            Ok(report) => {
                let summary = format!(
                    "Ingested {} from {}: {} plane(s), {} item(s), {} touch(es) ({} resolved, {} \
                     ambiguous, {} unresolved) against {} indexed file(s).",
                    report.project_id,
                    report.source.display(),
                    report.planes,
                    report.items,
                    report.touches,
                    report.resolved,
                    report.ambiguous,
                    report.unresolved,
                    report.indexed_files
                );
                report_response(&summary, &report)
            }
            // The refusal path. `ops::sync` validates before it writes, so
            // an error here means nothing was written, and saying so is the
            // difference between a caller retrying and a caller assuming a
            // partial roadmap is now in the graph.
            Err(e) => format!(
                "Error: {e}\n\nNothing was written. The roadmap file is validated before any row \
                 is touched, so the stored plan is exactly what it was before this call."
            ),
        }
    }
}

// ============================================================================
// Impact: the pre-explain body, and the explain section appended to it
// ============================================================================

impl CodegraphServer {
    /// The impact response exactly as it was before `explain` existed.
    ///
    /// Kept as its own function rather than as a branch inside [`Self::impact`]
    /// so that "explain off changes nothing" is a property of the structure.
    /// `tests/mcp_tools.rs::impact_with_explain_off_is_byte_identical_to_today`
    /// pins it against output captured from the pre-change tree.
    async fn impact_report(&self, p: &ImpactParams) -> String {
        let depth = p.depth.unwrap_or(3);
        let include_ambiguous = p.include_ambiguous.unwrap_or(false);

        match crate::graph::dependencies::get_reverse_dependencies(
            &self.db,
            &self.project_id,
            &p.name,
            &crate::graph::NAME_EDGE_TYPES,
            depth,
            include_ambiguous,
        )
        .await
        {
            Ok(result) => {
                if result.groups.is_empty() && result.stale_references.is_empty() {
                    return format!("'{}' has no reverse dependencies", p.name);
                }
                let total: usize = result.groups.iter().map(|g| g.items.len()).sum();
                let mut out = format!(
                    "Impact analysis for '{}': {total} dependent(s) (depth {depth}):\n\n",
                    p.name
                );
                if result.name_ambiguous {
                    out.push_str(&format!(
                        "'{}' matches {} distinct symbols, shown separately:\n\n",
                        p.name,
                        result.groups.len()
                    ));
                }
                for g in &result.groups {
                    if result.name_ambiguous {
                        out.push_str(&format!("-- {} ({}) --\n", g.root_qualified_name, g.root_file));
                    }
                    for d in &g.items {
                        let indent = "  ".repeat(d.depth);
                        out.push_str(&format!(
                            "{indent}- {} ({}) in {}\n",
                            d.name, d.node_type, d.file_path
                        ));
                    }
                    if include_ambiguous {
                        for a in &g.ambiguous {
                            let indent = "  ".repeat(a.depth);
                            out.push_str(&format!(
                                "{indent}- [AMBIGUOUS] '{}' ({}), candidates: {}\n",
                                a.to_name,
                                a.to_type,
                                a.candidates.join(", ")
                            ));
                        }
                    }
                }
                if !result.stale_references.is_empty() {
                    let reason = if result.live_definitions == 0 {
                        "no live symbol currently has this name; check for an incomplete rename"
                    } else {
                        "this name still has live definitions; these references could not be bound to any of them"
                    };
                    out.push_str(&format!(
                        "\n{} unresolved reference(s) still named '{}' ({reason}):\n",
                        result.stale_references.len(),
                        p.name
                    ));
                    for u in &result.stale_references {
                        out.push_str(&format!(
                            "  - {} ({}) → [UNRESOLVED] '{}' ({})\n",
                            u.from_name, u.from_file, u.to_name, u.to_type
                        ));
                    }
                }
                out
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// The evidence-chain section appended when `explain` is on.
    ///
    /// Chains come from the facade, not from a second implementation here.
    /// `facade::explain_chains_for_symbol` is the same function the CLI's
    /// `--explain` path calls, and it takes the already-open connection
    /// rather than opening its own, because a second connection to an
    /// embedded surrealkv store deadlocks on its single-writer file lock.
    ///
    /// This used to call `ExplainGraph::load` and `explain::explain_symbol`
    /// directly, because `src/main.rs` re-declared its own module tree and
    /// the bin crate's copy of this file had no `crate::facade` to reach.
    /// `main.rs` now uses the library crate, so the wrapper resolves in both
    /// and the workaround is gone.
    async fn explain_section(&self, symbol: &str, limit: usize) -> String {
        let chains =
            match crate::facade::explain_chains_for_symbol(&self.db, &self.project_id, symbol).await
            {
                Ok(c) => c,
                Err(e) => return format!("\n## Evidence chains\n\nError: {e}\n"),
            };

        if chains.is_empty() {
            return "\n## Evidence chains\n\nNo findings to explain for this symbol.\n".to_string();
        }

        let total = chains.len();
        let shown: &[Chain] = &chains[..limit.min(total)];

        let mut out = if shown.len() == total {
            format!("\n## Evidence chains ({total})\n\n")
        } else {
            format!(
                "\n## Evidence chains ({} of {total} shown; raise explain_limit for the rest. \
                 Stale references and ambiguous refusals are ordered first.)\n\n",
                shown.len()
            )
        };
        for c in shown {
            out.push_str(&c.render());
            out.push('\n');
        }

        out.push_str(
            "Every step above is a claim about specific rows and is re-checkable. Pass any object \
             from the block below to codegraph_verify_chain, unchanged, to have it re-derived \
             against the live index.\n\n",
        );
        match serde_json::to_string_pretty(shown) {
            Ok(json) => {
                out.push_str("```json\n");
                out.push_str(&json);
                out.push_str("\n```\n");
            }
            Err(e) => out.push_str(&format!(
                "(the chains above could not be serialized to JSON: {e})\n"
            )),
        }
        out
    }
}

/// Resolve a [`ChainArg`] to a [`Chain`].
///
/// The object arm is already typed, so it needs no work: rmcp's own
/// extractor rejected anything that was not a chain before this function
/// ran. Only the string arm has parsing left to do, and a string that does
/// not hold a chain is an error, never a default.
fn decode_chain(arg: ChainArg) -> Result<Chain, String> {
    match arg {
        ChainArg::Object(chain) => Ok(*chain),
        ChainArg::Text(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "the chain argument was a string, but not a codegraph evidence chain: {e}. Pass \
                 one object from the JSON block codegraph_impact emits under explain=true, \
                 unchanged."
            )
        }),
    }
}

/// A one-line human summary, then the typed report as JSON.
///
/// Same shape for every planes tool: an agent reads the JSON, a human
/// skims the first line. The report is whatever `plan::ops` returned,
/// serialized whole and unedited, so a field added to a report struct
/// reaches MCP without anything here changing.
fn report_response(summary: &str, report: &impl Serialize) -> String {
    match serde_json::to_string_pretty(report) {
        Ok(json) => format!("{summary}\n\n```json\n{json}\n```\n"),
        Err(e) => format!("{summary}\n\n(the report could not be serialized to JSON: {e})\n"),
    }
}

/// Human rendering of a verdict, followed by the verdict as JSON.
fn render_verdict(chain: &Chain, verdict: &ChainVerdict) -> String {
    let failures = verdict.failures();
    let mut out = format!(
        "{}: {}\n",
        chain.finding.as_tag(),
        chain.subject
    );
    if verdict.ok {
        out.push_str(&format!(
            "verify: PASS ({} step(s) re-derived against the live index)\n",
            verdict.steps.len()
        ));
    } else {
        out.push_str(&format!(
            "verify: FAIL ({} step(s) checked, {} failed)\n",
            verdict.steps.len(),
            failures.len()
        ));
        for f in &failures {
            out.push_str(&format!(
                "  step {} ({}): {}\n",
                f.index,
                f.fact,
                f.reason.as_deref().unwrap_or("no reason recorded")
            ));
        }
    }
    match serde_json::to_string_pretty(verdict) {
        Ok(json) => {
            out.push_str("\n```json\n");
            out.push_str(&json);
            out.push_str("\n```\n");
        }
        Err(e) => out.push_str(&format!("\n(verdict could not be serialized: {e})\n")),
    }
    out
}

// ============================================================================
// Architecture: unfiltered (unchanged) and file-scoped
// ============================================================================

impl CodegraphServer {
    /// The architecture response exactly as it was before `file_filter` did
    /// anything, for a call that supplies no filter.
    async fn architecture_report(&self, hub_limit: usize) -> String {
        let mut out = String::new();

        match crate::graph::search::project_summary(&self.db, &self.project_id).await {
            Ok(summary) => {
                out.push_str("## Project Structure\n\n");
                out.push_str("**Node types:**\n");
                for (nt, c) in &summary.node_types {
                    out.push_str(&format!("- {nt}: {c}\n"));
                }
                out.push_str("\n**Languages:**\n");
                for (l, c) in &summary.languages {
                    out.push_str(&format!("- {l}: {c}\n"));
                }
                out.push_str("\n**Edge types:**\n");
                for (et, c) in &summary.edge_types {
                    out.push_str(&format!("- {et}: {c}\n"));
                }
            }
            Err(e) => out.push_str(&format!("Error: {e}\n")),
        }

        out.push_str(&format!("\n## Hub Nodes (top {hub_limit})\n\n"));
        match crate::graph::hub_nodes::find_hub_nodes(&self.db, &self.project_id, hub_limit).await {
            Ok(hubs) => {
                for h in &hubs {
                    out.push_str(&format!(
                        "- {} ({}): in:{} out:{} total:{}, in {}\n",
                        h.name, h.node_type, h.in_degree, h.out_degree, h.total_degree, h.file_path
                    ));
                }
            }
            Err(e) => out.push_str(&format!("Error: {e}\n")),
        }

        out
    }

    /// The same overview, scoped to the files a filter matches.
    ///
    /// Hubs are computed over the whole project and then narrowed, rather
    /// than narrowed and then ranked, because a hub's degree is a property
    /// of the whole graph: a function called from everywhere is a hub of
    /// this project even when the filter only shows the file it lives in.
    /// The limit is applied last, so a filtered call still returns up to
    /// `hub_limit` rows instead of whatever survives a pre-truncated list.
    async fn architecture_report_filtered(
        &self,
        hub_limit: usize,
        filter: &FileFilter<'_>,
    ) -> String {
        let mut out = String::new();

        let summary = match scoped_summary(&self.db, &self.project_id, filter).await {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}\n"),
        };

        out.push_str("## Project Structure\n\n");
        out.push_str(&format!(
            "Scope: file_filter {:?} ({} match), {} of {} indexed file(s), {} node(s).\n",
            filter.raw,
            filter.kind(),
            summary.matched_files,
            summary.total_files,
            summary.nodes
        ));
        out.push_str(
            "A node counts when its own file matches; an edge counts when the file holding its \
             source matches.\n\n",
        );

        if summary.matched_files == 0 {
            out.push_str(&format!(
                "Nothing matched, so there is nothing to report at this filter. Indexed paths look \
                 like: {}.\n",
                summary.sample_paths.join(", ")
            ));
            return out;
        }

        out.push_str("**Node types:**\n");
        for (nt, c) in &summary.node_types {
            out.push_str(&format!("- {nt}: {c}\n"));
        }
        out.push_str("\n**Languages:**\n");
        for (l, c) in &summary.languages {
            out.push_str(&format!("- {l}: {c}\n"));
        }
        out.push_str("\n**Edge types:**\n");
        for (et, c) in &summary.edge_types {
            out.push_str(&format!("- {et}: {c}\n"));
        }

        out.push_str(&format!(
            "\n## Hub Nodes (top {hub_limit} within {:?})\n\n",
            filter.raw
        ));
        // usize::MAX makes `find_hub_nodes`'s own truncation a no-op, so the
        // narrowing below sees the full ranking.
        match crate::graph::hub_nodes::find_hub_nodes(&self.db, &self.project_id, usize::MAX).await {
            Ok(hubs) => {
                let mut shown = 0usize;
                for h in hubs.iter().filter(|h| filter.matches(&h.file_path)) {
                    if shown == hub_limit {
                        break;
                    }
                    shown += 1;
                    out.push_str(&format!(
                        "- {} ({}): in:{} out:{} total:{}, in {}\n",
                        h.name, h.node_type, h.in_degree, h.out_degree, h.total_degree, h.file_path
                    ));
                }
                if shown == 0 {
                    // Reachable only when every node the filter matched is an
                    // `import`, which `find_hub_nodes` excludes by design.
                    // Degree zero is not the reason: a zero-degree node is
                    // still ranked and would have printed.
                    out.push_str(
                        "(this scope matched only import nodes, which are never ranked as hubs)\n",
                    );
                }
            }
            Err(e) => out.push_str(&format!("Error: {e}\n")),
        }

        out
    }
}

// ============================================================================
// File filtering
// ============================================================================

/// A `file_filter` argument and how it is matched.
///
/// Two modes rather than one because both readings of "focus on a specific
/// file path" are in use: an agent that knows a path fragment types
/// `graph/`, and an agent that knows a subtree types `src/graph/**`. A value
/// carrying `*` or `?` is a glob; everything else is a substring, which is
/// what this parameter was documented as before it did anything.
pub(crate) struct FileFilter<'a> {
    raw: &'a str,
    glob: bool,
}

impl<'a> FileFilter<'a> {
    pub(crate) fn new(raw: &'a str) -> Self {
        Self {
            raw,
            glob: raw.contains('*') || raw.contains('?'),
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        if self.glob {
            "glob"
        } else {
            "substring"
        }
    }

    pub(crate) fn matches(&self, path: &str) -> bool {
        if self.glob {
            glob_match(self.raw, path)
        } else {
            path.contains(self.raw)
        }
    }
}

/// Match `path` against a glob `pattern`.
///
/// `*` matches any run of characters that does not cross a path separator,
/// `**` matches any run including separators, `?` matches one non-separator
/// character, and everything else is literal. `a/**/b` also matches `a/b`,
/// which is the convention every tool that writes `src/**` expects.
///
/// Backtracking, so a pathological pattern is quadratic in the path length.
/// Both inputs here are one filesystem path and one hand-typed filter, so
/// the bound is not worth an automaton.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    glob_here(&p, &s)
}

fn glob_here(p: &[char], s: &[char]) -> bool {
    let Some(&first) = p.first() else {
        return s.is_empty();
    };
    match first {
        '*' if p.get(1) == Some(&'*') => {
            // Collapse `***` and friends down to one `**`.
            let mut rest = &p[2..];
            while rest.first() == Some(&'*') {
                rest = &rest[1..];
            }
            if glob_here(rest, s) {
                return true;
            }
            // `a/**/b` matching `a/b`: let the `**` swallow the separator
            // that follows it rather than requiring a directory in between.
            if rest.first() == Some(&'/') && glob_here(&rest[1..], s) {
                return true;
            }
            (0..s.len()).any(|i| glob_here(rest, &s[i + 1..]))
        }
        '*' => {
            let rest = &p[1..];
            if glob_here(rest, s) {
                return true;
            }
            for i in 0..s.len() {
                if s[i] == '/' {
                    return false;
                }
                if glob_here(rest, &s[i + 1..]) {
                    return true;
                }
            }
            false
        }
        '?' => matches!(s.first(), Some(&c) if c != '/') && glob_here(&p[1..], &s[1..]),
        lit => matches!(s.first(), Some(&c) if c == lit) && glob_here(&p[1..], &s[1..]),
    }
}

// ============================================================================
// File-scoped summary
// ============================================================================

/// The node/language/edge distribution over the files a filter matched.
pub(crate) struct ScopedSummary {
    pub(crate) total_files: usize,
    pub(crate) matched_files: usize,
    pub(crate) nodes: usize,
    pub(crate) node_types: Vec<(String, i64)>,
    pub(crate) languages: Vec<(String, i64)>,
    pub(crate) edge_types: Vec<(String, i64)>,
    /// A few real indexed paths, so a filter that matched nothing can be
    /// corrected without a second round trip.
    pub(crate) sample_paths: Vec<String>,
}

/// `graph::search::project_summary`, restricted to matching files.
///
/// Built here rather than in `graph::search` because the aggregate that
/// function returns carries no file paths, so there is nothing to narrow
/// after the fact. Two scans, the same shape `hub_nodes` already runs.
pub(crate) async fn scoped_summary(
    db: &Surreal<Any>,
    project_id: &str,
    filter: &FileFilter<'_>,
) -> anyhow::Result<ScopedSummary> {
    use crate::graph::get_str;

    let node_rows: Vec<surrealdb_types::Value> = db
        .query(
            "SELECT node_id, node_type, language, file_path FROM code_node WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await?
        .take(0)?;

    let mut all_files: HashSet<String> = HashSet::new();
    let mut matched_files: HashSet<String> = HashSet::new();
    let mut in_scope: HashSet<String> = HashSet::new();
    let mut node_types: BTreeMap<String, i64> = BTreeMap::new();
    let mut languages: BTreeMap<String, i64> = BTreeMap::new();
    let mut nodes = 0usize;

    for v in &node_rows {
        let surrealdb_types::Value::Object(obj) = v else {
            continue;
        };
        let file_path = get_str(obj, "file_path");
        if !file_path.is_empty() {
            all_files.insert(file_path.clone());
        }
        if !filter.matches(&file_path) {
            continue;
        }
        if !file_path.is_empty() {
            matched_files.insert(file_path);
        }
        nodes += 1;
        let node_id = get_str(obj, "node_id");
        if !node_id.is_empty() {
            in_scope.insert(node_id);
        }
        let nt = get_str(obj, "node_type");
        if !nt.is_empty() {
            *node_types.entry(nt).or_insert(0) += 1;
        }
        let lang = get_str(obj, "language");
        if !lang.is_empty() {
            *languages.entry(lang).or_insert(0) += 1;
        }
    }

    let edge_rows: Vec<surrealdb_types::Value> = db
        .query("SELECT from_id, edge_type FROM code_edge WHERE project_id = $pid")
        .bind(("pid", project_id.to_string()))
        .await?
        .take(0)?;

    let mut edge_types: BTreeMap<String, i64> = BTreeMap::new();
    for v in &edge_rows {
        let surrealdb_types::Value::Object(obj) = v else {
            continue;
        };
        if !in_scope.contains(&get_str(obj, "from_id")) {
            continue;
        }
        let et = get_str(obj, "edge_type");
        if !et.is_empty() {
            *edge_types.entry(et).or_insert(0) += 1;
        }
    }

    let mut sample_paths: Vec<String> = all_files.iter().cloned().collect();
    sample_paths.sort();
    sample_paths.truncate(3);

    Ok(ScopedSummary {
        total_files: all_files.len(),
        matched_files: matched_files.len(),
        nodes,
        node_types: by_count_desc(node_types),
        languages: by_count_desc(languages),
        edge_types: by_count_desc(edge_types),
        sample_paths,
    })
}

/// Highest count first, then alphabetically, so two runs over one index
/// print the same order.
fn by_count_desc(map: BTreeMap<String, i64>) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// How many live fingerprint rows the project has, to tell "no clones" apart
/// from "nothing was compared".
async fn count_fingerprints(db: &Surreal<Any>, project_id: &str) -> anyhow::Result<usize> {
    let rows: Vec<surrealdb_types::Value> = db
        .query("SELECT hash FROM fingerprint WHERE project_id = $pid")
        .bind(("pid", project_id.to_string()))
        .await?
        .take(0)?;
    Ok(rows
        .iter()
        .filter(|v| match v {
            surrealdb_types::Value::Object(obj) => !crate::graph::get_str(obj, "hash").is_empty(),
            _ => false,
        })
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_is_a_glob_only_when_it_looks_like_one() {
        assert_eq!(FileFilter::new("src/graph/").kind(), "substring");
        assert_eq!(FileFilter::new("src/graph/**").kind(), "glob");
        assert_eq!(FileFilter::new("a?c.rs").kind(), "glob");
        // A substring filter matches anywhere in the path, which is what
        // this parameter was documented as doing before it did anything.
        assert!(FileFilter::new("graph/").matches("src/graph/mod.rs"));
        assert!(!FileFilter::new("graph/").matches("src/index/mod.rs"));
    }

    #[test]
    fn single_star_stops_at_a_separator_and_double_star_crosses_it() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/graph/mod.rs"));
        assert!(glob_match("src/**", "src/graph/mod.rs"));
        assert!(glob_match("src/**/*.rs", "src/graph/mod.rs"));
        assert!(glob_match("**/*.rs", "src/graph/mod.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("*.rs", "main.rs"));
    }

    #[test]
    fn a_double_star_directory_also_matches_no_directory() {
        // `src/**/mod.rs` matching `src/mod.rs` is what every caller that
        // writes `src/**` expects, and the reason the `**` arm has its own
        // separator-swallowing case.
        assert!(glob_match("src/**/mod.rs", "src/mod.rs"));
        assert!(glob_match("src/**/mod.rs", "src/graph/mod.rs"));
        assert!(glob_match("src/**/mod.rs", "src/a/b/c/mod.rs"));
        assert!(!glob_match("src/**/mod.rs", "tests/mod.rs"));
    }

    #[test]
    fn question_mark_is_one_character_and_never_a_separator() {
        assert!(glob_match("a?c.rs", "abc.rs"));
        assert!(!glob_match("a?c.rs", "ac.rs"));
        assert!(!glob_match("a?c.rs", "a/c.rs"));
    }

    #[test]
    fn a_literal_pattern_is_an_exact_match_not_a_substring() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("main.rs", "src/main.rs"));
        assert!(!glob_match("src/main", "src/main.rs"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn repeated_stars_collapse_rather_than_multiplying_the_search() {
        assert!(glob_match("src/***/mod.rs", "src/graph/mod.rs"));
        assert!(glob_match("****", "any/path/at/all.rs"));
    }

    /// House style, enforced over the whole file rather than over the tool
    /// descriptions alone.
    ///
    /// The narrower version of this test only checked descriptions, which
    /// let five em dashes sit in the output format strings: text an agent
    /// reads and a human reads over its shoulder, every bit as public as a
    /// description. Checking the source itself catches a new one wherever
    /// it is added, including in a comment.
    #[test]
    fn no_em_dash_survives_anywhere_in_this_file() {
        let src = include_str!("server.rs");
        let offenders: Vec<(usize, &str)> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains('\u{2014}') || l.contains(" \u{2013} "))
            .map(|(i, l)| (i + 1, l.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "em dash or en-dash-as-punctuation in src/mcp/server.rs: {offenders:#?}"
        );
    }

    #[test]
    fn counts_come_back_highest_first_then_alphabetically() {
        let mut m = BTreeMap::new();
        m.insert("zebra".to_string(), 5);
        m.insert("apple".to_string(), 5);
        m.insert("middle".to_string(), 9);
        assert_eq!(
            by_count_desc(m),
            vec![
                ("middle".to_string(), 9),
                ("apple".to_string(), 5),
                ("zebra".to_string(), 5),
            ]
        );
    }

    /// Decode the way rmcp's extractor decodes, over the whole arguments
    /// object, so this exercises the real MCP boundary rather than an
    /// internal helper.
    fn decode_params(args: serde_json::Value) -> Result<VerifyChainParams, String> {
        serde_json::from_value(args).map_err(|e| e.to_string())
    }

    #[test]
    fn a_chain_argument_is_never_defaulted_into_an_empty_chain() {
        // The point of the typed parameter: junk is refused at the boundary
        // by serde, before the handler runs, rather than deserialized into
        // an empty chain that would then "verify" vacuously with zero steps.
        assert!(decode_params(serde_json::json!({"chain": {"finding": "stale_reference"}})).is_err());
        assert!(decode_params(serde_json::json!({"chain": 7})).is_err());
        assert!(decode_params(serde_json::json!({})).is_err());
    }

    #[test]
    fn a_string_argument_reaches_the_string_arm_and_still_has_to_be_a_chain() {
        // A bare string decodes fine as ChainArg::Text: the schema admits
        // it, so the parse happens in decode_chain instead, where it fails.
        let p = decode_params(serde_json::json!({"chain": "not json at all"}))
            .expect("a string is a valid ChainArg");
        let err = decode_chain(p.chain).expect_err("but not a valid chain");
        assert!(err.contains("not a codegraph evidence chain"), "{err}");
    }

    #[test]
    fn the_object_arm_round_trips_without_reparsing() {
        let chain = Chain {
            explain_version: crate::graph::explain::EXPLAIN_VERSION,
            finding: crate::graph::explain::FindingKind::StaleReference,
            subject: "helper".to_string(),
            steps: Vec::new(),
            replay: Vec::new(),
        };
        let args = serde_json::json!({ "chain": serde_json::to_value(&chain).unwrap() });
        let decoded = decode_chain(decode_params(args).expect("decodes").chain).expect("is a chain");
        assert_eq!(decoded, chain);
    }
}

/// Start the MCP server on stdio.
pub async fn serve_stdio(db: Arc<Surreal<Any>>, project_id: String) -> anyhow::Result<()> {
    tracing::info!(project_id = %project_id, "starting MCP server on stdio");

    let server = CodegraphServer::new(db, project_id);
    let transport = rmcp::transport::io::stdio();

    let service = server.serve(transport).await?;
    service.waiting().await?;

    tracing::info!("MCP server shut down");
    Ok(())
}
