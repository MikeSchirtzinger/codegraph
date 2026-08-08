//! MCP server implementation using rmcp.
//!
//! Tools:
//! - codegraph_search: Find code entities by name
//! - codegraph_impact: Analyze change impact (reverse deps)
//! - codegraph_architecture: Get project structure overview
//! - codegraph_quality: Find coupling hotspots and circular deps

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
            "Codegraph MCP server for project '{}'. \
             Query code structure, dependencies, and quality metrics.",
            self.project_id
        ));
        info
    }
}

// --- Tool parameter types ---

/// Parameters for searching code entities.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Search query — matches against entity names (substring match)
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
}

/// Parameters for architecture overview.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ArchitectureParams {
    /// Optional: focus on a specific file path (substring match)
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

// --- Tool implementations ---

#[tool_router]
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
        description = "Search for code entities by name. Returns functions, structs, traits, classes matching the query."
    )]
    async fn search(&self, params: Parameters<SearchParams>) -> String {
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

    /// Analyze change impact — shows what depends on a given entity.
    #[tool(
        name = "codegraph_impact",
        description = "Analyze change impact — shows all code that depends on a given function/struct/trait, so you know what might break."
    )]
    async fn impact(&self, params: Parameters<ImpactParams>) -> String {
        let p = params.0;
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
                        "'{}' matches {} distinct symbols — shown separately:\n\n",
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
                                "{indent}- [AMBIGUOUS] '{}' ({}) — candidates: {}\n",
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

    /// Get project architecture overview.
    #[tool(
        name = "codegraph_architecture",
        description = "Get project architecture overview — node/edge distribution, languages, and the most-connected hub entities."
    )]
    async fn architecture(&self, params: Parameters<ArchitectureParams>) -> String {
        let p = params.0;
        let hub_limit = p.limit.unwrap_or(15);
        let _ = p.file_filter; // TODO: file filtering

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
                        "- {} ({}) — in:{} out:{} total:{} — {}\n",
                        h.name, h.node_type, h.in_degree, h.out_degree, h.total_degree, h.file_path
                    ));
                }
            }
            Err(e) => out.push_str(&format!("Error: {e}\n")),
        }

        out
    }

    /// Code quality analysis.
    #[tool(
        name = "codegraph_quality",
        description = "Code quality analysis — 'coupling' for file coupling, 'circular' for circular deps, 'hubs' for hub hotspots."
    )]
    async fn quality(&self, params: Parameters<QualityParams>) -> String {
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
                                "- {} — Ca:{} Ce:{} I:{:.2} ({} nodes)\n",
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
                                "- {} ({}) — degree:{} in:{} out:{} — {}\n",
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
