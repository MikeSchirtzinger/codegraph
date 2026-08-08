//! Runnable mirror of brevity's structural-gate call — the exact facade
//! sequence `run_from_plan`'s Gate 1 seam performs (`open_store` →
//! `impact_verdict_for_paths`), printed as JSON. Exists so the gate's
//! behavior can be demonstrated and probed from a shell without building
//! brevity, e.g. the killtest-6 incomplete-rename scenario:
//!
//! ```sh
//! codegraph index <root> --project-id demo --db-url surrealkv://…/graph.db
//! # …rename a definition on disk, leave a caller stale, re-index…
//! cargo run --example gate_verdict -- surrealkv://…/graph.db demo src/the/renamed_file.rs
//! # → non-empty stale_references ⇒ the gate rejects
//! ```

use codegraph::facade;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(db_url), Some(project_id)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: gate_verdict <db-url> <project-id> <path> [<path>…]");
    };
    let paths: Vec<String> = args.collect();
    if paths.is_empty() {
        anyhow::bail!("usage: gate_verdict <db-url> <project-id> <path> [<path>…]");
    }

    let store = facade::open_store(&db_url, &project_id).await?;
    let verdict = facade::impact_verdict_for_paths(&store, &paths).await?;
    println!("{}", serde_json::to_string_pretty(&verdict)?);
    Ok(())
}
