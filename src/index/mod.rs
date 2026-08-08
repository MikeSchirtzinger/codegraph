//! Generic multi-language code indexer.
//!
//! Parses arbitrary codebases via tree-sitter and populates SurrealDB with
//! `code_node` + `code_edge` records, isolated by `project_id`.

pub mod extractors;
pub mod fingerprint;
pub mod incremental;
pub mod node_id;
pub mod parser;
pub mod qualified_name;
pub mod resolve;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use surrealdb_types::SurrealValue;

use crate::index::incremental::FileChange;
use crate::index::parser::ParsedFile;

/// Controls which edge types are extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingTier {
    /// Functions, classes, imports only. Fastest.
    Fast,
    /// + call edges.
    Balanced,
    /// + references, type annotations, all edges. Slowest but most complete.
    Full,
}

impl std::str::FromStr for IndexingTier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "full" => Ok(Self::Full),
            _ => anyhow::bail!("unknown indexing tier: {s} (expected fast|balanced|full)"),
        }
    }
}

/// Configuration for a generic indexing run.
pub struct IndexConfig {
    pub project_id: String,
    pub root_path: std::path::PathBuf,
    pub tier: IndexingTier,
    pub languages: Option<Vec<String>>,
    pub force: bool,
}

/// Result of an indexing run.
#[derive(Debug, Default)]
pub struct IndexResult {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_unchanged: usize,
    pub nodes_created: usize,
    pub edges_created: usize,
    pub errors: Vec<String>,
    /// Populated by the resolver pass that runs at the end of
    /// `index_project` — the full R1 cascade (`resolve::resolve_project`)
    /// on a `--force` run, R4's targeted incremental re-resolve
    /// (`resolve::resolve_incremental`) otherwise.
    pub resolution: resolve::ResolveStats,
    /// Populated by the structural-fingerprint pass (step 5b, right after
    /// resolution — fingerprints read RESOLVED bindings). See
    /// `index::fingerprint`.
    pub fingerprints: fingerprint::FingerprintStats,
}

/// Run the generic indexer on a codebase.
pub async fn index_project(db: &Arc<Surreal<Any>>, config: &IndexConfig) -> Result<IndexResult> {
    let root = config
        .root_path
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {}", config.root_path.display()))?;

    tracing::info!(
        project_id = %config.project_id,
        root = %root.display(),
        tier = ?config.tier,
        "starting generic code indexing"
    );

    let mut result = IndexResult::default();

    // 1. Discover source files
    let source_files = discover_source_files(&root, config.languages.as_deref());
    result.files_scanned = source_files.len();
    tracing::info!(files = source_files.len(), "discovered source files");

    // 2. Determine which files need (re-)indexing
    let changes = if config.force {
        source_files
            .iter()
            .map(|p| FileChange::Added(p.clone()))
            .collect::<Vec<_>>()
    } else {
        incremental::detect_changes(db, &config.project_id, &source_files).await?
    };

    let to_index: Vec<&std::path::PathBuf> = changes
        .iter()
        .filter_map(|c| match c {
            FileChange::Added(p) | FileChange::Modified(p) => Some(p),
            FileChange::Unchanged(_) => None,
            FileChange::Deleted(_) => None,
        })
        .collect();

    let deleted: Vec<&std::path::PathBuf> = changes
        .iter()
        .filter_map(|c| match c {
            FileChange::Deleted(p) => Some(p),
            _ => None,
        })
        .collect();

    result.files_unchanged = changes
        .iter()
        .filter(|c| matches!(c, FileChange::Unchanged(_)))
        .count();

    tracing::info!(
        to_index = to_index.len(),
        unchanged = result.files_unchanged,
        deleted = deleted.len(),
        "incremental change detection complete"
    );

    // R4: which files actually changed this pass (Added/Modified/Deleted)
    // and the union of symbol names their change added or removed — the
    // incremental resolver's two-sided affected-edge set (see
    // `resolve::resolve_incremental`). Each file's *old* names have to be
    // snapshotted immediately before its rows are deleted below —
    // `clean_file_nodes`/`store_parsed_file`'s upsert doesn't leave that
    // state around to recover afterward.
    let mut changed_files: BTreeSet<String> = BTreeSet::new();
    let mut delta_names: BTreeSet<String> = BTreeSet::new();

    // Deletion tracking (design doc §10 option 1): definitions this run
    // observed disappearing (`deletion_rows`), and the (qualified_name,
    // file_path) keys this run (re-)defined (`live_keys`, which heal any
    // prior run's rows — a re-added symbol is no longer deleted). Not
    // collected on `--force`: a force run skips `detect_changes`, so it
    // can't see vanished files and re-baselines instead (the table is wiped
    // below — the design's documented graceful degradation).
    let mut deletion_rows: Vec<DeletedSymbolRow> = Vec::new();
    let mut live_keys: Vec<Vec<String>> = Vec::new();

    // 3. Clean up deleted files
    for path in &deleted {
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_str = rel.to_string_lossy();
        let old_symbols = resolve::load_file_symbols(db, &config.project_id, &rel_str).await?;
        // Name-keyed consumers filter here; `load_file_symbols` deliberately
        // returns unnamed rows too so `old_node_ids` below stays complete.
        delta_names.extend(
            old_symbols
                .iter()
                .filter(|s| !s.name.is_empty())
                .map(|s| s.name.clone()),
        );
        changed_files.insert(rel_str.to_string());
        deletion_rows.extend(
            old_symbols
                .iter()
                .filter(|s| s.node_type != "import" && !s.name.is_empty())
                .map(|s| DeletedSymbolRow {
                    project_id: config.project_id.clone(),
                    name: s.name.clone(),
                    qualified_name: s.qualified_name.clone(),
                    node_type: s.node_type.clone(),
                    file_path: rel_str.to_string(),
                }),
        );
        // Intentionally unfiltered — see `clean_file_nodes`.
        let old_node_ids: Vec<String> = old_symbols
            .iter()
            .map(|symbol| symbol.node_id.clone())
            .collect();
        clean_file_nodes(db, &config.project_id, &rel_str, &old_node_ids).await?;
    }

    // 4. Parse and store each file.
    for path in &to_index {
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();

        match parser::parse_file(path, &rel_str, &config.project_id, config.tier) {
            Ok(parsed) => {
                let old_symbols =
                    resolve::load_file_symbols(db, &config.project_id, &rel_str).await?;
                // Intentionally unfiltered — see `clean_file_nodes`. Name-keyed
                // consumers below (`old_names`, `deletion_rows`) filter for themselves.
                let old_node_ids: Vec<String> = old_symbols
                    .iter()
                    .map(|symbol| symbol.node_id.clone())
                    .collect();
                match store_parsed_file(
                    db,
                    &config.project_id,
                    &rel_str,
                    path,
                    &parsed,
                    &old_node_ids,
                )
                .await
                {
                    Ok((nodes, edges)) => {
                        result.nodes_created += nodes;
                        result.edges_created += edges;
                        result.files_indexed += 1;
                        changed_files.insert(rel_str.clone());
                        let old_names: BTreeSet<String> = old_symbols
                            .iter()
                            .filter(|s| !s.name.is_empty())
                            .map(|s| s.name.clone())
                            .collect();
                        let new_names: BTreeSet<String> =
                            parsed.nodes.iter().map(|n| n.name.clone()).collect();
                        delta_names.extend(old_names.symmetric_difference(&new_names).cloned());
                        if !config.force {
                            let new_qns: std::collections::HashSet<&str> = parsed
                                .nodes
                                .iter()
                                .map(|n| n.qualified_name.as_str())
                                .collect();
                            deletion_rows.extend(
                                old_symbols
                                    .iter()
                                    .filter(|s| {
                                        s.node_type != "import"
                                            && !s.name.is_empty()
                                            && !new_qns.contains(s.qualified_name.as_str())
                                    })
                                    .map(|s| DeletedSymbolRow {
                                        project_id: config.project_id.clone(),
                                        name: s.name.clone(),
                                        qualified_name: s.qualified_name.clone(),
                                        node_type: s.node_type.clone(),
                                        file_path: rel_str.clone(),
                                    }),
                            );
                            live_keys.extend(
                                parsed
                                    .nodes
                                    .iter()
                                    .filter(|n| n.node_type != "import")
                                    .map(|n| deleted_symbol_key(&n.qualified_name, &rel_str)),
                            );
                        }
                    }
                    Err(e) => {
                        let msg = format!("{rel_str}: store failed: {e}");
                        tracing::warn!("{msg}");
                        result.errors.push(msg);
                        result.files_skipped += 1;
                    }
                }
            }
            Err(e) => {
                let msg = format!("{rel_str}: parse failed: {e}");
                tracing::warn!("{msg}");
                result.errors.push(msg);
                result.files_skipped += 1;
            }
        }
    }

    // 4b. Deletion-tracking write-back (design doc §10 option 1). On
    // `--force`, wipe instead: a force run re-baselines "what exists"
    // without diffing (no `detect_changes`), so carrying deletion history
    // across it would mix two epochs — this is the documented degradation
    // ("--force behaves like today"), and doubles as the escape hatch if a
    // row is ever wrong. On any other run: heal every (qualified_name,
    // file_path) this run re-defined, then record what disappeared.
    if config.force {
        db.query("DELETE deleted_symbol WHERE project_id = $pid")
            .bind(("pid", config.project_id.clone()))
            .await
            .context("deleted_symbol force-wipe failed")?
            .check()
            .context("deleted_symbol force-wipe validation failed")?;
    } else {
        write_deleted_symbols(db, &config.project_id, deletion_rows, &live_keys).await?;
    }

    // 5. Resolver pass: `--force` (or a project's very first index, where
    // every file counts as changed anyway) gets the full R1 cascade over
    // every edge (`resolve_project`) — this doubles as the independent
    // ground truth R4's oracle property is checked against (see
    // `tests/incremental_reresolution.rs`). Any other run is a genuine
    // incremental re-index: R4's targeted two-sided re-resolve
    // (`resolve_incremental`) touches only edges from a changed file or
    // project-wide edges whose to_name matches a changed symbol name,
    // leaving every other edge's binding and resolution_gen untouched. See
    // `specs/resolution-layer-v1.md` §"Incremental re-resolution".
    result.resolution = if config.force {
        resolve::resolve_project(db, &config.project_id)
            .await
            .context("resolver pass failed")?
    } else {
        resolve::resolve_incremental(db, &config.project_id, &changed_files, &delta_names)
            .await
            .context("incremental resolver pass failed")?
    };

    // 5b. Structural fingerprints — after resolution (the neighborhood rule
    // reads RESOLVED bindings), on BOTH full and incremental paths; on
    // `--force` the pass wipes history first (same degradation contract as
    // the deleted_symbol wipe above). See `index::fingerprint` module docs.
    result.fingerprints = fingerprint::update_fingerprints(db, &config.project_id, config.force)
        .await
        .context("fingerprint pass failed")?;

    // 6. Update project registry
    update_project_registry(db, config, &result).await?;

    tracing::info!(
        files_indexed = result.files_indexed,
        nodes = result.nodes_created,
        edges = result.edges_created,
        skipped = result.files_skipped,
        errors = result.errors.len(),
        resolved = result.resolution.resolved,
        ambiguous = result.resolution.ambiguous,
        unresolved = result.resolution.unresolved,
        file_refs = result.resolution.file_refs,
        fingerprints = result.fingerprints.symbols,
        "indexing complete"
    );

    Ok(result)
}

/// Discover all source files under root, optionally filtered by language.
fn discover_source_files(root: &Path, languages: Option<&[String]>) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Skip hidden dirs, build artifacts, node_modules, .git, .jj,
            // target — but never on the walk root itself (depth 0): a
            // project root's own directory name (e.g. a dot-prefixed
            // tempdir, as `tempfile::tempdir()` creates — see
            // `tests/incremental_reresolution.rs`) isn't "a hidden dir
            // encountered while walking", it's the thing the caller
            // explicitly asked to index. `filter_entry` rejecting the root
            // itself would prune the entire walk to nothing.
            if e.file_type().is_dir() && e.depth() > 0 {
                return !name.starts_with('.')
                    && name != "node_modules"
                    && name != "target"
                    && name != "build"
                    && name != "dist"
                    && name != "vendor"
                    && name != "__pycache__";
            }
            true
        })
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let lang = parser::extension_to_language(ext);
        if lang.is_none() {
            continue;
        }

        // Apply language filter if specified
        if let Some(filter) = languages {
            let lang_name = lang.unwrap();
            if !filter.iter().any(|f| f.eq_ignore_ascii_case(lang_name)) {
                continue;
            }
        }

        files.push(path.to_path_buf());
    }

    files
}

/// Remove all nodes and edges for a file from the project.
async fn clean_file_nodes(
    db: &Surreal<Any>,
    project_id: &str,
    rel_path: &str,
    old_node_ids: &[String],
) -> Result<()> {
    let pid = project_id.to_string();
    let fp = rel_path.to_string();

    // Delete edges *emitted by* this file — i.e. `from_id` is one of its own
    // nodes. `code_edge` is a flat SCHEMALESS table keyed by `from_id`/
    // `to_id` strings (see `store_parsed_file` below), not a RELATE edge —
    // it has no `in`/`out` record links to filter on. The caller already
    // loaded this file's stored nodes for incremental resolution/deletion
    // tracking, so reuse those ids rather than making this DELETE scan
    // `code_edge` with an empty subquery result on every fresh-index file.
    //
    // INVARIANT: `old_node_ids` must list *every* node stored for this file,
    // not just the named ones. The node delete below is by `file_path` and so
    // is unconditional; if this edge delete saw a narrower set, the difference
    // would be nodes deleted while their outgoing edges survive as orphans
    // pointing at a dead `node_id`, accumulating on every re-index. That is
    // why `resolve::load_file_symbols` returns unnamed rows too and leaves
    // name filtering to its name-keyed callers — do not "tidy" that filter
    // back into the loader. Pinned by
    // `unnamed_nodes_are_still_covered_by_the_edge_delete_set`.
    //
    // Deliberately `from_id` only, NOT `to_id`: an edge whose *target*
    // happens to resolve into this file (a cross-file caller elsewhere)
    // still belongs to — and is only ever regenerated by re-parsing — its
    // own source file, not this one. Deleting it here would silently drop
    // the caller's reference entirely instead of letting R4's incremental
    // resolver re-examine it (`resolve::resolve_incremental`'s delta-name
    // match) and correctly flip it to UNRESOLVED — exactly the stale-
    // reference signal `graph::dependencies`'s `stale_references` and the
    // rename-refactor kill-test both depend on surviving.
    if !old_node_ids.is_empty() {
        db.query("DELETE code_edge WHERE project_id = $pid AND from_id IN $ids")
            .bind(("pid", pid.clone()))
            .bind(("ids", old_node_ids.to_vec()))
            .await?
            .check()?;
    }

    // Delete nodes
    db.query("DELETE code_node WHERE project_id = $pid AND file_path = $fp")
        .bind(("pid", pid.clone()))
        .bind(("fp", fp.clone()))
        .await?;

    // Delete file metadata
    db.query("DELETE file_metadata WHERE project_id = $pid AND file_path = $fp")
        .bind(("pid", pid))
        .bind(("fp", fp))
        .await?;

    tracing::debug!(file = rel_path, "cleaned deleted file nodes");
    Ok(())
}

/// One `code_node` row for a bulk `INSERT`. Serialized via SurrealDB's own
/// serializer (Rust `None` → `NONE`, matching the SCHEMAFULL `option<…>`
/// fields), not serde_json (`None` → `NULL`, which `option<string>` rejects).
/// `created_at` is intentionally absent — its schema DEFAULT fills it.
#[derive(SurrealValue)]
struct NodeRow {
    node_id: String,
    project_id: String,
    name: String,
    node_type: String,
    language: String,
    file_path: String,
    start_line: Option<i64>,
    end_line: Option<i64>,
    content: Option<String>,
    metadata: serde_json::Value,
    qualified_name: String,
}

/// One `code_edge` row for a bulk `INSERT`. code_edge is SCHEMALESS; the
/// `Option` targets still serialize `None → NONE` for consistency with the
/// per-record path the resolver/graph queries were written against.
#[derive(SurrealValue)]
struct EdgeRow {
    from_id: String,
    to_id: String,
    to_name: Option<String>,
    to_type: Option<String>,
    edge_type: String,
    confidence: String,
    weight: f64,
    project_id: String,
}

/// Store a parsed file's nodes and edges into SurrealDB.
async fn store_parsed_file(
    db: &Surreal<Any>,
    project_id: &str,
    rel_path: &str,
    abs_path: &Path,
    parsed: &ParsedFile,
    old_node_ids: &[String],
) -> Result<(usize, usize)> {
    // Delete existing nodes/edges for this file first (upsert pattern)
    clean_file_nodes(db, project_id, rel_path, old_node_ids).await?;

    let pid = project_id.to_string();
    let fp = rel_path.to_string();

    // Bulk-insert nodes in a single round-trip. One `INSERT INTO code_node
    // [ ... ]` replaces the old one-awaited-CREATE-per-node loop — that
    // per-record await chain was the dominant cost of a full index (~5k
    // sequential round-trips self-indexing this repo). code_node is
    // SCHEMAFULL; `created_at` is omitted on purpose so its schema DEFAULT
    // time::now() fills it. Bound as typed structs, NOT serde_json::json!,
    // deliberately: SurrealDB's serializer maps a Rust `None` to `NONE`,
    // whereas serde_json maps it to `null`/`NULL` — and `option<string>`
    // (`none | string`) rejects `NULL`, so a json! path silently failed every
    // file with a null `content`. `.check()` surfaces any row's validation
    // error, exactly as the per-record `resp.check()` did.
    if !parsed.nodes.is_empty() {
        let rows: Vec<NodeRow> = parsed
            .nodes
            .iter()
            .map(|node| NodeRow {
                node_id: node.id.clone(),
                project_id: pid.clone(),
                name: node.name.clone(),
                node_type: node.node_type.clone(),
                language: node.language.clone(),
                file_path: fp.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                content: node.content.clone(),
                metadata: serde_json::Value::Object(
                    node.metadata
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
                qualified_name: node.qualified_name.clone(),
            })
            .collect();

        db.query("INSERT INTO code_node $rows")
            .bind(("rows", rows))
            .await
            .with_context(|| format!("bulk node insert failed for {rel_path}"))?
            .check()
            .with_context(|| format!("bulk node insert validation failed for {rel_path}"))?;
    }
    let node_count = parsed.nodes.len();

    // Bulk-insert edges the same way (regular records, not RELATE). code_edge
    // is SCHEMALESS; `weight` mirrors the per-record CREATE's literal 1.0.
    // Unlike the old loop — which ignored every edge insert's Result — a
    // failure here now propagates rather than being silently dropped.
    if !parsed.edges.is_empty() {
        let erows: Vec<EdgeRow> = parsed
            .edges
            .iter()
            .map(|edge| EdgeRow {
                from_id: edge.from_id.clone(),
                to_id: edge.to_id.clone(),
                to_name: edge.to_name.clone(),
                to_type: edge.to_type.clone(),
                edge_type: edge.edge_type.clone(),
                confidence: edge.confidence.clone(),
                weight: 1.0,
                project_id: pid.clone(),
            })
            .collect();

        db.query("INSERT INTO code_edge $erows")
            .bind(("erows", erows))
            .await
            .with_context(|| format!("bulk edge insert failed for {rel_path}"))?
            .check()
            .with_context(|| format!("bulk edge insert validation failed for {rel_path}"))?;
    }
    let edge_count = parsed.edges.len();

    // Update file_metadata for incremental tracking
    let hash = incremental::hash_file(abs_path)?;
    let file_size = std::fs::metadata(abs_path).map(|m| m.len() as i64).ok();

    db.query(
        "DELETE file_metadata WHERE project_id = $pid AND file_path = $fp; \
         CREATE file_metadata SET \
            project_id = $pid, \
            file_path = $fp, \
            content_hash = $hash, \
            file_size = $fsize, \
            language = $lang, \
            node_count = $nc, \
            last_indexed_at = time::now()",
    )
    .bind(("pid", pid))
    .bind(("fp", fp))
    .bind(("hash", hash))
    .bind(("fsize", file_size))
    .bind(("lang", parsed.language.clone()))
    .bind(("nc", node_count as i64))
    .await?;

    Ok((node_count, edge_count))
}


/// One `deleted_symbol` row for a bulk `INSERT` — a definition an
/// incremental re-index observed disappearing (see `src/schema.surql`).
/// `deleted_at` is intentionally absent — its schema DEFAULT fills it.
#[derive(SurrealValue)]
struct DeletedSymbolRow {
    project_id: String,
    name: String,
    qualified_name: String,
    node_type: String,
    file_path: String,
}

/// Composite identity of a deletion fact: the same qualified name deleted
/// from two different files is two facts (crate-root files produce bare
/// qualified names, so cross-file qualified-name collisions are legal — a
/// heal keyed on qualified_name alone could erase a still-true row). A
/// two-element SurrealDB array value, compared whole against the heal
/// statement's bound key list.
fn deleted_symbol_key(qualified_name: &str, file_path: &str) -> Vec<String> {
    vec![qualified_name.to_string(), file_path.to_string()]
}

/// Heal-then-record, one round-trip. Healing runs against *prior* runs'
/// rows: any (qualified_name, file_path) freshly (re-)defined this run is by
/// definition not deleted. This run's own captures already exclude re-defined
/// qualified names per file (the diff in `index_project`), so heal and insert
/// can't fight. Rows never expire by age — an incomplete rename from three
/// weeks ago is still a break; false positives are impossible regardless of
/// row age because rows are only query *targets*: a rejection additionally
/// requires a live UNRESOLVED edge bare-tailing the name (see
/// `graph::dependencies::find_stale_references`).
async fn write_deleted_symbols(
    db: &Surreal<Any>,
    project_id: &str,
    rows: Vec<DeletedSymbolRow>,
    live_keys: &[Vec<String>],
) -> Result<()> {
    if !live_keys.is_empty() {
        db.query(
            "DELETE deleted_symbol WHERE project_id = $pid \
             AND [qualified_name, file_path] IN $keys",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("keys", live_keys.to_vec()))
        .await
        .context("deleted_symbol heal failed")?
        .check()
        .context("deleted_symbol heal validation failed")?;
    }

    if !rows.is_empty() {
        let count = rows.len();
        db.query("INSERT INTO deleted_symbol $rows")
            .bind(("rows", rows))
            .await
            .context("deleted_symbol bulk insert failed")?
            .check()
            .context("deleted_symbol bulk insert validation failed")?;
        tracing::debug!(deleted_symbols = count, "recorded disappeared definitions");
    }

    Ok(())
}

/// Create or update the project registry entry.
async fn update_project_registry(
    db: &Surreal<Any>,
    config: &IndexConfig,
    result: &IndexResult,
) -> Result<()> {
    let pid = config.project_id.clone();

    let name = config
        .root_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let root = config.root_path.to_string_lossy().to_string();

    db.query(
        "DELETE project_registry WHERE project_id = $pid; \
         CREATE project_registry SET \
            project_id = $pid, \
            name = $name, \
            root_path = $root, \
            node_count = $nc, \
            edge_count = $ec, \
            file_count = $fc, \
            last_indexed_at = time::now(), \
            created_at = time::now()",
    )
    .bind(("pid", pid))
    .bind(("name", name))
    .bind(("root", root))
    .bind(("nc", result.nodes_created as i64))
    .bind(("ec", result.edges_created as i64))
    .bind(("fc", result.files_scanned as i64))
    .await?;

    Ok(())
}
