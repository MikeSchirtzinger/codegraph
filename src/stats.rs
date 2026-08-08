use anyhow::{Context, Result};
use serde::Deserialize;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use surrealdb_types::SurrealValue;

#[derive(Debug, Deserialize, SurrealValue, Default)]
struct CountResult {
    count: i64,
}

#[derive(Debug, Deserialize, SurrealValue)]
struct TypeCount {
    node_type: String,
    count: i64,
}

#[derive(Debug, Deserialize, SurrealValue)]
struct LangCount {
    language: String,
    count: i64,
}

#[derive(Debug, Deserialize, SurrealValue)]
struct RegistryRow {
    name: String,
    root_path: String,
    node_count: Option<i64>,
    edge_count: Option<i64>,
    file_count: Option<i64>,
    last_indexed_at: Option<String>,
}

/// Print summary statistics for a project's codegraph.
pub async fn print_stats(db: &Surreal<Any>, project_id: &str) -> Result<()> {
    let pid = project_id.to_string();

    println!("Codegraph Statistics — project '{project_id}'");
    println!("============================================");

    // Project registry row (written at the end of every `index` run).
    let mut response = db
        .query("SELECT name, root_path, node_count, edge_count, file_count, last_indexed_at FROM project_registry WHERE project_id = $pid;")
        .bind(("pid", pid.clone()))
        .await
        .context("project_registry query failed")?;
    let registry: Vec<RegistryRow> = response.take(0).unwrap_or_default();
    if let Some(r) = registry.first() {
        println!("  Root:          {}", r.root_path);
        println!("  Nodes:         {}", r.node_count.unwrap_or(0));
        println!("  Edges:         {}", r.edge_count.unwrap_or(0));
        println!("  Files scanned: {}", r.file_count.unwrap_or(0));
        if let Some(ts) = &r.last_indexed_at {
            println!("  Last indexed:  {ts}");
        }
    } else {
        println!("  (no project_registry entry — run `codegraph index` first)");
    }
    println!();

    // Live counts straight off code_node / code_edge / file_metadata, in case
    // the registry snapshot is stale relative to the actual table contents.
    let node_count = count_where(db, "code_node", &pid).await?;
    let edge_count = count_where(db, "code_edge", &pid).await?;
    let file_count = count_where(db, "file_metadata", &pid).await?;

    println!("Live Table Counts");
    println!("-----------------");
    println!("  code_node:      {node_count}");
    println!("  code_edge:      {edge_count}");
    println!("  file_metadata:  {file_count}");
    println!();

    // Node type distribution
    let mut response = db
        .query("SELECT node_type, count() AS count FROM code_node WHERE project_id = $pid GROUP BY node_type ORDER BY count DESC;")
        .bind(("pid", pid.clone()))
        .await
        .context("node_type distribution query failed")?;
    let types: Vec<TypeCount> = response.take(0).unwrap_or_default();
    if !types.is_empty() {
        println!("Node Type Distribution");
        println!("-----------------------");
        for t in &types {
            println!("  {:<15} {}", t.node_type, t.count);
        }
        println!();
    }

    // Language distribution
    let mut response = db
        .query("SELECT language, count() AS count FROM code_node WHERE project_id = $pid GROUP BY language ORDER BY count DESC;")
        .bind(("pid", pid))
        .await
        .context("language distribution query failed")?;
    let langs: Vec<LangCount> = response.take(0).unwrap_or_default();
    if !langs.is_empty() {
        println!("Language Distribution");
        println!("----------------------");
        for l in &langs {
            println!("  {:<15} {}", l.language, l.count);
        }
    }

    Ok(())
}

async fn count_where(db: &Surreal<Any>, table: &str, project_id: &str) -> Result<i64> {
    let query = format!("SELECT count() AS count FROM {table} WHERE project_id = $pid GROUP ALL;");
    let mut response = db
        .query(&query)
        .bind(("pid", project_id.to_string()))
        .await
        .context("Count query failed")?;
    let result: Option<CountResult> = response.take(0).unwrap_or(None);
    Ok(result.map(|r| r.count).unwrap_or(0))
}
