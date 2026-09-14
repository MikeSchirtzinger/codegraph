//! RF-7: the schema DDL is applied once per store, not once per invocation.
//!
//! Every assertion here is on what `apply_schema` *did*, never on how long
//! it took. A wall-clock assertion on DDL that got faster is a flaky test
//! on a loaded machine, and "no DDL ran" is the actual claim anyway.

use anyhow::Result;
use codegraph::db::{self, SchemaInit};
use codegraph::plan::store;

/// A fresh, isolated, in-process store with no schema of any kind on it.
async fn empty_store() -> Result<std::sync::Arc<surrealdb::Surreal<surrealdb::engine::any::Any>>> {
    db::connect(Some("mem://")).await
}

#[tokio::test]
async fn first_open_applies_the_core_ddl_and_the_second_skips_it() -> Result<()> {
    let db = empty_store().await?;

    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Applied,
        "the first open of a fresh store has to run the DDL"
    );
    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Skipped,
        "a store already carrying this binary's schema must not re-run the DDL"
    );
    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Skipped,
        "and it must keep skipping, not alternate"
    );
    Ok(())
}

#[tokio::test]
async fn a_version_mismatch_re_runs_the_core_ddl() -> Result<()> {
    let db = empty_store().await?;
    assert_eq!(db::init_schema(&db).await?, SchemaInit::Applied);
    assert_eq!(db::init_schema(&db).await?, SchemaInit::Skipped);

    // What an older build's store looks like from here: a recorded version
    // that is not this binary's. Written through the same table the reader
    // reads, so the test exercises the real comparison rather than a mock
    // of it.
    db.query("UPSERT type::record('schema_meta', 'core') SET key = 'core', version = 'a-previous-release'")
        .await?
        .check()?;

    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Applied,
        "a store at a different schema version has to re-run the DDL"
    );
    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Skipped,
        "and the re-run has to record the new version, or every run re-applies"
    );
    Ok(())
}

#[tokio::test]
async fn a_missing_version_record_re_runs_the_core_ddl() -> Result<()> {
    let db = empty_store().await?;
    assert_eq!(db::init_schema(&db).await?, SchemaInit::Applied);
    assert_eq!(db::init_schema(&db).await?, SchemaInit::Skipped);

    // A store written by any build that predates versioning carries the
    // tables but no version record at all. It must not be mistaken for a
    // current one.
    db.query("DELETE schema_meta").await?.check()?;

    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Applied,
        "a store with the tables but no recorded version has to re-run the DDL"
    );
    Ok(())
}

#[tokio::test]
async fn the_plan_schema_is_versioned_separately_from_the_core_schema() -> Result<()> {
    let db = empty_store().await?;

    assert_eq!(db::init_schema(&db).await?, SchemaInit::Applied);
    assert_eq!(
        store::ensure_schema(&db).await?,
        SchemaInit::Applied,
        "the core schema's version must not make the plan schema look present"
    );
    assert_eq!(store::ensure_schema(&db).await?, SchemaInit::Skipped);
    assert_eq!(
        db::init_schema(&db).await?,
        SchemaInit::Skipped,
        "and applying the plan schema must not disturb the core one"
    );
    Ok(())
}

#[tokio::test]
async fn a_skipped_store_still_answers_queries_against_every_defined_table() -> Result<()> {
    let db = empty_store().await?;
    assert_eq!(db::init_schema(&db).await?, SchemaInit::Applied);
    assert_eq!(store::ensure_schema(&db).await?, SchemaInit::Applied);

    // The skip is only correct if the tables it skipped defining are really
    // there. Reading each one is the check: this engine errors on a read of
    // an undefined table rather than returning nothing, which is the same
    // property `doctor`'s schema probe relies on.
    assert_eq!(db::init_schema(&db).await?, SchemaInit::Skipped);
    assert_eq!(store::ensure_schema(&db).await?, SchemaInit::Skipped);

    for table in [
        "code_node",
        "code_edge",
        "file_metadata",
        "fingerprint",
        "project_registry",
        "work_plane",
        "work_item",
        "work_touch",
    ] {
        db.query(format!("SELECT * FROM {table} LIMIT 1"))
            .await
            .unwrap_or_else(|e| panic!("{table} is not queryable after a skipped open: {e}"))
            .check()
            .unwrap_or_else(|e| panic!("{table} is not queryable after a skipped open: {e}"));
    }
    Ok(())
}
