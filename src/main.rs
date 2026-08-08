mod canon;
mod cli;
mod context;
mod db;
mod graph;
mod index;
mod mcp;
mod stats;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        // Logs go to stderr so they don't contaminate JSON / machine-readable
        // payloads on stdout.
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if cli.verbose {
                    "codegraph=debug".into()
                } else {
                    "codegraph=info".into()
                }
            }),
        )
        .init();

    match cli.command {
        Commands::Index {
            path,
            project_id,
            tier,
            languages,
            force,
            db_url,
        } => {
            let client = db::connect(db_url.as_deref()).await?;
            db::init_schema(&client).await?;

            let tier: index::IndexingTier = tier.parse()?;

            let config = index::IndexConfig {
                project_id: project_id.clone(),
                root_path: path,
                tier,
                languages,
                force,
            };

            let result = index::index_project(&client, &config).await?;

            println!("\n=== Indexing Complete ===");
            println!("  Project:     {project_id}");
            println!(
                "  Files:       {} scanned, {} indexed, {} unchanged, {} skipped",
                result.files_scanned,
                result.files_indexed,
                result.files_unchanged,
                result.files_skipped
            );
            println!("  Nodes:       {}", result.nodes_created);
            println!("  Edges:       {}", result.edges_created);
            println!("\n=== Resolution (R1) ===");
            let r = &result.resolution;
            let pct = |n: usize| {
                if r.edges_considered == 0 {
                    0.0
                } else {
                    100.0 * n as f64 / r.edges_considered as f64
                }
            };
            println!("  Edges:       {} name-edges considered", r.edges_considered);
            println!("  Resolved:    {} ({:.1}%)", r.resolved, pct(r.resolved));
            println!("  Ambiguous:   {} ({:.1}%)", r.ambiguous, pct(r.ambiguous));
            println!("  Unresolved:  {} ({:.1}%)", r.unresolved, pct(r.unresolved));
            println!("  File refs:   {}", r.file_refs);
            let f = &result.fingerprints;
            println!("\n=== Fingerprints ===");
            println!(
                "  Symbols:     {} fingerprinted (generation {})",
                f.symbols, f.generation
            );
            println!(
                "  Delta:       {} changed, {} added, {} removed vs previous generation",
                f.changed, f.added, f.removed
            );
            if !result.errors.is_empty() {
                println!("  Errors:      {}", result.errors.len());
                for e in &result.errors {
                    println!("    - {e}");
                }
            }
        }

        Commands::Resolve { project_id, db_url } => {
            let client = db::connect(db_url.as_deref()).await?;
            db::init_schema(&client).await?;

            let stats = index::resolve::resolve_project(&client, &project_id).await?;

            println!("\n=== Resolution Complete ===");
            println!("  Project:     {project_id}");
            let pct = |n: usize| {
                if stats.edges_considered == 0 {
                    0.0
                } else {
                    100.0 * n as f64 / stats.edges_considered as f64
                }
            };
            println!("  Edges:       {} considered", stats.edges_considered);
            println!("  Resolved:    {} ({:.1}%)", stats.resolved, pct(stats.resolved));
            println!("  Ambiguous:   {} ({:.1}%)", stats.ambiguous, pct(stats.ambiguous));
            println!("  Unresolved:  {} ({:.1}%)", stats.unresolved, pct(stats.unresolved));
            println!("  File refs:   {}", stats.file_refs);
        }

        Commands::Context {
            output,
            project_id,
            db_url,
        } => {
            let client = db::connect(db_url.as_deref()).await?;
            context::generate_context(&client, &project_id, &output).await?;
        }

        Commands::Stats { project_id, db_url } => {
            let client = db::connect(db_url.as_deref()).await?;
            stats::print_stats(&client, &project_id).await?;
        }

        Commands::Serve { project_id, db_url } => {
            let client = db::connect(db_url.as_deref()).await?;
            mcp::server::serve_stdio(client, project_id).await?;
        }

        Commands::Clones {
            project_id,
            min_edges,
            json,
            db_url,
        } => {
            let client = db::connect(db_url.as_deref()).await?;
            // Idempotent DDL, same as Query — a store that was never
            // indexed answers with zero groups instead of a table error.
            db::init_schema(&client).await?;

            let groups = graph::clones::find_clone_groups(&client, &project_id, min_edges).await?;
            if json {
                println!("{}", serde_json::to_string(&groups)?);
            } else {
                let symbols: usize = groups.iter().map(|g| g.members.len()).sum();
                println!(
                    "=== Structural Clones ({} groups, {} symbols, --min-edges {min_edges}) ===\n",
                    groups.len(),
                    symbols
                );
                for g in &groups {
                    println!(
                        "Group of {} — fingerprint {} ({}-edge neighborhood):",
                        g.members.len(),
                        g.fingerprint,
                        g.edge_count
                    );
                    for m in &g.members {
                        println!(
                            "  {} ({}) — {}:{}",
                            m.qualified_name,
                            m.node_type,
                            m.file_path,
                            m.start_line.unwrap_or(0)
                        );
                    }
                    println!();
                }
            }
        }

        Commands::Query {
            project_id,
            kind,
            name,
            limit,
            depth,
            include_ambiguous,
            json,
            db_url,
        } => {
            let client = db::connect(db_url.as_deref()).await?;
            // Idempotent DDL, same as Index/Resolve run on connect — without
            // it, `query` against a store that was created but never
            // indexed (SCHEMAFULL `code_node` undefined) hard-errors on
            // "table does not exist" instead of the graceful "zero results" /
            // `graph_empty` a caller (the `--json` path especially) expects.
            db::init_schema(&client).await?;

            match kind.as_str() {
                "summary" => {
                    let summary = graph::search::project_summary(&client, &project_id).await?;
                    if json {
                        println!("{}", serde_json::to_string(&summary)?);
                    } else {
                        println!("=== Project: {project_id} ===\n");
                        println!("Node types:");
                        for (nt, c) in &summary.node_types {
                            println!("  {nt:>15}: {c}");
                        }
                        println!("\nLanguages:");
                        for (l, c) in &summary.languages {
                            println!("  {l:>15}: {c}");
                        }
                        println!("\nEdge types:");
                        for (et, c) in &summary.edge_types {
                            println!("  {et:>15}: {c}");
                        }
                    }
                }
                "hubs" => {
                    let hubs =
                        graph::hub_nodes::find_hub_nodes(&client, &project_id, limit).await?;
                    if json {
                        println!("{}", serde_json::to_string(&hubs)?);
                    } else {
                        println!("=== Hub Nodes (top {limit}) ===\n");
                        for h in &hubs {
                            println!(
                                "  {} ({}) — in:{} out:{} total:{} — {}",
                                h.name,
                                h.node_type,
                                h.in_degree,
                                h.out_degree,
                                h.total_degree,
                                h.file_path
                            );
                        }
                    }
                }
                "coupling" => {
                    let coupling =
                        graph::coupling::calculate_file_coupling(&client, &project_id, limit)
                            .await?;
                    if json {
                        println!("{}", serde_json::to_string(&coupling)?);
                    } else {
                        println!("=== File Coupling (top {limit}) ===\n");
                        println!("  {:>50}  Ca   Ce  I     Nodes", "File");
                        for c in &coupling {
                            println!(
                                "  {:>50}  {:>3}  {:>3}  {:.2}  {:>3}",
                                c.file_path, c.afferent, c.efferent, c.instability, c.node_count
                            );
                        }
                    }
                }
                "search" => {
                    let q = name.as_deref().unwrap_or("");
                    let results =
                        graph::search::search_nodes(&client, &project_id, q, None, limit).await?;
                    if json {
                        println!("{}", serde_json::to_string(&results)?);
                    } else {
                        println!("=== Search: '{q}' ({} results) ===\n", results.len());
                        for n in &results {
                            println!(
                                "  {} ({}) — {}:{}",
                                n.name,
                                n.node_type,
                                n.file_path,
                                n.start_line.unwrap_or(0)
                            );
                        }
                    }
                }
                "calls" => {
                    let fname = name.as_deref().unwrap_or("");
                    let result = graph::call_chain::trace_calls(
                        &client,
                        &project_id,
                        fname,
                        depth,
                        include_ambiguous,
                    )
                    .await?;
                    if json {
                        println!("{}", serde_json::to_string(&result)?);
                    } else {
                        println!("=== Call Chain from '{fname}' (depth {depth}) ===\n");
                        if result.groups.is_empty() {
                            println!("No function named '{fname}' found.");
                        } else {
                            if result.name_ambiguous {
                                println!(
                                    "'{fname}' matches {} distinct functions — showing each separately:\n",
                                    result.groups.len()
                                );
                            }
                            for g in &result.groups {
                                if result.name_ambiguous {
                                    println!("-- {} ({}) --", g.root_qualified_name, g.root_file);
                                }
                                for c in &g.entries {
                                    let indent = "  ".repeat(c.depth);
                                    println!(
                                        "{indent}{} → {} ({})",
                                        c.caller_name, c.callee_name, c.callee_file
                                    );
                                }
                                if include_ambiguous {
                                    for a in &g.ambiguous {
                                        let indent = "  ".repeat(a.depth);
                                        println!(
                                            "{indent}{} → [AMBIGUOUS] '{}' ({}) — candidates: {}",
                                            a.caller_name,
                                            a.to_name,
                                            a.to_type,
                                            a.candidates.join(", ")
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                "deps" => {
                    let sname = name.as_deref().unwrap_or("");
                    let result = graph::dependencies::get_dependencies(
                        &client,
                        &project_id,
                        sname,
                        &graph::NAME_EDGE_TYPES,
                        depth,
                        include_ambiguous,
                    )
                    .await?;
                    if json {
                        println!("{}", serde_json::to_string(&result)?);
                    } else {
                        print_dependency_result(&result, "Dependencies of", sname, depth, include_ambiguous);
                    }
                }
                "rdeps" => {
                    let sname = name.as_deref().unwrap_or("");
                    if json {
                        // Built via `codegraph::graph::dependencies` (the
                        // lib crate's copy, not main.rs's own private `mod
                        // graph;`) so the result is the same
                        // `DependencyResult` type `codegraph::facade`'s
                        // projection expects — reusing `client`, the
                        // connection already open above, rather than a
                        // second `facade::open_store`: a second connection
                        // to the same embedded surrealkv store while
                        // `client` is still held open deadlocks on its
                        // single-writer file lock (only `ws://` remote
                        // stores would tolerate it), and there's no reason
                        // to pay for a second connection at all when one is
                        // already open.
                        let dep_result = codegraph::graph::dependencies::get_reverse_dependencies(
                            &client,
                            &project_id,
                            sname,
                            &codegraph::graph::NAME_EDGE_TYPES,
                            depth,
                            include_ambiguous,
                        )
                        .await?;
                        let graph_empty = !codegraph::facade::is_indexed_conn(&client, &project_id)
                            .await
                            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                        let verdict = codegraph::facade::project_dependency_result(
                            codegraph::facade::VerdictTarget::Symbol(sname.to_string()),
                            sname,
                            &dep_result,
                            graph_empty,
                        );
                        println!("{}", serde_json::to_string(&verdict)?);
                    } else {
                        let result = graph::dependencies::get_reverse_dependencies(
                            &client,
                            &project_id,
                            sname,
                            &graph::NAME_EDGE_TYPES,
                            depth,
                            include_ambiguous,
                        )
                        .await?;
                        print_dependency_result(
                            &result,
                            "Reverse Dependencies of",
                            sname,
                            depth,
                            include_ambiguous,
                        );
                        if !result.stale_references.is_empty() {
                            let reason = if result.live_definitions == 0 {
                                "no live symbol currently has this name; check for an incomplete rename"
                            } else {
                                "this name still has live definitions; these references could not be bound to any of them"
                            };
                            println!(
                                "\n! {} unresolved reference(s) still named '{sname}' ({reason}):",
                                result.stale_references.len()
                            );
                            for u in &result.stale_references {
                                println!(
                                    "  {} ({}) → [UNRESOLVED] '{}' ({})",
                                    u.from_name, u.from_file, u.to_name, u.to_type
                                );
                            }
                        }
                    }
                }
                "circular" => {
                    let circular =
                        graph::circular::detect_circular_deps(&client, &project_id).await?;
                    if json {
                        println!("{}", serde_json::to_string(&circular)?);
                    } else {
                        println!("=== Circular Dependencies ({} found) ===\n", circular.len());
                        for c in &circular {
                            println!(
                                "  {} ↔ {} (a→b: {}, b→a: {}) via {}",
                                c.file_a,
                                c.file_b,
                                c.a_to_b_edges,
                                c.b_to_a_edges,
                                c.via.join("+")
                            );
                        }
                    }
                }
                other => {
                    anyhow::bail!("unknown query kind: {other} (expected: summary|hubs|coupling|search|calls|deps|rdeps|circular)");
                }
            }
        }
    }

    Ok(())
}

/// Shared printer for `deps`/`rdeps` — both return the same grouped
/// `DependencyResult` shape (R2: uniform consumption of the resolved
/// graph), so both render the same way: per-symbol groups (only labeled
/// when the queried name was itself ambiguous, D3), each with its resolved
/// items, its always-visible unresolved references, and (with
/// `--include-ambiguous`) its ambiguous ones.
fn print_dependency_result(
    result: &graph::dependencies::DependencyResult,
    verb: &str,
    name: &str,
    depth: usize,
    include_ambiguous: bool,
) {
    let total: usize = result.groups.iter().map(|g| g.items.len()).sum();
    println!("=== {verb} '{name}' (depth {depth}, {total} found) ===\n");
    if result.groups.is_empty() {
        println!("No symbol named '{name}' found.");
        return;
    }
    if result.name_ambiguous {
        println!(
            "'{name}' matches {} distinct symbols — showing each separately:\n",
            result.groups.len()
        );
    }
    for g in &result.groups {
        if result.name_ambiguous {
            println!("-- {} ({}) --", g.root_qualified_name, g.root_file);
        }
        for d in &g.items {
            let indent = "  ".repeat(d.depth);
            println!("{indent}{} ({}) — {}", d.name, d.node_type, d.file_path);
        }
        for u in &g.unresolved {
            let indent = "  ".repeat(u.depth);
            println!("{indent}{} → [UNRESOLVED] '{}' ({})", u.from_name, u.to_name, u.to_type);
        }
        if include_ambiguous {
            for a in &g.ambiguous {
                let indent = "  ".repeat(a.depth);
                println!(
                    "{indent}{} → [AMBIGUOUS] '{}' ({}) — candidates: {}",
                    a.from_name,
                    a.to_name,
                    a.to_type,
                    a.candidates.join(", ")
                );
            }
        }
    }
}
