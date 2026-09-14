use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
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

/// What a schema application actually did.
///
/// Returned rather than logged so a test can assert the skip happened
/// without timing anything: a wall-clock assertion on DDL that got faster
/// is a flaky test, and an assertion that no DDL ran is the real claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaInit {
    /// The DDL ran, and the store now records this binary's version of it.
    Applied,
    /// The store already recorded this binary's version, so no DDL ran.
    Skipped,
}

/// Where a store records which schema documents it carries.
///
/// Deliberately never `DEFINE`d anywhere. It has to be readable before any
/// DDL has ever run against a brand new store, and an undefined, schemaless
/// table is exactly that: readable, empty, and needing no bootstrap of its
/// own. Defining it in `schema.surql` would put the version record behind
/// the very DDL it exists to skip.
const SCHEMA_META_TABLE: &str = "schema_meta";

/// The version of a schema document, as recorded on a store.
///
/// The document's own SHA-256, not a hand-maintained integer. Editing a
/// `.surql` file is then the bump, with no second place to remember: a
/// counter someone has to increment by hand is a counter that eventually is
/// not incremented, and that failure mode is a store silently running the
/// previous release's DDL forever.
fn schema_version(source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hex::encode(hasher.finalize())
}

/// Read the version of one schema document this store already carries.
///
/// `None` covers three states that all want the same answer, apply the DDL:
/// a store that has never had this document applied, a store written by a
/// build that predates versioning, and a store whose read failed. The last
/// one is folded in deliberately rather than propagated. A fresh store has
/// no `schema_meta` table at all and this engine errors on a read of an
/// undefined table rather than returning nothing (the same behaviour
/// `doctor`'s schema probe relies on), so an error here is the ordinary
/// first-run case. Nothing is swallowed by doing so: the only consequence
/// of a wrong `None` is that the DDL runs, and a store that is genuinely
/// broken fails loudly on that DDL a few lines below.
async fn stored_schema_version(db: &Surreal<Any>, key: &str) -> Option<String> {
    let mut response = db
        .query(format!(
            "SELECT VALUE version FROM type::record('{SCHEMA_META_TABLE}', $key)"
        ))
        .bind(("key", key.to_string()))
        .await
        .ok()?;
    let versions: Vec<String> = response.take(0).ok()?;
    versions.into_iter().next()
}

/// Apply one schema document, unless the store already carries this exact
/// version of it.
///
/// Every `codegraph` invocation that opens a store used to re-execute all 72
/// idempotent `DEFINE` statements in `schema.surql`, or all 47 in
/// `schema_plan.surql`, against a schema that had not changed. A flat tax
/// per invocation, measured in `specs/receipts/index-profile-20260914.md`
/// section 3.1 and root-caused in
/// `specs/receipts/store-cost-20260914.md`, where it also turned out to be
/// the whole of the query-side regression RF-3 had left open since July.
/// One record read replaces it when nothing changed, and the DDL still runs
/// in full the first time, after an upgrade, and after any edit to the
/// document.
pub async fn apply_schema(db: &Surreal<Any>, key: &str, source: &str) -> Result<SchemaInit> {
    let wanted = schema_version(source);

    if stored_schema_version(db, key).await.as_deref() == Some(wanted.as_str()) {
        tracing::debug!("Schema {key} already at version {wanted}, skipping DDL");
        return Ok(SchemaInit::Skipped);
    }

    db.query(source)
        .await
        .with_context(|| format!("{key} schema DDL execution failed"))?
        .check()
        .with_context(|| format!("{key} schema DDL validation failed"))?;

    // Recorded only after `check` passed. A half-applied schema that claimed
    // to be current would skip its own repair on the next run.
    db.query(format!(
        "UPSERT type::record('{SCHEMA_META_TABLE}', $key) \
         SET key = $key, version = $version, updated_at = time::now()"
    ))
    .bind(("key", key.to_string()))
    .bind(("version", wanted.clone()))
    .await
    .with_context(|| format!("recording the {key} schema version failed"))?
    .check()
    .with_context(|| format!("recording the {key} schema version was rejected"))?;

    tracing::info!("Schema {key} applied (version {wanted})");
    Ok(SchemaInit::Applied)
}

/// Run the core graph schema DDL, unless the store already carries it.
pub async fn init_schema(db: &Surreal<Any>) -> Result<SchemaInit> {
    apply_schema(db, "core", include_str!("schema.surql")).await
}

