use anyhow::{Context, Result};
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;
use surrealdb::Surreal;

const NS: &str = "codegraph";
const DB: &str = "codegraph";

/// Default embedded SurrealDB location — a surrealkv file under the current
/// working directory. No server required. Pass `--db-url ws://...` (or set
/// SURREALDB_URL) to use a remote SurrealDB instance instead.
const DEFAULT_EMBEDDED_URL: &str = "surrealkv://.codegraph/graph.db";

/// Connect to SurrealDB and select the codegraph namespace/database.
///
/// Resolution order: explicit `url` argument > `SURREALDB_URL` env var >
/// embedded surrealkv file at `.codegraph/graph.db`. `surrealdb::engine::any::connect`
/// already dispatches on URL scheme, so both `surrealkv://` (embedded) and
/// `ws://` / `wss://` / `http://` / `https://` (remote) work transparently.
pub async fn connect(url: Option<&str>) -> Result<Arc<Surreal<Any>>> {
    let url = url.map(|s| s.to_string()).unwrap_or_else(|| {
        std::env::var("SURREALDB_URL").unwrap_or_else(|_| DEFAULT_EMBEDDED_URL.to_string())
    });

    let user = std::env::var("SURREALDB_USER").unwrap_or_else(|_| "root".to_string());
    let pass = std::env::var("SURREALDB_PASS").unwrap_or_else(|_| "root".to_string());

    let client = surrealdb::engine::any::connect(&url)
        .await
        .with_context(|| format!("Failed to connect to SurrealDB at {url}"))?;

    // Embedded engines (surrealkv://, mem://) don't have auth — signin fails
    // there, which is expected. Warn and proceed rather than hard-failing.
    if let Err(e) = client
        .signin(Root {
            username: user,
            password: pass,
        })
        .await
    {
        tracing::warn!("SurrealDB signin failed ({e}) — proceeding without auth");
    }

    client
        .use_ns(NS)
        .use_db(DB)
        .await
        .with_context(|| format!("Failed to select ns={NS} db={DB}"))?;

    tracing::info!("Connected to SurrealDB at {url} (ns={NS}, db={DB})");
    Ok(Arc::new(client))
}

/// Run idempotent schema DDL from the embedded schema.surql file.
pub async fn init_schema(db: &Surreal<Any>) -> Result<()> {
    let schema = include_str!("schema.surql");

    // Execute the schema as a single multi-statement query.
    // SurrealDB handles semicolon-delimited statements natively.
    db.query(schema)
        .await
        .context("Schema DDL execution failed")?
        .check()
        .context("Schema DDL validation failed")?;

    tracing::info!("Schema initialized (idempotent)");
    Ok(())
}
