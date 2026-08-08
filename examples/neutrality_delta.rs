//! Runnable mirror of the refactor-neutrality facade call —
//! `open_store` → `structural_delta_for_paths`, printed as JSON — so the
//! structure-preservation evidence can be demonstrated and probed from a
//! shell without building a consumer. Shell-reproducible proof:
//!
//! ```sh
//! # 1. Index a project (the fingerprint pass runs as index step 5b):
//! codegraph index <root> --project-id demo --db-url surrealkv://…/graph.db
//! # 2. PURE RENAME a definition on disk — update its callers too — then
//! #    re-index (incremental; do NOT --force, that wipes history):
//! codegraph index <root> --project-id demo --db-url surrealkv://…/graph.db
//! cargo run --example neutrality_delta -- surrealkv://…/graph.db demo src/the/renamed_file.rs src/the/caller_file.rs
//! # → every symbol "preserved" (the renamed one carries renamed_from),
//! #   structure_neutral: true — the rename is PROVEN structure-neutral.
//! # 3. Add a call somewhere instead and re-index:
//! #   → the callee reports "changed", structure_neutral: false.
//! ```

use codegraph::facade;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(db_url), Some(project_id)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: neutrality_delta <db-url> <project-id> <path> [<path>…]");
    };
    let paths: Vec<String> = args.collect();
    if paths.is_empty() {
        anyhow::bail!("usage: neutrality_delta <db-url> <project-id> <path> [<path>…]");
    }

    let store = facade::open_store(&db_url, &project_id).await?;
    let delta = facade::structural_delta_for_paths(&store, &paths).await?;
    println!("{}", serde_json::to_string_pretty(&delta)?);
    eprintln!(
        "structure_neutral: {} ({} preserved, {} changed, {} added, {} removed)",
        delta.is_structure_neutral(),
        delta.preserved.len(),
        delta.changed.len(),
        delta.added.len(),
        delta.removed.len()
    );
    Ok(())
}
