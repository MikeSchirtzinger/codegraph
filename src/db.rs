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

    let client = match surrealdb::engine::any::connect(&url).await {
        Ok(client) => client,
        Err(e) => return Err(explain_connect_failure(&url, e)),
    };

    // Only a server has credentials to present. An embedded engine has no
    // auth layer at all, so signing in to one cannot succeed and its failure
    // says nothing: it was logged as a warning on every single command run
    // against the default store, which is the loudest thing on a first run
    // and teaches a user that codegraph's warnings are noise. Not attempting
    // it is the correct behavior, not a suppressed message. A real signin
    // failure against a real server is still reported, and is the only case
    // where the warning means something.
    if !is_embedded(&url) {
        let user = std::env::var("SURREALDB_USER").unwrap_or_else(|_| "root".to_string());
        let pass = std::env::var("SURREALDB_PASS").unwrap_or_else(|_| "root".to_string());
        if let Err(e) = client
            .signin(Root {
                username: user,
                password: pass,
            })
            .await
        {
            tracing::warn!("SurrealDB signin failed ({e}), proceeding without auth");
        }
    }

    client
        .use_ns(NS)
        .use_db(DB)
        .await
        .with_context(|| format!("Failed to select ns={NS} db={DB}"))?;

    tracing::info!("Connected to SurrealDB at {url} (ns={NS}, db={DB})");
    Ok(Arc::new(client))
}

/// True when this url names an engine that runs in this process rather than
/// a server to connect out to.
///
/// Matched on the scheme, positively, rather than by excluding the remote
/// ones: an unrecognized scheme is treated as remote, so a new server
/// transport keeps getting its signin attempt and its warning. Only the two
/// engines that are known to have no auth layer skip it. `indxdb://` is a
/// browser engine that cannot occur here and is deliberately not listed.
pub fn is_embedded(url: &str) -> bool {
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or(url);
    matches!(scheme.to_ascii_lowercase().as_str(), "surrealkv" | "mem")
}

/// True when a connection failure is the embedded store's single-writer
/// lock, and not some other connection problem.
///
/// surrealkv holds one writer at a time and reports it through a `LOCK` file:
/// "Database at <path>/LOCK is already locked by another process". That
/// string is the driver's, and matching it is the only way to tell this case
/// apart from a genuinely unreachable store. The match is on the stable
/// middle of the sentence rather than the whole of it, so a reworded prefix
/// or a changed path rendering does not silently turn the diagnosis back off.
///
/// The cost of the match going stale is that the raw driver string comes
/// back, which is what shipped before this function existed. It never
/// produces a wrong diagnosis, only an absent one.
pub fn is_store_locked(message: &str) -> bool {
    message.contains("is already locked")
}

/// Turn a connection failure into something a user can act on.
///
/// The lock case is the one worth naming: two codegraph processes against
/// one embedded store is a normal thing to do by accident (an MCP `serve`
/// left running in another terminal is the usual cause), and the driver's
/// own sentence names a `LOCK` file the user never created and cannot
/// usefully delete.
fn explain_connect_failure(url: &str, error: surrealdb::Error) -> anyhow::Error {
    let raw = error.to_string();
    if is_store_locked(&raw) {
        return anyhow::Error::new(error).context(format!(
            "the code graph store at {url} is open in another process, and it allows one writer at a time.\n\
             Either close the other codegraph process (an MCP \"codegraph serve\" left running is the usual one), \
             or point this command at a different store with --db-url."
        ));
    }
    anyhow::Error::new(error).context(format!("Failed to connect to SurrealDB at {url}"))
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
