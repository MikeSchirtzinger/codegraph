//! Produce and verify evidence chains for one symbol against a real index
//! (`specs/explain-v1.md`).
//!
//! This is the library API the `--explain` CLI flag wraps, exercised
//! directly, and it is also the shape of the replay block a client runs on
//! their own machines: build the chains, then hand them straight back to the
//! verifier and print what it says. A chain that does not verify is printed
//! with its failing steps rather than dropped, because a verifier whose
//! failures are invisible is not a verifier.
//!
//! ```text
//! cargo run --release --example explain_chain -- \
//!     --db-url surrealkv:///path/graph.db --project-id cg-explain --symbol helper
//! ```

use codegraph::graph::explain::{self, ExplainGraph, FindingKind, Membership};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut db_url = String::new();
    let mut project_id = String::new();
    let mut symbol = String::new();
    let mut only: Option<String> = None;
    let mut limit = usize::MAX;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--db-url" => {
                db_url = value;
                i += 2;
            }
            "--project-id" => {
                project_id = value;
                i += 2;
            }
            "--symbol" => {
                symbol = value;
                i += 2;
            }
            "--only" => {
                only = args.get(i + 1).cloned();
                i += 2;
            }
            "--limit" => {
                limit = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                i += 2;
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    if db_url.is_empty() || project_id.is_empty() || symbol.is_empty() {
        anyhow::bail!("--db-url, --project-id and --symbol are all required");
    }

    let db = surrealdb::engine::any::connect(db_url.as_str()).await?;
    db.use_ns("codegraph").use_db("codegraph").await?;

    let graph = ExplainGraph::load(&db, &project_id).await?;
    let chains = explain::explain_symbol(&graph, &project_id, &symbol, &Membership::ExplicitSymbol);

    let wanted = |kind: FindingKind| match only.as_deref() {
        None => true,
        Some(tag) => kind.as_tag() == tag,
    };

    let mut shown = 0usize;
    let mut verified = 0usize;
    let mut failed = 0usize;

    for chain in chains.iter().filter(|c| wanted(c.finding)) {
        let verdict = explain::verify_chain(chain, &graph);
        if verdict.ok {
            verified += 1;
        } else {
            failed += 1;
        }
        if shown < limit {
            shown += 1;
            print!("{}", chain.render());
            println!(
                "  verify: {} ({} step(s) checked)",
                if verdict.ok { "PASS" } else { "FAIL" },
                verdict.steps.len()
            );
            for f in verdict.failures() {
                println!("    step {} ({}): {}", f.index, f.fact, f.reason.as_deref().unwrap_or(""));
            }
            println!();
        }
    }

    println!("{symbol}: {verified} chain(s) verified, {failed} failed");
    if failed > 0 {
        anyhow::bail!("{failed} chain(s) did not verify");
    }
    Ok(())
}
