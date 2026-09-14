//! Binding a `touches:` entry to the code graph. **Owned by lane L2.**
//!
//! A [`Touch`] is a name that has to be bound to something in the index,
//! which is the same problem `index::resolve` solves for call and member
//! edges. The vocabulary is therefore the same one, exactly:
//! [`TouchConfidence::Resolved`] means the selector bound to one target,
//! [`TouchConfidence::Ambiguous`] means several candidates survived and
//! none was chosen, [`TouchConfidence::Unresolved`] means nothing matched.
//! A plan carrying an UNRESOLVED touch is stale in precisely the sense a
//! stale code reference is stale, and it is reported in the same words so
//! the same judgment applies.
//!
//! ## Why this is not the r1 to r6 cascade
//!
//! `index::resolve`'s cascade is **context sensitive**. Its middle rules
//! scope the candidate pool by the *calling file*: R3 prefers a definition
//! in the caller's own file, R4 narrows by the caller's import facts, R5
//! narrows by the caller's language family. Those rules take a source file
//! as input and cannot run without one.
//!
//! A plan has no source file. `- symbol: sync` is written by a human in a
//! YAML document, not captured at a call site, so there is no caller to
//! scope by and no import list to consult. Running the cascade here would
//! mean inventing a context, and an invented context is exactly the kind of
//! silent guess the RESOLVED / AMBIGUOUS / UNRESOLVED vocabulary exists to
//! refuse.
//!
//! So this is a deliberately **context free** lookup: exact qualified name
//! first, then bare name, and several matches are reported as several
//! rather than narrowed by a rule whose input does not exist. The only
//! pieces of the resolver reused are the ones that are context free and
//! must not drift: [`normalize_separators`] and [`bare_name`], called
//! rather than reimplemented, so a plan and a call site agree on what
//! `a.b.C` means.
//!
//! ## Determinism
//!
//! Same file plus same index gives the same rows, byte for byte. Every
//! candidate list and every glob expansion is sorted before it leaves this
//! module, and nothing here iterates a `HashMap` into a result.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::index::resolve::{bare_name, normalize_separators};
use crate::plan::model::{ResolvedTouch, Selector, Touch, TouchConfidence};

/// Why a selector did not bind to exactly one target.
///
/// Stable and machine readable. Every variant here means the plan names
/// something that is not there, which is the only thing UNRESOLVED is
/// allowed to mean: a path with no file behind it, a symbol nothing answers
/// to, a glob that matched nothing, or a name several definitions answer to.
///
/// "The file exists but the graph has no node for it" is deliberately NOT
/// one of these. That is what the `indexed` flag on a bound row records
/// instead, because a roadmap item that touches a spec, a README, or a
/// schema file is a normal roadmap item and reporting it as stale would make
/// the staleness signal noise on the day it shipped. See
/// [`TouchIndex::resolve_file`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    /// No file at that path, in the index or on disk.
    NoSuchFile,
    /// Nothing in the index answers to this symbol name.
    NoSuchSymbol,
    /// The glob matched no file, indexed or on disk.
    NoGlobMatch,
    /// Several candidates survived and none was chosen. Carried on an
    /// AMBIGUOUS row, where the candidate list is the real answer and this
    /// is the tag that lets a caller filter on it.
    AmbiguousCandidates,
}

impl UnresolvedReason {
    /// The exact string written to `work_touch.reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            UnresolvedReason::NoSuchFile => "no_such_file",
            UnresolvedReason::NoSuchSymbol => "no_such_symbol",
            UnresolvedReason::NoGlobMatch => "no_glob_match",
            UnresolvedReason::AmbiguousCandidates => "ambiguous_candidates",
        }
    }

    /// Parse a `work_touch.reason` value back. Unknown values read as
    /// `None` rather than panicking, so a row written by a build that knows
    /// a reason this one does not is merely less informative here, never
    /// fatal.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "no_such_file" => Some(UnresolvedReason::NoSuchFile),
            "no_such_symbol" => Some(UnresolvedReason::NoSuchSymbol),
            "no_glob_match" => Some(UnresolvedReason::NoGlobMatch),
            "ambiguous_candidates" => Some(UnresolvedReason::AmbiguousCandidates),
            _ => None,
        }
    }
}

impl fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one `touches:` entry became.
///
/// One entry can produce more than one row. A `glob:` that matches four
/// files is four incidences on the hyperedge, not one, which is what keeps
/// the incidence table literally one row per (item, code entity) and keeps
/// collision detection and blast radius a plain set intersection.
///
/// `rows` is never empty: an entry that bound to nothing still gets its
/// UNRESOLVED row, so the text the author wrote survives in the store and
/// `plan stale` can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchBinding {
    /// One row per matched entity, sorted.
    pub rows: Vec<BoundRow>,
    /// The verdict every row in `rows` carries.
    pub confidence: TouchConfidence,
    /// Why it did not bind to exactly one target. `None` on RESOLVED.
    pub reason: Option<UnresolvedReason>,
    /// One sentence naming what was looked for and what was found.
    pub detail: Option<String>,
}

/// One `work_touch` row, plus the one thing a `ResolvedTouch` cannot carry.
///
/// `indexed` is the bit that keeps the confidence vocabulary honest. A
/// `file:` or `glob:` touch binds to a path, and a path can be real without
/// the code graph having a single node for it: a spec, a README, a `.surql`
/// schema. That touch is RESOLVED, because the thing it names exists, and
/// `indexed` is `false`, because nothing in the graph can be reached from
/// it. A `symbol:` touch binds to a node id, so the question does not arise
/// and this is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundRow {
    /// The row itself.
    pub touch: ResolvedTouch,
    /// Whether the code graph holds a row for this target.
    pub indexed: Option<bool>,
}

impl TouchBinding {
    fn resolved(rows: Vec<BoundRow>) -> Self {
        TouchBinding {
            rows,
            confidence: TouchConfidence::Resolved,
            reason: None,
            detail: None,
        }
    }

    fn ambiguous(touch: &Touch, candidates: Vec<String>, detail: String) -> Self {
        TouchBinding {
            rows: vec![BoundRow {
                touch: ResolvedTouch::ambiguous(touch, candidates),
                indexed: None,
            }],
            confidence: TouchConfidence::Ambiguous,
            reason: Some(UnresolvedReason::AmbiguousCandidates),
            detail: Some(detail),
        }
    }

    fn unresolved(touch: &Touch, reason: UnresolvedReason, detail: String) -> Self {
        TouchBinding {
            rows: vec![BoundRow {
                touch: ResolvedTouch::unresolved(touch),
                indexed: None,
            }],
            confidence: TouchConfidence::Unresolved,
            reason: Some(reason),
            detail: Some(detail),
        }
    }
}

/// One indexed symbol, projected to what a touch lookup needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolFacts {
    /// `code_node.node_id`.
    pub node_id: String,
    /// Bare name.
    pub name: String,
    /// Computed `::` separated qualified name.
    pub qualified_name: String,
    /// Repo relative path of the file that defines it.
    pub file_path: String,
    /// Node type, e.g. `function`, `struct`.
    pub node_type: String,
}

/// Everything a touch lookup reads, loaded once per command.
///
/// Built from the indexed graph, never from the working tree, with one
/// exception: [`TouchIndex::root`] is used solely to tell "this path is on
/// disk but was not indexed" apart from "this path does not exist", which
/// is a distinction the graph alone cannot make.
pub struct TouchIndex {
    /// Absolute path the index's `file_path` values are relative to, when
    /// it is known. Read from `project_registry.root_path`, which is the
    /// root the project was actually indexed at, so it is the right frame
    /// of reference rather than wherever the command happens to run.
    root: Option<PathBuf>,
    /// Every repo relative path the index knows, sorted and deduplicated.
    /// Union of `code_node.file_path` and `file_metadata.file_path`: the
    /// second is what makes a file that parsed to zero symbols still count
    /// as indexed.
    files: Vec<String>,
    /// Every repo relative path that exists in the working tree, sorted.
    /// A superset of `files` in practice, since it includes the specs,
    /// docs, and schema files a roadmap legitimately points at and the
    /// indexer has no grammar for.
    disk_files: Vec<String>,
    /// How `disk_files` was found, for the reader who wants to know why a
    /// glob missed something.
    disk_strategy: String,
    /// Symbols by exact qualified name, each list sorted by node id.
    by_qualified: HashMap<String, Vec<usize>>,
    /// Symbols by bare name, each list sorted by node id.
    by_bare: HashMap<String, Vec<usize>>,
    /// Symbols by defining file, each list sorted by node id.
    by_file: BTreeMap<String, Vec<usize>>,
    /// Symbol facts by node id.
    by_id: HashMap<String, usize>,
    /// The symbols themselves, sorted by node id.
    symbols: Vec<SymbolFacts>,
}

impl TouchIndex {
    /// Load everything a touch lookup reads for one project.
    ///
    /// `root_hint` is used only when `project_registry` carries no
    /// `root_path` for the project, which happens when the roadmap is
    /// synced into a store the project was never registered in.
    pub async fn build(
        db: &Surreal<Any>,
        project_id: &str,
        root_hint: Option<&Path>,
    ) -> Result<Self> {
        let nodes = crate::graph::load_project_nodes(db, project_id)
            .await
            .context("loading indexed symbols for touch resolution failed")?;

        let mut resp = db
            .query("SELECT file_path FROM file_metadata WHERE project_id = $pid")
            .bind(("pid", project_id.to_string()))
            .await
            .context("loading indexed file list for touch resolution failed")?;
        let meta_rows: Vec<surrealdb_types::Value> = resp.take(0)?;

        let mut resp = db
            .query("SELECT root_path FROM project_registry WHERE project_id = $pid")
            .bind(("pid", project_id.to_string()))
            .await
            .context("loading the project's indexed root failed")?;
        let registry_rows: Vec<surrealdb_types::Value> = resp.take(0)?;

        let registered_root = registry_rows
            .iter()
            .find_map(|v| value_field(v, "root_path"))
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let mut files: BTreeSet<String> = BTreeSet::new();
        for row in &meta_rows {
            if let Some(path) = value_field(row, "file_path") {
                if !path.is_empty() {
                    files.insert(path);
                }
            }
        }

        // Sorted by node id so every candidate list this index produces is
        // already in a deterministic order before anything sorts it again.
        let mut symbols: Vec<SymbolFacts> = nodes
            .into_iter()
            .map(|n| SymbolFacts {
                node_id: n.id,
                name: n.name,
                qualified_name: n.qualified_name,
                file_path: n.file_path,
                node_type: n.node_type,
            })
            .collect();
        symbols.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        let mut by_qualified: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_bare: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();

        for (i, s) in symbols.iter().enumerate() {
            if !s.file_path.is_empty() {
                files.insert(s.file_path.clone());
                by_file.entry(s.file_path.clone()).or_default().push(i);
            }
            by_id.insert(s.node_id.clone(), i);
            // An `import` node is a reference, not a definition. The same
            // exclusion `graph::dependencies::matching_roots` applies to
            // every name anchored query, applied here for the same reason:
            // `- symbol: Foo` means the definition of Foo, and counting
            // every file that imports it as a candidate would make almost
            // every symbol touch AMBIGUOUS for no information gain.
            if s.node_type == "import" {
                continue;
            }
            if !s.qualified_name.is_empty() {
                by_qualified
                    .entry(s.qualified_name.clone())
                    .or_default()
                    .push(i);
            }
            if !s.name.is_empty() {
                by_bare.entry(s.name.clone()).or_default().push(i);
            }
        }

        let root = registered_root.or_else(|| root_hint.map(Path::to_path_buf));
        let (disk_files, disk_strategy) = match root.as_deref() {
            Some(r) => working_tree_files(r),
            None => (
                Vec::new(),
                "not scanned, because the project's root is unknown".to_string(),
            ),
        };

        Ok(TouchIndex {
            root,
            disk_files,
            disk_strategy,
            files: files.into_iter().collect(),
            by_qualified,
            by_bare,
            by_file,
            by_id,
            symbols,
        })
    }

    /// How many indexed files this index knows about.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// How many indexed symbols this index knows about, imports included.
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// The root the index's paths are relative to, when it is known.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// How many files the working tree scan found.
    pub fn disk_file_count(&self) -> usize {
        self.disk_files.len()
    }

    /// How the working tree scan found them.
    pub fn disk_strategy(&self) -> &str {
        &self.disk_strategy
    }

    /// True when a file exists at this repo relative path in the working
    /// tree. Checked directly rather than through the scan list, so a file
    /// written since the scan is still found.
    pub fn on_disk(&self, path: &str) -> bool {
        match self.root.as_ref() {
            Some(root) => root.join(path).is_file(),
            None => false,
        }
    }

    /// Facts about one indexed symbol.
    pub fn symbol(&self, node_id: &str) -> Option<&SymbolFacts> {
        self.by_id.get(node_id).map(|&i| &self.symbols[i])
    }

    /// Every symbol defined in one file, sorted by node id.
    pub fn symbols_in_file(&self, file_path: &str) -> Vec<&SymbolFacts> {
        self.by_file
            .get(file_path)
            .map(|idx| idx.iter().map(|&i| &self.symbols[i]).collect())
            .unwrap_or_default()
    }

    /// True when the index has a row for this repo relative path.
    pub fn has_file(&self, path: &str) -> bool {
        self.files.binary_search(&path.to_string()).is_ok()
    }

    /// Bind one `touches:` entry.
    pub fn resolve(&self, touch: &Touch) -> TouchBinding {
        match touch.selector {
            Selector::File => self.resolve_file(touch),
            Selector::Symbol => self.resolve_symbol(touch),
            Selector::Glob => self.resolve_glob(touch),
        }
    }

    /// `file:` binds to the repo relative path itself, because a path has
    /// no node of its own in this graph.
    ///
    /// **A file that exists binds.** Whether the code graph holds anything
    /// for it is recorded separately, in `indexed`, not folded into the
    /// confidence. The distinction is the difference between a useful
    /// staleness report and a useless one: on this repository's own roadmap
    /// 23 of 61 touches name specs, a README, and a `.surql` schema, all of
    /// them present and correct and none of them a file codegraph has a
    /// grammar for. Calling those UNRESOLVED made `plan stale` 100 percent
    /// noise on the day it shipped, while saying nothing true that
    /// `indexed: false` does not say better.
    ///
    /// UNRESOLVED is therefore reserved for the one case that is actually a
    /// stale plan: there is no file at that path at all.
    fn resolve_file(&self, touch: &Touch) -> TouchBinding {
        let path = normalize_repo_path(&touch.raw);
        let indexed = self.has_file(&path);
        if indexed || self.on_disk(&path) {
            return TouchBinding::resolved(vec![BoundRow {
                touch: ResolvedTouch::resolved(touch, path),
                indexed: Some(indexed),
            }]);
        }
        TouchBinding::unresolved(
            touch,
            UnresolvedReason::NoSuchFile,
            match self.root() {
                Some(root) => format!(
                    "no file at {path}, in the index or under {}. The plan points at a file that is not there",
                    root.display()
                ),
                None => format!(
                    "{path} is not in the index, and the project's root is unknown so the working tree could not be checked. Index this project and sync again"
                ),
            },
        )
    }

    /// `symbol:` binds to a `code_node.node_id`.
    ///
    /// Exact qualified name first, then bare name. A raw carrying `::`,
    /// `.`, or `/` is a qualified name and gets the exact lookup; a bare
    /// raw skips straight to the bare lookup, since comparing a bare string
    /// against `qualified_name` would only ever match a top level symbol
    /// the bare lookup already finds.
    ///
    /// A failed qualified lookup falls through to the bare tail rather than
    /// stopping, because `Commands::Index` naming a variant whose computed
    /// `qualified_name` is `cli::Commands::Index` is a spelling difference,
    /// not a stale plan. The fallback can only ever widen the candidate
    /// set, and a widened set that is not a singleton comes back AMBIGUOUS
    /// with every candidate listed, never as a pick.
    fn resolve_symbol(&self, touch: &Touch) -> TouchBinding {
        let normalized = normalize_separators(&touch.raw);
        let qualified = touch.raw.contains("::") || touch.raw.contains('.') || touch.raw.contains('/');

        if qualified {
            if let Some(idx) = self.by_qualified.get(&normalized) {
                return self.verdict_for(touch, idx, &format!("qualified name {normalized}"));
            }
        }

        let bare = bare_name(&normalized);
        if let Some(idx) = self.by_bare.get(bare) {
            let looked_for = if qualified {
                format!("bare name {bare}, after no symbol matched the qualified name {normalized}")
            } else {
                format!("bare name {bare}")
            };
            return self.verdict_for(touch, idx, &looked_for);
        }

        TouchBinding::unresolved(
            touch,
            UnresolvedReason::NoSuchSymbol,
            format!(
                "no indexed symbol answers to {}, by qualified name or by bare name. The plan points at a symbol that is not there",
                touch.raw
            ),
        )
    }

    /// One or several matches, never a pick.
    fn verdict_for(&self, touch: &Touch, idx: &[usize], looked_for: &str) -> TouchBinding {
        if idx.len() == 1 {
            let s = &self.symbols[idx[0]];
            // A symbol touch binds to a node id, which only exists because
            // the file was indexed, so `indexed` has nothing to add here.
            return TouchBinding::resolved(vec![BoundRow {
                touch: ResolvedTouch::resolved(touch, s.node_id.clone()),
                indexed: None,
            }]);
        }
        let candidates: Vec<String> = idx.iter().map(|&i| self.symbols[i].node_id.clone()).collect();
        let where_they_live: Vec<String> = idx
            .iter()
            .map(|&i| {
                let s = &self.symbols[i];
                format!("{} ({} in {})", s.qualified_name, s.node_type, s.file_path)
            })
            .collect();
        TouchBinding::ambiguous(
            touch,
            candidates,
            format!(
                "{} matched {} symbols by {looked_for}: {}. Write the qualified name to pick one",
                touch.raw,
                idx.len(),
                where_they_live.join(", ")
            ),
        )
    }

    /// `glob:` expands against the working tree, one row per match.
    ///
    /// Matched against the union of the working tree scan and the indexed
    /// paths, for the same reason [`TouchIndex::resolve_file`] checks disk:
    /// `- glob: "docs/**"` names files that exist, and answering "nothing
    /// matched" because none of them is Rust would be false. Each row
    /// carries its own `indexed`, since one glob routinely spans both.
    fn resolve_glob(&self, touch: &Touch) -> TouchBinding {
        let pattern = normalize_repo_path(&touch.raw);
        let mut candidates: BTreeSet<&str> = BTreeSet::new();
        candidates.extend(self.files.iter().map(String::as_str));
        candidates.extend(self.disk_files.iter().map(String::as_str));

        let matches: Vec<&str> = candidates
            .into_iter()
            .filter(|path| glob_matches(&pattern, path))
            .collect();

        if matches.is_empty() {
            return TouchBinding::unresolved(
                touch,
                UnresolvedReason::NoGlobMatch,
                format!(
                    "{} matched none of the {} indexed paths or the {} working tree paths found by {}. The glob names files that are not there",
                    touch.raw,
                    self.files.len(),
                    self.disk_files.len(),
                    self.disk_strategy
                ),
            );
        }

        // A `BTreeSet` walk is sorted, so the expansion is deterministic.
        TouchBinding::resolved(
            matches
                .into_iter()
                .map(|path| BoundRow {
                    touch: ResolvedTouch::resolved(touch, path.to_string()),
                    indexed: Some(self.has_file(path)),
                })
                .collect(),
        )
    }
}

/// Every file in the working tree, repo relative and sorted, plus how it was
/// found.
///
/// A roadmap points at more than source code, so this cannot reuse
/// `index::discover_source_files`, which filters to the languages codegraph
/// has grammars for. It follows the same two-strategy shape that function
/// does, for the same reasons and with the same skip list, so a glob in a
/// plan and a glob in the indexer disagree about a path only when git
/// itself does.
///
/// `git ls-files --cached --others --exclude-standard` first: exact
/// gitignore semantics for free, including untracked-but-not-ignored files,
/// so a file written a second ago is matchable without being committed.
/// A directory walk is the fallback, and it is not a lesser mode: it is what
/// runs on a plain directory with no repository around it.
fn working_tree_files(root: &Path) -> (Vec<String>, String) {
    match git_listed_files(root) {
        Ok(files) => (
            files,
            "git ls-files, so .gitignore is respected".to_string(),
        ),
        Err(reason) => (
            walked_files(root),
            format!("a directory walk with the built-in skip list ({reason})"),
        ),
    }
}

/// `git ls-files` at `root`, or why it could not be used.
fn git_listed_files(root: &Path) -> std::result::Result<Vec<String>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()
        .map_err(|e| format!("git could not be run: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git declined: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .filter(|p| !p.split('/').any(is_skipped_dir))
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// The fallback walk.
fn walked_files(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .is_some_and(is_skipped_dir)
        })
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Directories a roadmap never means, whatever git thinks of them. The same
/// list `index::is_skipped_dir` applies, kept here because that one is
/// private and because a plan and an index agreeing on this is a contract,
/// not a coincidence: a repository that commits its `vendor/` tree would
/// otherwise turn one `glob:` into thousands of incidences.
fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.')
        || name == "node_modules"
        || name == "target"
        || name == "build"
        || name == "dist"
        || name == "vendor"
        || name == "__pycache__"
}

/// Read one string field off a raw SurrealDB row.
fn value_field(value: &surrealdb_types::Value, key: &str) -> Option<String> {
    let surrealdb_types::Value::Object(obj) = value else {
        return None;
    };
    match obj.get(key) {
        Some(surrealdb_types::Value::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// Put a path written by a human into the shape `code_node.file_path`
/// carries: forward slashes, no `./` prefix, no leading or trailing slash.
///
/// Deliberately textual and non canonicalizing. A `..` segment is left
/// alone rather than resolved, because resolving it would let a plan reach
/// outside the indexed root through a path that then fails to match
/// anything anyway, and silently rewriting what the author wrote is the
/// opposite of what the verbatim `raw` column is for.
pub fn normalize_repo_path(raw: &str) -> String {
    let unified = raw.replace('\\', "/");
    let trimmed = unified.trim().trim_matches('/');
    let mut out = String::with_capacity(trimmed.len());
    let mut first = true;
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if !first {
            out.push('/');
        }
        out.push_str(segment);
        first = false;
    }
    out
}

/// Match a repo relative path against a glob pattern.
///
/// Supported, and nothing else:
/// - `**` matches zero or more whole path segments.
/// - `*` matches zero or more characters within one segment, never `/`.
/// - `?` matches exactly one character within one segment, never `/`.
///
/// `src/index/**` therefore matches `src/index/mod.rs` and
/// `src/index/extractors/rust.rs`, and `src/**/*.rs` matches `src/main.rs`
/// as well as `src/plan/ops.rs`, because a `**` segment is allowed to
/// consume nothing.
///
/// Written here rather than pulled in as a dependency: the board forbids
/// new crates without justification, and the whole of the behavior above is
/// forty lines with no allocation.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    match_segments(&p, &s)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => {
            // Zero or more whole segments. Try every split point; the
            // shortest consumption is tried first, which keeps the common
            // trailing `**` case at one step.
            for take in 0..=path.len() {
                if match_segments(&pattern[1..], &path[take..]) {
                    return true;
                }
            }
            false
        }
        Some(seg) => match path.first() {
            None => false,
            Some(actual) => {
                match_one_segment(seg.as_bytes(), actual.as_bytes())
                    && match_segments(&pattern[1..], &path[1..])
            }
        },
    }
}

/// `*` and `?` within a single segment, iterative with one backtrack point,
/// which is the standard linear time wildcard match and cannot blow the
/// stack on a pathological pattern.
fn match_one_segment(pattern: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut resume) = (usize::MAX, 0usize);

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star = pi;
            resume = ti;
            pi += 1;
        } else if star != usize::MAX {
            resume += 1;
            ti = resume;
            pi = star + 1;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_star_spans_whole_segments_and_may_consume_none() {
        assert!(glob_matches("src/index/**", "src/index/mod.rs"));
        assert!(glob_matches("src/index/**", "src/index/extractors/rust.rs"));
        assert!(glob_matches("src/**/*.rs", "src/main.rs"));
        assert!(glob_matches("src/**/*.rs", "src/plan/ops.rs"));
        assert!(!glob_matches("src/index/**", "src/plan/ops.rs"));
        assert!(!glob_matches("src/**/*.rs", "tests/common/mod.rs"));
    }

    #[test]
    fn single_star_never_crosses_a_separator() {
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/plan/ops.rs"));
        assert!(glob_matches("src/plan/*.rs", "src/plan/ops.rs"));
    }

    #[test]
    fn question_mark_is_exactly_one_character() {
        assert!(glob_matches("src/?ain.rs", "src/main.rs"));
        assert!(!glob_matches("src/?.rs", "src/main.rs"));
    }

    #[test]
    fn a_pathological_star_run_still_terminates() {
        // Backtracking matchers that recurse on every `*` go exponential
        // here. This one is linear and answers immediately.
        assert!(!match_one_segment(
            b"*a*a*a*a*a*a*a*a*b",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
    }

    #[test]
    fn repo_paths_normalize_to_the_stored_shape() {
        assert_eq!(normalize_repo_path("./src/cli.rs"), "src/cli.rs");
        assert_eq!(normalize_repo_path("/src/cli.rs"), "src/cli.rs");
        assert_eq!(normalize_repo_path("src//cli.rs"), "src/cli.rs");
        assert_eq!(normalize_repo_path("src\\cli.rs"), "src/cli.rs");
        assert_eq!(normalize_repo_path("  src/cli.rs  "), "src/cli.rs");
        // `..` is left exactly as written; see the function docs.
        assert_eq!(normalize_repo_path("../other/cli.rs"), "../other/cli.rs");
    }

    #[test]
    fn reason_strings_round_trip() {
        for r in [
            UnresolvedReason::NoSuchFile,
            UnresolvedReason::NoSuchSymbol,
            UnresolvedReason::NoGlobMatch,
            UnresolvedReason::AmbiguousCandidates,
        ] {
            assert_eq!(UnresolvedReason::from_str_opt(r.as_str()), Some(r));
        }
        assert_eq!(UnresolvedReason::from_str_opt("invented_later"), None);
        // Both of these were reasons an earlier build wrote for a file that
        // exists and has no node. Such a touch now binds RESOLVED with
        // `indexed: false`, so the strings are gone rather than kept as
        // vocabulary nothing produces.
        assert_eq!(UnresolvedReason::from_str_opt("no_such_file_in_index"), None);
        assert_eq!(UnresolvedReason::from_str_opt("file_type_not_indexed"), None);
    }
}
