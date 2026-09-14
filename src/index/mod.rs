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
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

impl IndexingTier {
    /// The spelling `--tier` accepts, so a message about the tier can be
    /// pasted straight back onto a command line.
    pub fn as_str(self) -> &'static str {
        match self {
            IndexingTier::Fast => "fast",
            IndexingTier::Balanced => "balanced",
            IndexingTier::Full => "full",
        }
    }
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

// ============================================================================
// Progress reporting
// ============================================================================
//
// Before this existed, `index` wrote three log lines for an entire run. On a
// 279-file C repository that is 8.6 minutes of silence, which reads as a
// hang, not as work. Everything below exists to turn that silence into a
// line a human or an agent can act on, and it is hand-rolled rather than
// pulled from a progress-bar crate because the whole requirement is two
// writers and a clock.
//
// Output goes to stderr, next to the tracing lines and away from the
// machine-readable stdout every `--json` path writes to.

/// How progress is rendered. Resolved once per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStyle {
    /// One line, rewritten in place. Chosen when stderr is a terminal.
    InPlace,
    /// One appended line per update, at a lower cadence. Chosen when stderr
    /// is a pipe or a file, which is what an agent, a CI job, and `2> log`
    /// all are. Rewriting a line in place there produces a wall of carriage
    /// returns nobody can read.
    Plain,
    /// Nothing is written.
    Off,
}

impl ProgressStyle {
    /// Decide the style for this run.
    ///
    /// `CODEGRAPH_PROGRESS` overrides the terminal check and accepts
    /// `tty`, `plain`, `off`, and `auto`. An unrecognized value falls back
    /// to `auto` rather than failing the run: progress is reporting, and
    /// reporting must never be the reason an index does not happen.
    pub fn detect() -> Self {
        match std::env::var("CODEGRAPH_PROGRESS")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "tty" => ProgressStyle::InPlace,
            "plain" => ProgressStyle::Plain,
            "off" | "none" | "0" => ProgressStyle::Off,
            _ => {
                if std::io::stderr().is_terminal() {
                    ProgressStyle::InPlace
                } else {
                    ProgressStyle::Plain
                }
            }
        }
    }
}

/// Whether a per-file progress line is due.
///
/// Pure, and separated out so the cadence is testable without a clock or a
/// terminal. The file thresholds are the "every N files" half of the rule
/// and the durations are the "or every T seconds" half, whichever comes
/// first. The 100 ms floor on the in-place path is the one addition: 25
/// files can go by in a millisecond on a warm cache, and a terminal
/// repainted a thousand times a second is worse than no progress at all.
pub fn progress_tick_due(
    style: ProgressStyle,
    files_since_last: usize,
    since_last: Duration,
) -> bool {
    match style {
        ProgressStyle::Off => false,
        ProgressStyle::InPlace => {
            (files_since_last >= 25 && since_last >= Duration::from_millis(100))
                || since_last >= Duration::from_secs(2)
        }
        ProgressStyle::Plain => {
            files_since_last >= 500 || since_last >= Duration::from_secs(10)
        }
    }
}

/// Phase timer and progress writer for one indexing run.
pub struct Progress {
    style: ProgressStyle,
    run_started: Instant,
    phase_started: Instant,
    phase_name: Option<String>,
    /// Completed phases, in order, with the wall time each took.
    phases: Vec<(String, Duration)>,
    last_tick: Instant,
    last_tick_files: usize,
    /// True when an in-place line is on screen and has to be cleared before
    /// anything else is written.
    line_open: bool,
}

impl Progress {
    /// A reporter for this run, with the style auto-detected.
    pub fn new() -> Self {
        Self::with_style(ProgressStyle::detect())
    }

    /// A reporter with the style forced. Used by the tests.
    pub fn with_style(style: ProgressStyle) -> Self {
        let now = Instant::now();
        Progress {
            style,
            run_started: now,
            phase_started: now,
            phase_name: None,
            phases: Vec::new(),
            last_tick: now,
            last_tick_files: 0,
            line_open: false,
        }
    }

    /// Wall time since the run started.
    pub fn elapsed(&self) -> Duration {
        self.run_started.elapsed()
    }

    /// Write one standalone line, outside any phase. Used for the tier
    /// notice at the top of a run.
    pub fn note(&mut self, text: &str) {
        self.emit(&format!("[codegraph] {text}"), false);
    }

    /// Close the phase in flight, if any, and open a new one.
    pub fn phase(&mut self, name: &str) {
        self.close_phase();
        self.phase_started = Instant::now();
        self.last_tick = self.phase_started;
        self.last_tick_files = 0;
        self.phase_name = Some(name.to_string());
        let at = self.run_started.elapsed();
        self.emit(
            &format!("[codegraph] {name} starting at {}", secs(at)),
            false,
        );
    }

    /// Report progress within the current phase. Rate-limited by
    /// [`progress_tick_due`]; call it once per file and let it decide.
    pub fn tick(&mut self, done: usize, total: usize) {
        if self.style == ProgressStyle::Off {
            return;
        }
        let since_last = self.last_tick.elapsed();
        let files_since_last = done.saturating_sub(self.last_tick_files);
        if !progress_tick_due(self.style, files_since_last, since_last) {
            return;
        }
        self.last_tick = Instant::now();
        self.last_tick_files = done;
        self.emit(&self.render_tick(done, total), true);
    }

    /// The per-file line. Split out so it can be rendered without writing.
    fn render_tick(&self, done: usize, total: usize) -> String {
        let elapsed = self.phase_started.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            done as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        let pct = if total > 0 {
            100.0 * done as f64 / total as f64
        } else {
            0.0
        };
        format!(
            "[codegraph]   {done}/{total} files ({pct:.0}%), {}, {rate:.1} files/s",
            secs(elapsed)
        )
    }

    /// Close the run: finish the last phase and print the total with the
    /// per-phase split, so a slow run can be attributed without a rerun.
    pub fn finish(&mut self) {
        self.close_phase();
        if self.style == ProgressStyle::Off {
            return;
        }
        let total = self.run_started.elapsed();
        let split = self
            .phases
            .iter()
            .map(|(name, d)| format!("{name} {}", secs(*d)))
            .collect::<Vec<_>>()
            .join(", ");
        if split.is_empty() {
            self.emit(&format!("[codegraph] done in {}", secs(total)), false);
        } else {
            self.emit(
                &format!("[codegraph] done in {} ({split})", secs(total)),
                false,
            );
        }
    }

    /// Record the elapsed time of the phase in flight.
    fn close_phase(&mut self) {
        if let Some(name) = self.phase_name.take() {
            self.phases.push((name, self.phase_started.elapsed()));
        }
    }

    /// Write one line to stderr.
    ///
    /// `transient` lines are the per-file ticks, which the in-place style
    /// overwrites and the plain style appends like any other line. A
    /// non-transient line always terminates whatever was on screen first,
    /// so a phase header never lands in the middle of a tick.
    fn emit(&mut self, text: &str, transient: bool) {
        if self.style == ProgressStyle::Off {
            return;
        }
        let mut err = std::io::stderr().lock();
        match (self.style, transient) {
            (ProgressStyle::InPlace, true) => {
                // \x1b[K clears from the cursor to the end of the line, so a
                // shorter line cannot leave the tail of a longer one behind.
                // Only ever written to a real terminal.
                let _ = write!(err, "\r{text}\x1b[K");
                let _ = err.flush();
                self.line_open = true;
            }
            (ProgressStyle::InPlace, false) => {
                if self.line_open {
                    let _ = write!(err, "\r\x1b[K");
                    self.line_open = false;
                }
                let _ = writeln!(err, "{text}");
                let _ = err.flush();
            }
            (_, _) => {
                let _ = writeln!(err, "{text}");
                let _ = err.flush();
            }
        }
    }
}

impl Default for Progress {
    fn default() -> Self {
        Progress::new()
    }
}

impl Drop for Progress {
    /// A run that ends early (an error out of a phase) must not leave a
    /// half-written in-place line on the terminal for the shell prompt to
    /// land in the middle of.
    fn drop(&mut self) {
        if self.line_open {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "\r\x1b[K");
            let _ = err.flush();
        }
    }
}

/// Format a duration the way every line here formats one.
fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
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
    /// Which file-discovery strategy produced the candidate list. Reported
    /// because "why is that file in my graph" and "why is it not" are both
    /// answered by this one fact.
    pub discovery: DiscoveryStrategy,
    /// Wall time for the whole run.
    pub elapsed: Duration,
}

/// Run the generic indexer on a codebase.
pub async fn index_project(db: &Arc<Surreal<Any>>, config: &IndexConfig) -> Result<IndexResult> {
    let root = config
        .root_path
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {}", config.root_path.display()))?;

    // `index <file>` used to succeed as a zero-file run: the walk of a file
    // yields the file itself, `filter_entry` never fires, and the result was
    // an empty graph reported as a success. Refuse it by name instead, and
    // say what to type.
    if root.is_file() {
        let dir = root.parent().unwrap_or(Path::new("."));
        anyhow::bail!(
            "{} is a file, and codegraph indexes directories. Point it at the directory that contains the file instead:\n  codegraph index {}",
            root.display(),
            dir.display()
        );
    }

    tracing::info!(
        project_id = %config.project_id,
        root = %root.display(),
        tier = ?config.tier,
        "starting generic code indexing"
    );

    let mut result = IndexResult::default();
    let mut progress = Progress::new();

    // The tier decides how much of the graph exists at all, and it has
    // always defaulted to the slowest one without saying so. Say so.
    progress.note(&format!(
        "tier {}. Lighter tiers exist: --tier fast indexes definitions only, --tier balanced adds call edges",
        config.tier.as_str()
    ));

    // 1. Discover source files
    progress.phase("discovery");
    let discovered = discover_source_files(&root, config.languages.as_deref());
    let source_files = discovered.files;
    result.discovery = discovered.strategy;
    result.files_scanned = source_files.len();
    tracing::info!(
        files = source_files.len(),
        strategy = %result.discovery.describe(),
        "discovered source files"
    );
    progress.note(&format!(
        "  {} source files found by {}",
        source_files.len(),
        result.discovery.describe()
    ));

    // 2. Determine which files need (re-)indexing
    progress.phase("change detection");
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
    progress.phase("parse+store");
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
    let to_index_total = to_index.len();
    for (done, path) in to_index.iter().enumerate() {
        progress.tick(done, to_index_total);
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

    // One last line so the phase ends on "all of them", not on whatever
    // count the rate limiter happened to stop at.
    progress.note(&format!(
        "  {to_index_total}/{to_index_total} files parsed and stored"
    ));

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
    // `file_ref` derivation is a sub-step inside the resolver pass, not a
    // phase this function can bracket: both `resolve_project` and
    // `resolve_incremental` build it before they return. Its size is
    // reported in the run summary as `file_refs` instead of as its own
    // timing.
    progress.phase("resolve");
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
    progress.phase("fingerprint");
    result.fingerprints = fingerprint::update_fingerprints(db, &config.project_id, config.force)
        .await
        .context("fingerprint pass failed")?;

    // 6. Update project registry
    progress.phase("registry");
    update_project_registry(db, config, &result).await?;

    result.elapsed = progress.elapsed();
    progress.finish();

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

/// Which mechanism produced the candidate file list for a run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DiscoveryStrategy {
    /// `git ls-files --cached --others --exclude-standard`, run inside the
    /// walk root. Gives exact gitignore semantics for free: `.gitignore` at
    /// every level, `.git/info/exclude`, and the user's global excludes
    /// file, with tracked files always included and ignored files always
    /// out. Untracked-but-not-ignored files are in, so a file written a
    /// second ago is indexed without being committed first.
    Git,
    /// A directory walk plus the built-in skip list. The fallback.
    Walk {
        /// Why git discovery was not used. Printed, because "it indexed my
        /// build output" and "git is not installed here" are the same
        /// symptom from the user's side.
        reason: String,
    },
    /// No discovery has run yet. Only ever seen on a default-constructed
    /// [`IndexResult`].
    #[default]
    Unknown,
}

impl DiscoveryStrategy {
    /// One short phrase for the log line and the run summary.
    pub fn describe(&self) -> String {
        match self {
            DiscoveryStrategy::Git => {
                "git ls-files, so .gitignore is respected".to_string()
            }
            DiscoveryStrategy::Walk { reason } => {
                format!("directory walk with the built-in skip list ({reason})")
            }
            DiscoveryStrategy::Unknown => "not run".to_string(),
        }
    }
}

/// The candidate source files, and how they were found.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// Absolute paths, in discovery order.
    pub files: Vec<PathBuf>,
    /// The mechanism that produced them.
    pub strategy: DiscoveryStrategy,
}

/// Directories that are never first-party source, whatever git thinks of
/// them.
///
/// This list predates gitignore support and survives it. A repository that
/// commits its `vendor/` tree (normal in Go) or its `node_modules` (rare but
/// real) is tracked by git and would otherwise be pulled in as the project's
/// own code, which is both wrong and the difference between a 30-second
/// index and a 30-minute one. Keeping it applied on both discovery paths
/// also means turning gitignore support on cannot widen what gets indexed,
/// only narrow it.
fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.')
        || name == "node_modules"
        || name == "target"
        || name == "build"
        || name == "dist"
        || name == "vendor"
        || name == "__pycache__"
}

/// True when this path is a language codegraph can parse and passes the
/// `--languages` filter.
fn wanted_source_file(path: &Path, languages: Option<&[String]>) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let Some(lang) = parser::extension_to_language(ext) else {
        return false;
    };
    match languages {
        Some(filter) => filter.iter().any(|f| f.eq_ignore_ascii_case(lang)),
        None => true,
    }
}

/// Discover all source files under root, optionally filtered by language.
///
/// Prefers git's own idea of what belongs to the project, and falls back to
/// walking the tree. The fallback is not a lesser mode: it is what runs on a
/// plain directory with no repository around it, which is a first-class way
/// to point codegraph at a codebase.
pub fn discover_source_files(root: &Path, languages: Option<&[String]>) -> Discovery {
    match git_tracked_files(root) {
        Ok(paths) => {
            let files: Vec<PathBuf> = paths
                .into_iter()
                .filter(|rel| {
                    rel.parent()
                        .into_iter()
                        .flat_map(Path::components)
                        .all(|c| !is_skipped_dir(&c.as_os_str().to_string_lossy()))
                })
                .map(|rel| root.join(rel))
                .filter(|p| wanted_source_file(p, languages))
                // `--cached` lists files that are in the index but no longer
                // on disk, and git lists a symlink as an ordinary entry.
                // `symlink_metadata` rejects both without following a link
                // out of the tree, which matches what the walk has always
                // done with `follow_links(false)`.
                .filter(|p| {
                    std::fs::symlink_metadata(p)
                        .map(|m| m.file_type().is_file())
                        .unwrap_or(false)
                })
                .collect();

            // An empty git answer is ambiguous in a way that matters: it is
            // what a repository with no source files looks like, and it is
            // also what indexing a directory that is itself gitignored looks
            // like (a vendored copy under `third_party/`, say). Falling back
            // costs one directory walk and cannot produce fewer files, so it
            // is never the wrong side to err on.
            if files.is_empty() {
                return walk_source_files(
                    root,
                    languages,
                    "git listed no source files under this path".to_string(),
                );
            }

            Discovery {
                files,
                strategy: DiscoveryStrategy::Git,
            }
        }
        Err(reason) => walk_source_files(root, languages, reason),
    }
}

/// Ask git for every file it considers part of the project under `root`.
///
/// `--cached` is the tracked set, `--others` adds untracked files, and
/// `--exclude-standard` subtracts everything the ignore rules exclude. That
/// combination is the definition of "files that belong to this project", and
/// it is exactly the semantics a user means when they ask why their build
/// output got indexed.
///
/// Returns the reason it could not be used on failure, for the log line.
/// Nothing here is fatal: every failure mode falls through to the walk.
fn git_tracked_files(root: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()
        .map_err(|e| format!("git could not be run: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("git ls-files failed");
        return Err(format!("git declined: {}", first.trim()));
    }

    // Paths come back relative to `root` because of `-C`, NUL-separated
    // because of `-z`, which is the only encoding that survives a newline in
    // a filename. A path that is not UTF-8 is dropped rather than lossily
    // converted, because a lossy path does not open.
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for chunk in output.stdout.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(chunk) else {
            continue;
        };
        // During a merge conflict `--cached` lists one path once per stage.
        if seen.insert(text.to_string()) {
            files.push(PathBuf::from(text));
        }
    }
    Ok(files)
}

/// Walk the tree and keep every parsable source file, skipping the
/// directories in [`is_skipped_dir`].
fn walk_source_files(root: &Path, languages: Option<&[String]>, reason: String) -> Discovery {
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
                return !is_skipped_dir(&name);
            }
            true
        })
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if !wanted_source_file(entry.path(), languages) {
            continue;
        }
        files.push(entry.path().to_path_buf());
    }

    Discovery {
        files,
        strategy: DiscoveryStrategy::Walk { reason },
    }
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
