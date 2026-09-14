//! The `codegraph` binary: argument parsing, dispatch, and printing.
//!
//! It declares no modules of its own. Everything it calls lives in the
//! library crate and is reached through `use codegraph::…`, so the module
//! tree is compiled exactly once and `crate::` means the library in every
//! file that is part of it. See `src/lib.rs` for what went wrong when this
//! file re-declared the tree instead.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use codegraph::cli::{Cli, Commands, PlanCmd};
use codegraph::plan::ops;
use codegraph::{config, context, db, graph, index, mcp, stats};

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
            let root = config::repo_root(&path);
            let project_id = config::project_id(project_id.as_deref(), Some(&path))?;
            let client = db::connect(Some(&config::db_url(db_url.as_deref(), &root)?)).await?;
            db::init_schema(&client).await?;

            let tier: index::IndexingTier = config::tier(tier.as_deref(), &root)?.parse()?;

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
            println!("  Tier:        {}", tier.as_str());
            println!("  Discovery:   {}", result.discovery.describe());
            println!("  Elapsed:     {:.1}s", result.elapsed.as_secs_f64());
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
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
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
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            context::generate_context(&client, &project_id, &output).await?;
        }

        Commands::Stats { project_id, db_url } => {
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            stats::print_stats(&client, &project_id).await?;
        }

        Commands::Serve { project_id, db_url } => {
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            mcp::server::serve_stdio(client, project_id).await?;
        }

        Commands::Clones {
            project_id,
            min_edges,
            json,
            db_url,
        } => {
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            // Idempotent DDL, same as Query. A store that was never
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
                        "Group of {}, fingerprint {} ({}-edge neighborhood):",
                        g.members.len(),
                        g.fingerprint,
                        g.edge_count
                    );
                    for m in &g.members {
                        println!(
                            "  {} ({}), {}:{}",
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
            explain,
            json,
            db_url,
        } => {
            // Four query kinds take a name and have no meaning without one.
            // Leaving `--name` optional for them meant a forgotten flag
            // exited 0 with "No function named '' found", which reads as a
            // fact about the codebase rather than a fact about the command
            // line. Refuse before opening the store.
            let name = require_name(&kind, name)?;
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            // Idempotent DDL, same as Index/Resolve run on connect. Without
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
                                "  {} ({}): in:{} out:{} total:{}, {}",
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
                                "  {} ({}), {}:{}",
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
                                    "'{fname}' matches {} distinct functions, showing each separately:\n",
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
                                            "{indent}{} → [AMBIGUOUS] '{}' ({}), candidates: {}",
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
                        // projection expects, reusing `client`, the
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
                        let mut verdict = codegraph::facade::project_dependency_result(
                            codegraph::facade::VerdictTarget::Symbol(sname.to_string()),
                            sname,
                            &dep_result,
                            graph_empty,
                        );
                        // `explanations` stays absent unless asked for, so
                        // the bytes of a plain verdict are what they were
                        // before the field existed.
                        if explain {
                            verdict.explanations = Some(
                                codegraph::facade::explain_chains_for_symbol(
                                    &client,
                                    &project_id,
                                    sname,
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                            );
                        }
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
                        if explain {
                            let chains = codegraph::facade::explain_chains_for_symbol(
                                &client,
                                &project_id,
                                sname,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                            println!("\n=== Evidence ({} chain(s)) ===\n", chains.len());
                            for chain in &chains {
                                print!("{}", chain.render());
                                println!();
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

        Commands::Init {
            path,
            project_id,
            force,
            json,
        } => {
            let start = match path {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let root = config::repo_root(&start);
            let project_id = config::project_id(project_id.as_deref(), Some(root.as_path()))?;
            let options = codegraph::init::InitOptions {
                root,
                project_id,
                force,
            };
            emit(&codegraph::init::run(&options)?, json)?;
        }

        Commands::Doctor {
            project_id,
            db_url,
            json,
        } => {
            let root = config::repo_root(&std::env::current_dir()?);
            let resolved =
                config::resolve_project_id(project_id.as_deref(), Some(root.as_path()))?;
            let options = codegraph::doctor::DoctorOptions {
                project_id: resolved.id,
                project_id_source: resolved.source.describe(),
                root,
                db_url,
            };
            let report = codegraph::doctor::run(&options).await?;
            emit(&report, json)?;
            // A doctor that found something broken has to say so in the exit
            // code too, or no script can act on it. A warning is still a
            // healthy exit.
            if !report.is_healthy() {
                std::io::stdout().flush().ok();
                std::process::exit(1);
            }
        }

        Commands::Plan {
            cmd,
            project_id,
            db_url,
            planes_file,
            json,
        } => {
            let root = config::repo_root(&std::env::current_dir()?);
            let planes_path: PathBuf =
                planes_file.unwrap_or_else(|| codegraph::plan::default_planes_path(&root));

            // `lint` reads the planes file and nothing else, so it runs
            // before any connection is opened. That is what lets it work on
            // a repo that has never been indexed, which is exactly when a
            // malformed planes file is most likely.
            if matches!(cmd, PlanCmd::Lint) {
                emit(&ops::lint(&planes_path)?, json)?;
                return Ok(());
            }

            let project_id = config::project_id(project_id.as_deref(), Some(root.as_path()))?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            db::init_schema(&client).await?;

            match cmd {
                PlanCmd::Sync => {
                    emit(&ops::sync(&client, &project_id, &planes_path).await?, json)?
                }
                PlanCmd::List {
                    plane,
                    status,
                    horizon,
                } => {
                    let filter = ops::ListFilter {
                        plane,
                        status,
                        horizon,
                    };
                    emit(&ops::list(&client, &project_id, &filter).await?, json)?
                }
                PlanCmd::Show { id } => emit(&ops::show(&client, &project_id, &id).await?, json)?,
                PlanCmd::Touching { target } => {
                    emit(&ops::touching(&client, &project_id, &target).await?, json)?
                }
                PlanCmd::Collisions => emit(&ops::collisions(&client, &project_id).await?, json)?,
                PlanCmd::Stale => emit(&ops::stale(&client, &project_id).await?, json)?,
                PlanCmd::Blast { id, depth } => {
                    emit(&ops::blast(&client, &project_id, &id, depth).await?, json)?
                }
                PlanCmd::Brief => {
                    let markdown = codegraph::landscape::brief(&client, &project_id).await?;
                    if json {
                        println!("{}", serde_json::to_string(&markdown)?);
                    } else {
                        print!("{markdown}");
                    }
                }
                PlanCmd::Lint => unreachable!("lint runs above, before the store is opened"),
            }
        }

        Commands::Landscape {
            project_id,
            format,
            output,
            db_url,
        } => {
            let project_id = config::project_id(project_id.as_deref(), None)?;
            let client = db::connect(Some(&store_url(db_url.as_deref())?)).await?;
            db::init_schema(&client).await?;

            let options = codegraph::landscape::LandscapeOptions {
                project_id,
                format,
                output,
            };
            let rendered = codegraph::landscape::run(&client, &options).await?;
            match &rendered.written_to {
                Some(path) => println!("Landscape written to {}", path.display()),
                None => print!("{}", rendered.rendered),
            }
        }
    }

    Ok(())
}

/// The store url for a command run from the current directory.
///
/// Anchored at the repo root by [`config::db_url`], so `codegraph query`
/// typed inside `src/` reads the same store `codegraph index` typed at the
/// root wrote, instead of silently creating an empty second one.
fn store_url(explicit: Option<&str>) -> Result<String> {
    let root = config::repo_root(&std::env::current_dir()?);
    config::db_url(explicit, &root)
}

/// Query kinds whose whole meaning is the name they are given.
const NAME_REQUIRED_KINDS: [&str; 4] = ["calls", "deps", "rdeps", "search"];

/// Reject a name-taking query kind that was given no name.
///
/// The old behavior was worse than an error: an empty name flowed into the
/// lookup, matched nothing, and printed "No function named '' found" with a
/// zero exit code. A caller reading that learns something false about their
/// codebase. The message names the flag and shows the command, because the
/// user already knows what they meant.
fn require_name(kind: &str, name: Option<String>) -> Result<Option<String>> {
    if !NAME_REQUIRED_KINDS.contains(&kind) {
        return Ok(name);
    }
    match name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => Ok(name),
        _ => anyhow::bail!(
            "--kind {kind} needs a symbol to work on. Pass one with --name:\n  codegraph query --kind {kind} --name <symbol>"
        ),
    }
}

/// Print one command result: machine-readable JSON on `--json`, the value's
/// own human rendering otherwise.
///
/// Every result type the new subcommands return carries both a `Serialize`
/// and a `Display` impl, and this is the only thing `main.rs` ever does with
/// them. That is deliberate: `src/main.rs` is owned by one lane and the
/// modules it dispatches into are owned by others, so the dispatch never
/// reads a field it does not own.
fn emit<T: serde::Serialize + std::fmt::Display>(value: &T, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        print!("{value}");
    }
    Ok(())
}

/// Shared printer for `deps`/`rdeps`. Both return the same grouped
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
            "'{name}' matches {} distinct symbols, showing each separately:\n",
            result.groups.len()
        );
    }
    for g in &result.groups {
        if result.name_ambiguous {
            println!("-- {} ({}) --", g.root_qualified_name, g.root_file);
        }
        for d in &g.items {
            let indent = "  ".repeat(d.depth);
            println!("{indent}{} ({}), {}", d.name, d.node_type, d.file_path);
        }
        for u in &g.unresolved {
            let indent = "  ".repeat(u.depth);
            println!("{indent}{} → [UNRESOLVED] '{}' ({})", u.from_name, u.to_name, u.to_type);
        }
        if include_ambiguous {
            for a in &g.ambiguous {
                let indent = "  ".repeat(a.depth);
                println!(
                    "{indent}{} → [AMBIGUOUS] '{}' ({}), candidates: {}",
                    a.from_name,
                    a.to_name,
                    a.to_type,
                    a.candidates.join(", ")
                );
            }
        }
    }
}
