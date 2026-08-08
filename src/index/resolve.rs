//! The resolver pass (R1): a post-extraction, language-agnostic cascade
//! that materializes real `to_id` bindings on every name-edge an extractor
//! left unresolved (`to_id = ""`, `to_name`/`to_type` carrying the lookup
//! key — see `parser::ExtractionContext::add_name_edge`). Implements
//! `specs/resolution-layer-v1.md` §"The resolver cascade" verbatim; the two
//! readings the spec text itself doesn't fully pin down (R1/R2 require a
//! qualified `to_name`; every rule's candidate pool is scoped to the
//! calling edge's own language) follow `tests/fixtures/README.md`'s
//! "Contract interpretations", cross-checked against this suite's own
//! `expected.yaml` manifests.
//!
//! Split in two halves:
//! - a pure, DB-free core (`ResolverNode`, `UnresolvedEdge`, `Indices`,
//!   `resolve_one`/`resolve_all`) with no full-project assumptions baked
//!   into the per-edge binding function — `resolve_one` takes whatever
//!   `nodes`/`indices` it's handed, so a future incremental pass (R4) can
//!   rebuild `Indices` from the current node set and re-resolve just the
//!   edges that need it, rather than the whole project.
//! - a thin SurrealDB adapter (`resolve_project`) that loads one project's
//!   nodes/edges, runs the pure core, writes bindings back, and derives the
//!   `file_ref` file-level graph from every cross-file RESOLVED binding.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use surrealdb_types::SurrealValue;

// ============================================================================
// Pure resolver core
// ============================================================================

/// A `code_node` row, projected to exactly the fields the cascade reads.
/// Deliberately its own type rather than `parser::CodeNode` — resolution
/// works over the whole project post-storage, and needs `file_path`, which
/// only exists as a DB column (a single-file `CodeNode` has no need for it).
#[derive(Debug, Clone)]
pub struct ResolverNode {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub language: String,
    pub file_path: String,
    pub qualified_name: String,
}

/// A `code_edge` row with `to_id = ""` — the resolver's unit of work.
/// `to_name` is kept verbatim (never normalized) so it can also serve as
/// the identifying key for writing the binding back.
#[derive(Debug, Clone)]
pub struct UnresolvedEdge {
    pub from_id: String,
    pub to_name: String,
    pub to_type: String,
    pub edge_type: String,
}

/// The cascade's verdict for one `UnresolvedEdge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub to_id: Option<String>,
    pub confidence: &'static str,
    pub resolved_by: &'static str,
    /// Populated (AMBIGUOUS only) with the survivor node ids, sorted for
    /// determinism — matches the spec's data-model note verbatim
    /// ("candidates: [node_id] (AMBIGUOUS only)").
    pub candidates: Option<Vec<String>>,
}

/// One fact extracted from a Rust `use` declaration — R4's only feed in v1
/// ("Slot exists in v1; only Rust feeds it" per the spec).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportFact {
    /// The imported path, `::`-normalized, with a leading `crate` segment
    /// stripped (this codebase's `qualified_name`s have no crate-root
    /// segment of their own — see `qualified_name::module_path`).
    path: String,
    /// `use foo::*;` matches any *direct* child of `path`, not `path`
    /// itself and not a deeper descendant — mirrors real glob-import scope.
    glob: bool,
}

impl ImportFact {
    fn matches(&self, candidate_qualified_name: &str) -> bool {
        if self.glob {
            return if self.path.is_empty() {
                // `use crate::*;` — direct children of the crate root are
                // simply top-level (unqualified) names.
                !candidate_qualified_name.contains("::")
            } else {
                match candidate_qualified_name
                    .strip_prefix(self.path.as_str())
                    .and_then(|rest| rest.strip_prefix("::"))
                {
                    Some(rest) => !rest.contains("::"),
                    None => false,
                }
            };
        }
        candidate_qualified_name == self.path
            || candidate_qualified_name.ends_with(&format!("::{}", self.path))
    }
}

/// Precomputed, edge-independent lookup structures over one project's
/// nodes — built once per resolution pass and shared across every edge's
/// `resolve_one` call. Rebuilding is cheap and side-effect-free, which is
/// what lets a future incremental pass call `build_indices` again on a
/// refreshed node set and reuse `resolve_one` unchanged.
pub struct Indices {
    by_id: HashMap<String, usize>,
    by_bare_key: HashMap<(String, String, String), Vec<usize>>,
    import_facts: HashMap<String, Vec<ImportFact>>,
}

/// Candidate-pool language scoping, adjudicated: a **closed table** of
/// language-tag merges, each requiring its own semantic justification —
/// not an open-ended "families are whatever's convenient" abstraction.
/// Only two merges exist:
/// - `{c, cpp}` — real `extern "C"` linkage means C and C++ share one
///   linker namespace in every deployed reality; `tests/fixtures/c-cpp`'s
///   `bonus-r5` case (a `.cpp` caller resolving `legacy_helper`, defined in
///   a plain `.c` file) is the fixture proof this merge is required, not
///   optional — measured directly: without it, that case regresses from
///   RESOLVED/r5 to UNRESOLVED (a false negative), which is worse than the
///   (nonexistent, in every fixture) risk of a false collision this merge
///   could theoretically introduce.
/// - `{javascript, typescript}` — TypeScript compiles to JavaScript; both
///   run in one JS runtime namespace at the point anything actually calls
///   anything, and both share this codebase's one extractor module.
///
/// Every other language tag is a singleton family. In particular, Python
/// and Go never share a namespace under any circumstance — no compilation
/// target, no shared runtime, no linkage — so `tests/fixtures/polyglot`
/// case c (`connect` defined in both `app.py` and `main.go`) must NOT
/// resolve as a collision; scoping them as separate singleton families is
/// what keeps that case from going AMBIGUOUS across a boundary no call
/// syntax in either language can actually cross. Do not add a merge here
/// without an equivalent fixture-backed justification — the table itself
/// is the contract, not a heuristic.
///
/// `pub` for the same reason as `normalize_separators`/`bare_name` below:
/// `graph::dependencies`'s stale-reference scan has to scope its candidate
/// pool exactly the way the cascade scoped it, and a second copy of this
/// closed table would be free to drift away from the contract.
pub fn language_family(language: &str) -> &str {
    match language {
        "c" | "cpp" => "c-cpp",
        "javascript" | "typescript" => "typescript",
        other => other,
    }
}

/// Normalize every separator an extractor might hand back (`.`, `/`) to
/// the canonical `::`. Applied only for matching — the edge's stored
/// `to_name` stays verbatim forever (the spec's audit-trail requirement).
/// `pub`, not just an internal cascade helper: `graph::dependencies` reuses
/// it verbatim (rather than re-implementing normalization) to check whether
/// a project-wide `UNRESOLVED` edge's raw `to_name` bare-tails to a queried
/// name — the only way a renamed-away target's stale callers are ever
/// visible (see that module's docs). Query-time matching must use the
/// *exact* rule the resolver used, or the two could silently drift apart.
pub fn normalize_separators(raw: &str) -> String {
    raw.replace(['.', '/'], "::")
}

/// The last `::`-segment of an already-normalized name. `pub` for the same
/// reason as `normalize_separators` above.
pub fn bare_name(normalized: &str) -> &str {
    normalized.rsplit("::").next().unwrap_or(normalized)
}

/// Strip a leading run of Rust module-path keywords (`crate::`, `self::`,
/// `super::` — the last repeatable) from an already-`::`-normalized name,
/// yielding the crate-relative remainder. This codebase's `qualified_name`s
/// carry no `crate` segment (see `qualified_name::module_path`), so a
/// `crate::a::b` call can only ever match `a::b` — R1/R2 as written match
/// literally and never strip, which is exactly why every module-path-prefixed
/// call went UNRESOLVED. Returns the input unchanged when it carries no such
/// prefix (the common case — the caller uses that to skip the R2m retry).
fn strip_rust_module_prefix(normalized: &str) -> &str {
    let mut n = normalized;
    loop {
        if let Some(r) = n.strip_prefix("crate::") {
            n = r;
        } else if let Some(r) = n.strip_prefix("self::") {
            n = r;
        } else if let Some(r) = n.strip_prefix("super::") {
            n = r;
        } else {
            return n;
        }
    }
}

/// Build the lookup structures `resolve_one` needs from a project's full
/// node set.
pub fn build_indices(nodes: &[ResolverNode]) -> Indices {
    let mut by_id = HashMap::with_capacity(nodes.len());
    let mut by_bare_key: HashMap<(String, String, String), Vec<usize>> = HashMap::new();

    for (i, n) in nodes.iter().enumerate() {
        by_id.insert(n.id.clone(), i);
        let key = (
            n.name.clone(),
            n.node_type.clone(),
            language_family(&n.language).to_string(),
        );
        by_bare_key.entry(key).or_default().push(i);
    }
    // Sort each bucket by node id so candidate order (and therefore the
    // `candidates` list on AMBIGUOUS edges) never depends on node
    // insertion/HashMap-iteration order — see `determinism_under_shuffled_insertion_order`.
    for bucket in by_bare_key.values_mut() {
        bucket.sort_by(|&a, &b| nodes[a].id.cmp(&nodes[b].id));
    }

    let mut import_facts: HashMap<String, Vec<ImportFact>> = HashMap::new();
    for n in nodes {
        if n.node_type == "import" && n.language == "rust" {
            import_facts
                .entry(n.file_path.clone())
                .or_default()
                .extend(parse_rust_use(&n.name));
        }
    }

    Indices {
        by_id,
        by_bare_key,
        import_facts,
    }
}

/// Apply the R1-R6 cascade to a single edge: the first rule whose
/// candidate pool narrows to exactly one node wins. Two readings not fully
/// pinned down by the spec's prose, followed here per
/// `tests/fixtures/README.md` ("Contract interpretations") and one
/// empirical correction found self-indexing this very repo (see the
/// paragraph below):
/// - A *qualified* (multi-segment) `to_name` is R1/R2's exclusive
///   territory. If neither finds exactly one match, resolution terminates
///   right there (R6, using R1's ∪ R2's candidates) — it never falls back
///   to bare-tail matching (R3-R5). A bare identifier is R3→R4→R5→R6's
///   territory exclusively, with R1/R2 skipped entirely (this half of the
///   split is what `tests/fixtures/README.md` documents directly: it keeps
///   R2's suffix test from trivially subsuming every bare name too, which
///   would make R3-R5 dead code).
/// - Every rule's candidate pool is scoped to the edge's own language
///   *family* (see `language_family`'s closed table: `{c, cpp}` and
///   `{javascript, typescript}` merge, everything else is a singleton) —
///   required for sane multi-language behavior (a same-named collision
///   across two languages that can't call each other must never surface
///   as ambiguity; `tests/fixtures/polyglot` case c, Python `connect` vs.
///   Go `connect`, is the regression test for the singleton side).
///
/// The first half of that split (qualified names never degrading to
/// bare-tail matching) is *not* directly stated by any fixture manifest —
/// it was added after self-indexing this repo surfaced real false
/// bindings/false ambiguity without it: `HashMap::new` (a call to the
/// stdlib) bound to this codebase's unrelated `CodegraphServer::new` just
/// because "new" happened to be otherwise project-unique, and
/// `cursor.node()` (a tree-sitter `TreeCursor` method) flagged AMBIGUOUS
/// against two unrelated project functions bare-named `node`. A qualified
/// prefix is real information ("HashMap::" is *not* "CodegraphServer::"
/// with the module elided) that bare-tail fallback silently discards.
pub fn resolve_one(edge: &UnresolvedEdge, nodes: &[ResolverNode], indices: &Indices) -> Binding {
    let Some(&src_idx) = indices.by_id.get(edge.from_id.as_str()) else {
        // Orphaned edge (source node missing) — shouldn't happen from a
        // real extractor pass, but fails safe rather than panicking.
        return Binding {
            to_id: None,
            confidence: "UNRESOLVED",
            resolved_by: "r6",
            candidates: None,
        };
    };
    let source = &nodes[src_idx];

    let normalized = normalize_separators(&edge.to_name);
    let bare = bare_name(&normalized).to_string();
    let is_qualified = normalized.contains("::");

    let key = (
        bare,
        edge.to_type.clone(),
        language_family(&source.language).to_string(),
    );
    let pool: &[usize] = indices
        .by_bare_key
        .get(&key)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    if is_qualified {
        // R1: exact qualified_name match.
        let r1_hits: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|&i| nodes[i].qualified_name == normalized)
            .collect();
        if r1_hits.len() == 1 {
            return bind(&nodes[r1_hits[0]], "r1");
        }

        // R2: qualified-suffix match (`db::connect` binds `myapp::db::connect`).
        let suffix = format!("::{normalized}");
        let r2_hits: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|&i| nodes[i].qualified_name.ends_with(&suffix))
            .collect();
        if r2_hits.len() == 1 {
            return bind(&nodes[r2_hits[0]], "r2");
        }

        // R2m — Rust module-relative retry (a second empirical correction, in
        // the same spirit as the qualified-never-degrades-to-bare-tail rule
        // above; found self-indexing this repo). The cascade has no notion of
        // module paths, so `crate::`/`self::`/`super::`-prefixed calls never
        // match a (crate-relative) `qualified_name` under R1/R2's literal test.
        // Strip the prefix and retry exact-or-suffix, Rust-family only (those
        // tokens are Rust keywords; `strip_rust_module_prefix` no-ops on every
        // other language's names). Purely additive: reached only after R1 and
        // R2 already failed to bind uniquely, and binds solely on its OWN
        // unique match — so it can turn UNRESOLVED→RESOLVED but never rebinds
        // an existing edge or manufactures new ambiguity (a non-unique R2m pool
        // simply falls through to the unchanged R1∪R2 terminal below). Measured
        // 2026-07-11 (`examples/resolver_ceiling.rs`): 25 self-index edges,
        // every one unique — e.g. `crate::db::connect`, `super::load_project_edges`.
        if source.language == "rust" {
            let rel = strip_rust_module_prefix(&normalized);
            if rel != normalized {
                let rel_suffix = format!("::{rel}");
                let rm_hits: Vec<usize> = pool
                    .iter()
                    .copied()
                    .filter(|&i| {
                        nodes[i].qualified_name == rel
                            || nodes[i].qualified_name.ends_with(&rel_suffix)
                    })
                    .collect();
                if rm_hits.len() == 1 {
                    return bind(&nodes[rm_hits[0]], "r2m");
                }
            }
        }

        // Terminal: R1 and R2's conditions are mutually exclusive (exact
        // match has no prefix; suffix match requires a non-empty one), so
        // their hits never overlap — no dedup needed beyond concatenation.
        let mut combined = r1_hits;
        combined.extend(r2_hits);
        return terminal(nodes, &combined);
    }

    // R3: same-file bare-name match.
    let hits: Vec<usize> = pool
        .iter()
        .copied()
        .filter(|&i| nodes[i].file_path == source.file_path)
        .collect();
    if hits.len() == 1 {
        return bind(&nodes[hits[0]], "r3");
    }

    // R4: import-informed. Only Rust files carry any `import_facts` entry
    // today — every other language's files have none, so this naturally
    // no-ops and falls through to R5 there.
    if let Some(facts) = indices.import_facts.get(&source.file_path) {
        let hits: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|&i| facts.iter().any(|f| f.matches(&nodes[i].qualified_name)))
            .collect();
        if hits.len() == 1 {
            return bind(&nodes[hits[0]], "r4");
        }
    }

    // R5 + R6 terminal: project-unique bare name (within this edge's
    // language family), else AMBIGUOUS/UNRESOLVED over the full pool.
    terminal(nodes, pool)
}

/// R6, shared by both branches above: exactly one survivor resolves (only
/// ever reachable from the bare-name branch as "r5" — the qualified
/// branch's R1/R2 already returned early on their own exactly-one case, so
/// its `hits` here is never length 1); more than one is AMBIGUOUS with
/// every survivor as a candidate; none is UNRESOLVED.
fn terminal(nodes: &[ResolverNode], hits: &[usize]) -> Binding {
    if hits.len() == 1 {
        return bind(&nodes[hits[0]], "r5");
    }
    if hits.is_empty() {
        return Binding {
            to_id: None,
            confidence: "UNRESOLVED",
            resolved_by: "r6",
            candidates: None,
        };
    }
    let mut ids: Vec<String> = hits.iter().map(|&i| nodes[i].id.clone()).collect();
    ids.sort();
    Binding {
        to_id: None,
        confidence: "AMBIGUOUS",
        resolved_by: "r6",
        candidates: Some(ids),
    }
}

fn bind(node: &ResolverNode, rule: &'static str) -> Binding {
    Binding {
        to_id: Some(node.id.clone()),
        confidence: "RESOLVED",
        resolved_by: rule,
        candidates: None,
    }
}

/// Resolve every edge against one shared, freshly-built `Indices` — the
/// full-project entry point (`resolve_project` below drives this). The
/// order of `edges` never affects any individual result (see
/// `determinism_under_shuffled_insertion_order`); the returned `Vec` is
/// positional, parallel to `edges`.
pub fn resolve_all(nodes: &[ResolverNode], edges: &[UnresolvedEdge]) -> Vec<Binding> {
    let indices = build_indices(nodes);
    edges.iter().map(|e| resolve_one(e, nodes, &indices)).collect()
}

// ============================================================================
// Rust `use`-declaration parsing (R4's only import-fact source in v1)
// ============================================================================

/// Parse one `use` declaration's raw source text (an `import` node's
/// `name`, which is the extractor's verbatim capture — see
/// `extractors::rust::extract_use`) into the paths it brings into scope.
/// Handles named imports (`use crate::alpha::helper;`), module imports
/// (`use crate::db::connection;`), globs (`use crate::alpha::*;`), and
/// grouped/nested imports (`use crate::foo::{bar, baz::{a, b}};`).
/// Aliases (`use foo::Bar as Baz;`) are matched by their *real* path, not
/// the local alias name — R4 stays a heuristic slot, not a symbol table,
/// and no fixture exercises this case.
fn parse_rust_use(raw: &str) -> Vec<ImportFact> {
    let Some(use_pos) = raw.find("use ") else {
        return Vec::new();
    };
    let body = raw[use_pos + 4..].trim().trim_end_matches(';').trim();
    let mut out = Vec::new();
    parse_use_tree(body, "", &mut out);
    out
}

fn parse_use_tree(tree: &str, prefix: &str, out: &mut Vec<ImportFact>) {
    let tree = tree.trim();
    if tree.is_empty() {
        return;
    }

    if let Some(open) = tree.find('{') {
        let Some(close) = tree.rfind('}') else {
            return; // malformed; skip defensively rather than guess
        };
        let head = tree[..open].trim().trim_end_matches(':');
        let new_prefix = join_path(prefix, head);
        for part in split_top_level(&tree[open + 1..close]) {
            parse_use_tree(part, &new_prefix, out);
        }
        return;
    }

    let path_part = tree.split(" as ").next().unwrap_or(tree).trim();
    if path_part.is_empty() || path_part == "self" {
        return;
    }

    let full = join_path(prefix, path_part);
    if let Some(module) = full.strip_suffix("::*") {
        out.push(ImportFact {
            path: strip_crate_prefix(module),
            glob: true,
        });
    } else if full == "*" {
        out.push(ImportFact {
            path: strip_crate_prefix(prefix),
            glob: true,
        });
    } else {
        out.push(ImportFact {
            path: strip_crate_prefix(&full),
            glob: false,
        });
    }
}

fn join_path(prefix: &str, segment: &str) -> String {
    let segment = segment.trim();
    if prefix.is_empty() {
        segment.to_string()
    } else if segment.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}::{segment}")
    }
}

fn strip_crate_prefix(path: &str) -> String {
    match path.strip_prefix("crate::") {
        Some(rest) => rest.to_string(),
        None if path == "crate" => String::new(),
        None => path.to_string(),
    }
}

/// Split on top-level commas only — a comma inside a nested `{...}` group
/// doesn't end the current segment.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

// ============================================================================
// SurrealDB adapter
// ============================================================================

/// Outcome of one full resolution pass, printed by the CLI.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolveStats {
    pub edges_considered: usize,
    pub resolved: usize,
    pub ambiguous: usize,
    pub unresolved: usize,
    pub file_refs: usize,
}

/// One row's worth of resolver output, ready to write back — carries the
/// original (verbatim) identifying fields alongside the computed `Binding`
/// so the write-back can re-find the exact row(s) it came from. Used by the
/// incremental pass (`resolve_incremental`), whose small, targeted
/// affected-set is written with `write_updates`'s per-edge UPDATE-by-WHERE.
struct EdgeUpdate {
    from_id: String,
    to_name: String,
    to_type: String,
    edge_type: String,
    binding: Binding,
}

/// A fully-materialized `code_edge` name-edge row, ready for a bulk `INSERT`.
/// The full pass (`resolve_project`) rewrites its whole unresolved set by
/// delete-and-reinsert of these rather than 4k+ per-edge UPDATEs — an INSERT
/// is an append (no per-row index lookup + rewrite), which is dramatically
/// cheaper on surrealkv. Serialized via SurrealDB's serializer (derive
/// `SurrealValue`): `None` → `NONE`, so `candidates` matches the schema's
/// `option<array<string>>`. `to_id` is a plain string (`""` when
/// unresolved/ambiguous) to match every existing `to_id`-shaped reader.
#[derive(Clone, SurrealValue)]
struct ResolvedEdgeRow {
    from_id: String,
    to_id: String,
    to_name: Option<String>,
    to_type: Option<String>,
    edge_type: String,
    confidence: String,
    weight: f64,
    project_id: String,
    resolved_by: String,
    resolution_gen: i64,
    candidates: Option<Vec<String>>,
}

const WRITE_CHUNK_SIZE: usize = 250;

/// Run the full resolver pass for one project: load its nodes and every
/// currently-unresolved (`to_id = ""`) edge, apply the cascade, write
/// `to_id`/`confidence`/`resolved_by`/`resolution_gen`/`candidates` back,
/// and derive the `file_ref` file-level graph from every cross-file
/// RESOLVED binding. Called automatically at the end of `codegraph index`
/// (see `index::index_project`); also reachable standalone via the
/// `codegraph resolve` CLI subcommand for re-runs against already-indexed
/// data.
pub async fn resolve_project(db: &Surreal<Any>, project_id: &str) -> Result<ResolveStats> {
    let nodes = load_nodes(db, project_id).await?;
    let edges = load_unresolved_edges(db, project_id).await?;
    let gen = next_resolution_gen(db, project_id).await?;

    // Only needed for the file_ref derivation below (id -> file_path);
    // `resolve_all` builds its own full `Indices` internally.
    let file_of: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.file_path.as_str()))
        .collect();

    let mut stats = ResolveStats {
        edges_considered: edges.len(),
        ..Default::default()
    };
    let mut rows: Vec<ResolvedEdgeRow> = Vec::with_capacity(edges.len());
    // Cross-file (from_file, to_file) pairs derived from RESOLVED bindings
    // this pass — a `BTreeSet` both dedups and gives deterministic write
    // order, though order has no functional effect here.
    let mut file_pairs: BTreeSet<(String, String)> = BTreeSet::new();

    let bindings = resolve_all(&nodes, &edges);
    for (edge, binding) in edges.into_iter().zip(bindings) {
        match binding.confidence {
            "RESOLVED" => {
                stats.resolved += 1;
                if let Some(to_id) = &binding.to_id {
                    if let (Some(&from_file), Some(&to_file)) =
                        (file_of.get(edge.from_id.as_str()), file_of.get(to_id.as_str()))
                    {
                        if from_file != to_file {
                            file_pairs.insert((from_file.to_string(), to_file.to_string()));
                        }
                    }
                }
            }
            "AMBIGUOUS" => stats.ambiguous += 1,
            _ => stats.unresolved += 1,
        }
        rows.push(ResolvedEdgeRow {
            from_id: edge.from_id,
            to_id: binding.to_id.unwrap_or_default(),
            to_name: Some(edge.to_name),
            to_type: Some(edge.to_type),
            edge_type: edge.edge_type,
            confidence: binding.confidence.to_string(),
            weight: 1.0,
            project_id: project_id.to_string(),
            resolved_by: binding.resolved_by.to_string(),
            resolution_gen: gen,
            candidates: binding.candidates,
        });
    }

    // Full-pass write-back by delete-and-reinsert (see `ResolvedEdgeRow`).
    // `load_unresolved_edges` loaded exactly the `to_id = ''` name-edges, so
    // clearing that set and reinserting each row with its binding is
    // equivalent to the old `write_updates(.., require_unresolved = true)` —
    // but replaces 4k+ UPDATE-by-WHERE executions with one DELETE + a handful
    // of chunked bulk INSERTs. EXTRACTED (real `to_id`) and `file_ref` edges
    // never carry `to_id = ''`, so they're untouched.
    db.query("DELETE code_edge WHERE project_id = $pid AND to_id = ''")
        .bind(("pid", project_id.to_string()))
        .await
        .context("clearing unresolved edges before rewrite failed")?
        .check()
        .context("clearing unresolved edges before rewrite failed")?;
    insert_resolved_edges(db, &rows).await?;
    stats.file_refs = write_file_refs(db, project_id, &file_pairs).await?;

    Ok(stats)
}

/// Bulk-insert fully-materialized name-edge rows in chunks — the full pass's
/// write-back path (`resolve_project`). Each chunk is one `INSERT INTO
/// code_edge [ ... ]` round trip.
async fn insert_resolved_edges(
    db: &Surreal<Any>,
    rows: &[ResolvedEdgeRow],
) -> Result<()> {
    for chunk in rows.chunks(WRITE_CHUNK_SIZE) {
        db.query("INSERT INTO code_edge $rows")
            .bind(("rows", chunk.to_vec()))
            .await
            .context("resolver bulk edge insert failed")?
            .check()
            .context("resolver bulk edge insert returned an error")?;
    }
    Ok(())
}

/// R4: two-sided incremental re-resolution, wired into the existing
/// content-hash incremental indexer (`index::index_project`). Given the
/// files that changed this pass (`changed_files` — already relative,
/// project-normalized paths; Added, Modified, *and* Deleted all count, per
/// `incremental::FileChange`) and the union of symbol names their change
/// added or removed (`delta_names` — `index::index_project` owns the
/// before/after snapshot, since it has to happen before each file's old
/// rows are deleted), re-resolves exactly:
/// (a) every edge whose source is a node in one of `changed_files` — those
///     files were just re-parsed, so every name-edge they emit is freshly
///     `to_id = ''` regardless of what it used to be;
/// (b) every edge project-wide, *regardless of its current confidence*,
///     whose `to_name`'s normalized bare tail is one of `delta_names` — the
///     only way a renamed-away or newly-added target's *other* callers ever
///     get re-examined.
/// (See `specs/resolution-layer-v1.md` §"Incremental re-resolution".)
///
/// Every edge outside that union keeps its existing binding and
/// `resolution_gen` untouched entirely — invalidation is per-affected-edge,
/// not a global gen bump (the spec's own prose reads as if a stale gen
/// might invalidate project-wide; that reading is wrong and would defeat
/// the point of an *incremental* pass). `file_ref` is still fully
/// recomputed from the project's entire current RESOLVED set
/// (`recompute_file_refs`) rather than selectively — it's cheap, derived,
/// read-only work, not a re-resolution, so recomputing it in full doesn't
/// compromise the edge-level selectivity this function is otherwise for.
pub async fn resolve_incremental(
    db: &Surreal<Any>,
    project_id: &str,
    changed_files: &BTreeSet<String>,
    delta_names: &BTreeSet<String>,
) -> Result<ResolveStats> {
    if changed_files.is_empty() {
        // Nothing changed this pass (e.g. every file came back Unchanged) —
        // nothing can possibly be affected, and querying the whole project
        // just to learn that would be pure waste.
        return Ok(ResolveStats::default());
    }

    let nodes = load_nodes(db, project_id).await?;
    let all_edges = load_all_edges(db, project_id).await?;
    let gen = next_resolution_gen(db, project_id).await?;

    let changed_ids: HashSet<&str> = nodes
        .iter()
        .filter(|n| changed_files.contains(&n.file_path))
        .map(|n| n.id.as_str())
        .collect();

    let affected: Vec<UnresolvedEdge> = all_edges
        .into_iter()
        .filter(|e| {
            if changed_ids.contains(e.from_id.as_str()) {
                return true;
            }
            let normalized = normalize_separators(&e.to_name);
            delta_names.contains(bare_name(&normalized))
        })
        .collect();

    let mut stats = ResolveStats {
        edges_considered: affected.len(),
        ..Default::default()
    };

    let indices = build_indices(&nodes);
    let mut updates = Vec::with_capacity(affected.len());
    for edge in affected {
        let binding = resolve_one(&edge, &nodes, &indices);
        match binding.confidence {
            "RESOLVED" => stats.resolved += 1,
            "AMBIGUOUS" => stats.ambiguous += 1,
            _ => stats.unresolved += 1,
        }
        updates.push(EdgeUpdate {
            from_id: edge.from_id,
            to_name: edge.to_name,
            to_type: edge.to_type,
            edge_type: edge.edge_type,
            binding,
        });
    }

    // Unlike `resolve_project`'s call, this must NOT require `to_id = ''` —
    // the whole point is being able to flip an already-RESOLVED edge whose
    // target just got renamed away.
    write_updates(db, project_id, &updates, gen, false).await?;
    stats.file_refs = recompute_file_refs(db, project_id, &nodes).await?;

    Ok(stats)
}

/// Fully re-derive `file_ref` from every currently-RESOLVED edge
/// project-wide — not just the subset `resolve_incremental` touched this
/// pass. `resolve_project`'s full pass folds the equivalent computation
/// into its own resolve loop instead (there, "every edge just resolved" and
/// "the project's entire RESOLVED set" are the same set by construction);
/// here they aren't, since only a subset of edges were re-resolved, so this
/// re-reads the RESOLVED set fresh rather than reusing the incremental
/// pass's own (partial) bindings.
async fn recompute_file_refs(
    db: &Surreal<Any>,
    project_id: &str,
    nodes: &[ResolverNode],
) -> Result<usize> {
    let mut resp = db
        .query(
            "SELECT from_id, to_id FROM code_edge \
             WHERE project_id = $pid AND confidence = 'RESOLVED' AND edge_type != 'file_ref'",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading resolved edges for file_ref recompute failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    let file_of: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.file_path.as_str()))
        .collect();

    let mut file_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for row in &rows {
        let surrealdb_types::Value::Object(obj) = row else {
            continue;
        };
        let from_id = extract_str(obj, "from_id");
        let to_id = extract_str(obj, "to_id");
        if let (Some(&ff), Some(&tf)) =
            (file_of.get(from_id.as_str()), file_of.get(to_id.as_str()))
        {
            if ff != tf {
                file_pairs.insert((ff.to_string(), tf.to_string()));
            }
        }
    }

    write_file_refs(db, project_id, &file_pairs).await
}

async fn load_nodes(db: &Surreal<Any>, project_id: &str) -> Result<Vec<ResolverNode>> {
    let mut resp = db
        .query(
            "SELECT node_id, name, node_type, language, file_path, qualified_name \
             FROM code_node WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading nodes for resolution failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let id = extract_str(obj, "node_id");
            if id.is_empty() {
                return None;
            }
            Some(ResolverNode {
                id,
                name: extract_str(obj, "name"),
                node_type: extract_str(obj, "node_type"),
                language: extract_str(obj, "language"),
                file_path: extract_str(obj, "file_path"),
                qualified_name: extract_str(obj, "qualified_name"),
            })
        })
        .collect())
}

async fn load_unresolved_edges(db: &Surreal<Any>, project_id: &str) -> Result<Vec<UnresolvedEdge>> {
    let mut resp = db
        .query(
            "SELECT from_id, to_name, to_type, edge_type FROM code_edge \
             WHERE project_id = $pid AND to_id = ''",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading unresolved edges failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let from_id = extract_str(obj, "from_id");
            if from_id.is_empty() {
                return None;
            }
            Some(UnresolvedEdge {
                from_id,
                to_name: extract_str(obj, "to_name"),
                to_type: extract_str(obj, "to_type"),
                edge_type: extract_str(obj, "edge_type"),
            })
        })
        .collect())
}

/// Load every non-structural edge in the project regardless of current
/// resolution state — unlike `load_unresolved_edges` (`to_id = ''` only),
/// used by the full pass. `resolve_incremental` must be able to re-examine
/// an edge that's already RESOLVED (its target may have been renamed away
/// since), so it can't filter on `to_id` up front. Still excludes
/// `confidence = 'EXTRACTED'` rows (`contains`, same-file macro `calls` —
/// real `to_id` known at parse time, never a resolver candidate — see
/// `parser::ExtractionContext::add_edge`) and derived `file_ref` rows (a
/// different row shape entirely: `from_file`/`to_file`, no
/// `from_id`/`to_name`).
async fn load_all_edges(db: &Surreal<Any>, project_id: &str) -> Result<Vec<UnresolvedEdge>> {
    let mut resp = db
        .query(
            "SELECT from_id, to_name, to_type, edge_type FROM code_edge \
             WHERE project_id = $pid AND confidence != 'EXTRACTED' AND edge_type != 'file_ref'",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading all edges for incremental resolution failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            let from_id = extract_str(obj, "from_id");
            if from_id.is_empty() {
                return None;
            }
            Some(UnresolvedEdge {
                from_id,
                to_name: extract_str(obj, "to_name"),
                to_type: extract_str(obj, "to_type"),
                edge_type: extract_str(obj, "edge_type"),
            })
        })
        .collect())
}

/// One symbol currently stored for a file — the "before" half of the
/// per-file delta `index::index_project` computes. `name` feeds
/// `resolve_incremental`'s `delta_names` exactly as the old names-only
/// loader did; `qualified_name`/`node_type` additionally feed the
/// deletion-tracking capture (design doc §10 option 1), which diffs by
/// qualified name and excludes `import` nodes.
#[derive(Debug, Clone)]
pub(crate) struct FileSymbol {
    pub(crate) node_id: String,
    pub(crate) name: String,
    pub(crate) qualified_name: String,
    pub(crate) node_type: String,
}

/// Every node currently stored for one file — the "before" half of the
/// symbol delta `resolve_incremental` and the deletion-tracking capture
/// both need (the "after" half is just `parsed.nodes`, already in memory
/// post-parse). Must be called *before* that file's old rows are deleted
/// (`index::clean_file_nodes` / `index::store_parsed_file`'s upsert) — once
/// they're gone there's no way to recover what used to be there.
/// `pub(crate)`: called from `index::index_project`, which owns the
/// add/modify/delete sequencing this has to interleave with.
///
/// **Returns every row, including any with an empty `name`, and callers that
/// want named symbols must filter for themselves.** This is deliberate and
/// load-bearing. `index::clean_file_nodes` derives its edge-delete set
/// (`DELETE code_edge WHERE from_id IN $ids`) from these `node_id`s, while
/// the matching node delete is by `file_path` and therefore unconditional.
/// If this function dropped rows, the two deletes would disagree: the node
/// would go and its outgoing edges would survive as orphans pointing at a
/// dead `node_id`, accumulating on every re-index. Filtering here would put
/// that invariant in a different module from the code that depends on it —
/// which is exactly the latent defect (D7) Phase 4 verification flagged.
pub(crate) async fn load_file_symbols(
    db: &Surreal<Any>,
    project_id: &str,
    file_path: &str,
) -> Result<Vec<FileSymbol>> {
    let mut resp = db
        .query(
            "SELECT node_id, name, qualified_name, node_type FROM code_node \
             WHERE project_id = $pid AND file_path = $fp",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("fp", file_path.to_string()))
        .await
        .context("loading existing node names failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    Ok(rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            Some(FileSymbol {
                node_id: extract_str(obj, "node_id"),
                name: extract_str(obj, "name"),
                qualified_name: extract_str(obj, "qualified_name"),
                node_type: extract_str(obj, "node_type"),
            })
        })
        .collect())
}

/// One generation per resolver pass (`resolution_gen`), monotonically
/// increasing project-wide — R4 (incremental re-resolution) will use this
/// to tell freshly-resolved edges apart from stale ones left over from a
/// previous pass.
async fn next_resolution_gen(db: &Surreal<Any>, project_id: &str) -> Result<i64> {
    let mut resp = db
        .query(
            "SELECT resolution_gen FROM code_edge \
             WHERE project_id = $pid AND resolution_gen != NONE",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading current resolution_gen failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    let max_gen = rows
        .iter()
        .filter_map(|v| {
            let surrealdb_types::Value::Object(obj) = v else {
                return None;
            };
            extract_i64(obj, "resolution_gen")
        })
        .max()
        .unwrap_or(0);

    Ok(max_gen + 1)
}

/// Write every binding back in chunks of `WRITE_CHUNK_SIZE` statements per
/// round trip — one already-open `db` session throughout (never reopens
/// the store), batched rather than one query per edge.
///
/// `require_unresolved` gates an extra `AND to_id = ''` on every UPDATE's
/// WHERE clause. `resolve_project`'s full pass only ever loads `to_id = ''`
/// rows to begin with (`load_unresolved_edges`), so the guard is redundant
/// there but harmless (`true`, preserves its exact prior behavior).
/// `resolve_incremental` must be able to flip an edge that's already
/// RESOLVED — e.g. its target got renamed away — so it needs the guard
/// dropped (`false`).
async fn write_updates(
    db: &Surreal<Any>,
    project_id: &str,
    updates: &[EdgeUpdate],
    gen: i64,
    require_unresolved: bool,
) -> Result<()> {
    let guard = if require_unresolved { " AND to_id = ''" } else { "" };
    for chunk in updates.chunks(WRITE_CHUNK_SIZE) {
        let mut sql = String::new();
        for i in 0..chunk.len() {
            sql.push_str(&format!(
                "UPDATE code_edge SET to_id = $to_id{i}, confidence = $conf{i}, \
                 resolved_by = $rb{i}, resolution_gen = $gen, candidates = $cand{i} \
                 WHERE project_id = $pid AND from_id = $from{i} AND to_name = $tname{i} \
                 AND to_type = $ttype{i} AND edge_type = $etype{i}{guard};\n"
            ));
        }

        let mut q = db
            .query(sql)
            .bind(("pid", project_id.to_string()))
            .bind(("gen", gen));
        for (i, u) in chunk.iter().enumerate() {
            q = q.bind((format!("to_id{i}"), u.binding.to_id.clone().unwrap_or_default()));
            q = q.bind((format!("conf{i}"), u.binding.confidence.to_string()));
            q = q.bind((format!("rb{i}"), u.binding.resolved_by.to_string()));
            q = q.bind((format!("cand{i}"), u.binding.candidates.clone()));
            q = q.bind((format!("from{i}"), u.from_id.clone()));
            q = q.bind((format!("tname{i}"), u.to_name.clone()));
            q = q.bind((format!("ttype{i}"), u.to_type.clone()));
            q = q.bind((format!("etype{i}"), u.edge_type.clone()));
        }

        let resp = q.await.context("resolver batch update failed")?;
        resp.check().context("resolver batch update returned an error")?;
    }
    Ok(())
}

/// Fully re-derive `file_ref` for this project: clear whatever a previous
/// pass wrote, then insert the current cross-file RESOLVED pairs. Kept as
/// ordinary rows in the same `code_edge` table (`edge_type = 'file_ref'`,
/// `from_file`/`to_file` — deliberately new field names rather than
/// reusing `from_id`/`to_id`, since a file path isn't a node id and
/// overloading those fields would break every existing node-id-shaped
/// reader) rather than a dedicated table — `code_edge` is already
/// SCHEMALESS and this keeps "one edge-shaped thing, one table,
/// distinguished by `edge_type`" the single pattern the rest of the
/// codebase already follows.
async fn write_file_refs(
    db: &Surreal<Any>,
    project_id: &str,
    pairs: &BTreeSet<(String, String)>,
) -> Result<usize> {
    db.query("DELETE code_edge WHERE project_id = $pid AND edge_type = 'file_ref'")
        .bind(("pid", project_id.to_string()))
        .await
        .context("clearing stale file_ref edges failed")?;

    for (from_file, to_file) in pairs {
        db.query(
            "CREATE code_edge SET edge_type = 'file_ref', from_file = $ff, to_file = $tf, \
             project_id = $pid, confidence = 'RESOLVED'",
        )
        .bind(("pid", project_id.to_string()))
        .bind(("ff", from_file.clone()))
        .bind(("tf", to_file.clone()))
        .await
        .context("creating file_ref edge failed")?;
    }

    Ok(pairs.len())
}

fn extract_str(obj: &surrealdb_types::Object, key: &str) -> String {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn extract_i64(obj: &surrealdb_types::Object, key: &str) -> Option<i64> {
    obj.get(key).and_then(|v| match v {
        surrealdb_types::Value::Number(surrealdb_types::Number::Int(n)) => Some(*n),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, node_type: &str, language: &str, file: &str, qn: &str) -> ResolverNode {
        ResolverNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: node_type.to_string(),
            language: language.to_string(),
            file_path: file.to_string(),
            qualified_name: qn.to_string(),
        }
    }

    fn edge(from: &str, to_name: &str, to_type: &str) -> UnresolvedEdge {
        UnresolvedEdge {
            from_id: from.to_string(),
            to_name: to_name.to_string(),
            to_type: to_type.to_string(),
            edge_type: "calls".to_string(),
        }
    }

    /// Rust fixture's case a: bare same-file call. `to_name` has no
    /// separator, so R1/R2 are skipped entirely even though `log_startup`'s
    /// own `qualified_name` happens to equal it exactly (crate-root item) —
    /// only R3 (same-file) is allowed to claim a bare name.
    #[test]
    fn r3_same_file_bare_name() {
        let nodes = vec![
            node("run", "run", "function", "rust", "src/main.rs", "run"),
            node("log_startup", "log_startup", "function", "rust", "src/main.rs", "log_startup"),
        ];
        let e = edge("run", "log_startup", "function");
        let indices = build_indices(&nodes);
        let b = resolve_one(&e, &nodes, &indices);
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.resolved_by, "r3");
        assert_eq!(b.to_id.as_deref(), Some("log_startup"));
    }

    /// Rust fixture's case b: a call spelled out with its full qualified
    /// path from the crate root exact-matches the target's qualified_name.
    #[test]
    fn r1_exact_qualified_match() {
        let nodes = vec![
            node("run", "run", "function", "rust", "src/main.rs", "run"),
            node(
                "connect",
                "connect",
                "function",
                "rust",
                "src/db/connection.rs",
                "db::connection::connect",
            ),
        ];
        let e = edge("run", "db::connection::connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r1");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("connect"));
    }

    /// Rust fixture's bonus-r2 case: importing the *module* means the call
    /// text is a shorter qualified suffix, not an exact match.
    #[test]
    fn r2_qualified_suffix_match() {
        let nodes = vec![
            node("caller", "call_connect_via_module_import", "function", "rust", "src/delta.rs", "delta::call_connect_via_module_import"),
            node(
                "connect",
                "connect",
                "function",
                "rust",
                "src/db/connection.rs",
                "db::connection::connect",
            ),
        ];
        let e = edge("caller", "connection::connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r2");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("connect"));
    }

    /// R2m: a `crate::`-prefixed Rust call binds the crate-relative target
    /// once the module prefix is stripped. Before this correction it went
    /// UNRESOLVED — `qualified_name`s carry no `crate` segment, so R1/R2's
    /// literal test never matched. Production shape: `crate::db::connect`.
    #[test]
    fn r2m_crate_prefix_exact_match() {
        let nodes = vec![
            node("caller", "run", "function", "rust", "src/main.rs", "run"),
            node("connect", "connect", "function", "rust", "src/db.rs", "db::connect"),
        ];
        let e = edge("caller", "crate::db::connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r2m");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("connect"));
    }

    /// R2m via `super::`: the stripped remainder resolves by qualified suffix
    /// against a deeper crate-relative `qualified_name`. Production shape:
    /// `super::load_project_edges`.
    #[test]
    fn r2m_super_prefix_suffix_match() {
        let nodes = vec![
            node("caller", "helper", "function", "rust", "src/mcp/loader.rs", "mcp::loader::helper"),
            node("target", "load_project_edges", "function", "rust", "src/mcp/mod.rs", "mcp::load_project_edges"),
        ];
        let e = edge("caller", "super::load_project_edges", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r2m");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("target"));
    }

    /// R2m must NOT manufacture a binding when the stripped remainder is
    /// non-unique — two crate-relative candidates means it falls through to
    /// the unchanged R1∪R2 terminal (UNRESOLVED here, since the literal
    /// `crate::`-prefixed name matches neither `qualified_name`), never a
    /// silent wrong pick.
    #[test]
    fn r2m_stays_unresolved_when_module_relative_not_unique() {
        let nodes = vec![
            node("caller", "run", "function", "rust", "src/main.rs", "run"),
            node("a", "connect", "function", "rust", "src/a/db.rs", "a::db::connect"),
            node("b", "connect", "function", "rust", "src/b/db.rs", "b::db::connect"),
        ];
        // strips to `db::connect`; both candidates suffix-match → non-unique.
        let e = edge("caller", "crate::db::connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED");
        assert_eq!(b.to_id, None);
    }

    /// A qualified `to_name` whose bare tail matches a node but whose
    /// prefix doesn't must NOT fall back to bare-tail matching at all — a
    /// qualified name is R1/R2's exclusive territory; failing both there
    /// terminates immediately as UNRESOLVED, even though "connect" alone
    /// would otherwise be project-unique. Regression test for a real bug
    /// found self-indexing this repo: see `qualified_name_never_falls_back_to_bare_tail_r5`
    /// and `qualified_name_never_falls_back_to_bare_tail_ambiguity` below for
    /// the exact production shape (`HashMap::new`, `cursor.node`) this
    /// generalizes.
    #[test]
    fn r2_requires_full_suffix_not_just_bare_tail() {
        let nodes = vec![
            node("caller", "caller", "function", "rust", "src/x.rs", "x::caller"),
            node("only", "connect", "function", "rust", "src/other.rs", "other::connect"),
        ];
        let e = edge("caller", "nonexistent::connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED");
        assert_eq!(b.to_id, None);
    }

    /// Same shape as above, but with TWO same-named candidates — a
    /// mismatched-prefix qualified call must stay UNRESOLVED here too, not
    /// become AMBIGUOUS against candidates its own qualification already
    /// ruled out.
    #[test]
    fn r2_mismatched_prefix_stays_unresolved_when_not_unique_either() {
        let nodes = vec![
            node("caller", "caller", "function", "rust", "src/x.rs", "x::caller"),
            node("a", "connect", "function", "rust", "src/a.rs", "a::connect"),
            node("b", "connect", "function", "rust", "src/b.rs", "b::connect"),
        ];
        let e = edge("caller", "nonexistent::connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED");
    }

    /// Regression test, production shape: self-indexing this very repo
    /// found `HashMap::new()` (a stdlib call — `to_name` normalizes to the
    /// qualified `HashMap::new`) incorrectly RESOLVED to this codebase's
    /// own `CodegraphServer::new`, because "new" alone happened to be
    /// project-unique and the old cascade let a qualified name that failed
    /// R1/R2 fall back to R5's bare-tail check anyway. Must be UNRESOLVED.
    #[test]
    fn qualified_name_never_falls_back_to_bare_tail_r5() {
        let nodes = vec![
            node("caller", "extract", "function", "rust", "src/index/extractors/go.rs", "index::extractors::go::extract"),
            node(
                "new_fn",
                "new",
                "function",
                "rust",
                "src/mcp/server.rs",
                "mcp::server::CodegraphServer::new",
            ),
        ];
        let e = edge("caller", "HashMap::new", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED", "HashMap::new must not bind to an unrelated project-unique `new`");
        assert_eq!(b.to_id, None);
    }

    /// Regression test, production shape: self-indexing this repo found
    /// `cursor.node()` (tree-sitter's `TreeCursor::node()`, `to_name`
    /// normalizes to qualified `cursor::node`) incorrectly flagged
    /// AMBIGUOUS against two unrelated project functions bare-named `node`,
    /// again via the same bare-tail fallback. Must be UNRESOLVED, not
    /// AMBIGUOUS.
    #[test]
    fn qualified_name_never_falls_back_to_bare_tail_ambiguity() {
        let nodes = vec![
            node("caller", "walk", "function", "rust", "src/index/extractors/go.rs", "index::extractors::go::walk"),
            node("node_fn_a", "node", "function", "rust", "src/a.rs", "a::node"),
            node("node_fn_b", "node", "function", "rust", "src/b.rs", "b::node"),
        ];
        let e = edge("caller", "cursor.node", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED", "cursor.node() must not be flagged ambiguous against unrelated same-bare-name project functions");
        assert_eq!(b.candidates, None);
    }

    /// R4 named-import variant (rust fixture's bonus-r4-named-import):
    /// `helper` collides project-wide, but a `use crate::alpha::helper;`
    /// in the caller's file narrows it before R5/R6 are ever reached.
    #[test]
    fn r4_named_import_narrows_collision() {
        let nodes = vec![
            node("caller", "call_helper_via_import", "function", "rust", "src/delta.rs", "delta::call_helper_via_import"),
            node("import1", "use crate::alpha::helper;", "import", "rust", "src/delta.rs", "delta::use crate::alpha::helper;"),
            node("alpha_helper", "helper", "function", "rust", "src/alpha.rs", "alpha::helper"),
            node("beta_helper", "helper", "function", "rust", "src/beta.rs", "beta::helper"),
        ];
        let e = edge("caller", "helper", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r4");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("alpha_helper"));
    }

    /// R4 glob-import variant (bonus-r4-glob-import): reached via
    /// `use crate::alpha::*;` rather than a named import.
    #[test]
    fn r4_glob_import_narrows_to_direct_child() {
        let nodes = vec![
            node("caller", "dispatch_unique", "function", "rust", "src/gamma.rs", "gamma::dispatch_unique"),
            node("import1", "use crate::alpha::*;", "import", "rust", "src/gamma.rs", "gamma::use crate::alpha::*;"),
            node("import2", "use crate::beta::*;", "import", "rust", "src/gamma.rs", "gamma::use crate::beta::*;"),
            node("alpha_unique", "unique_alpha_fn", "function", "rust", "src/alpha.rs", "alpha::unique_alpha_fn"),
        ];
        let e = edge("caller", "unique_alpha_fn", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r4");
        assert_eq!(b.to_id.as_deref(), Some("alpha_unique"));
    }

    /// A glob import that ALSO matches doesn't help resolve the actual
    /// collision case (both alpha and beta glob-import `helper` into
    /// gamma.rs) — R4 must still see 2 candidates and fall through, ending
    /// at R6/AMBIGUOUS exactly like a resolver with no import facts at
    /// all would (rust fixture's case c).
    #[test]
    fn r4_does_not_falsely_resolve_a_genuine_collision() {
        let nodes = vec![
            node("caller", "dispatch", "function", "rust", "src/gamma.rs", "gamma::dispatch"),
            node("import1", "use crate::alpha::*;", "import", "rust", "src/gamma.rs", "gamma::use crate::alpha::*;"),
            node("import2", "use crate::beta::*;", "import", "rust", "src/gamma.rs", "gamma::use crate::beta::*;"),
            node("alpha_helper", "helper", "function", "rust", "src/alpha.rs", "alpha::helper"),
            node("beta_helper", "helper", "function", "rust", "src/beta.rs", "beta::helper"),
        ];
        let e = edge("caller", "helper", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "AMBIGUOUS");
        assert_eq!(b.resolved_by, "r6");
        assert_eq!(
            b.candidates,
            Some(vec!["alpha_helper".to_string(), "beta_helper".to_string()])
        );
    }

    /// R5: project-unique bare name, no same-file/import help at all —
    /// same shape as the c-cpp fixture's bonus-r5 (legacy helper, called
    /// cross-file, no namespace/import mechanism available), kept
    /// same-language here (both "c") to isolate plain R5 from the
    /// cross-family merge — see `c_and_cpp_share_a_pool_bonus_r5` below for
    /// the cross-family (`.cpp` caller / `.c` callee) variant.
    #[test]
    fn r5_project_unique_bare_name() {
        let nodes = vec![
            node("main_fn", "main", "function", "c", "main.c", "main::main"),
            node("legacy_helper", "legacy_helper", "function", "c", "legacy.c", "legacy::legacy_helper"),
        ];
        let e = edge("main_fn", "legacy_helper", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r5");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("legacy_helper"));
    }

    /// c-cpp fixture's actual `bonus-r5` case, adjudicated (team-lead
    /// ruling): `{c, cpp}` is one of exactly two closed-table family
    /// merges in `language_family` — a `.cpp` caller DOES reach a `.c`
    /// callee via R5, since real `extern "C"` linkage means they share one
    /// linker namespace in deployed reality. Pinned directly against the
    /// fixture's own shape (main.cpp calling legacy_helper, defined in
    /// legacy.c) so this can't silently regress again.
    #[test]
    fn c_and_cpp_share_a_pool_bonus_r5() {
        let nodes = vec![
            node("main_fn", "main", "function", "cpp", "main.cpp", "main::main"),
            node("legacy_helper", "legacy_helper", "function", "c", "legacy.c", "legacy::legacy_helper"),
        ];
        let e = edge("main_fn", "legacy_helper", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.resolved_by, "r5");
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.to_id.as_deref(), Some("legacy_helper"));
    }

    /// The project-wide `walk_calls` collision this spec was written
    /// against directly (D3, verified on codegraph's own source: the name
    /// repeats across all 6 extractor files) — must come back AMBIGUOUS
    /// with every same-named, same-typed, same-language candidate, never
    /// blended down to one.
    #[test]
    fn six_way_collision_is_ambiguous_with_all_six_candidates() {
        let files = [
            "index/extractors/rust.rs",
            "index/extractors/go.rs",
            "index/extractors/python.rs",
            "index/extractors/typescript.rs",
            "index/extractors/java.rs",
            "index/extractors/c_cpp.rs",
        ];
        let mut nodes = vec![node("caller", "extract_calls", "function", "rust", "index/mod.rs", "index::mod::extract_calls")];
        let mut expected_ids = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let id = format!("walk_calls_{i}");
            nodes.push(node(&id, "walk_calls", "function", "rust", f, &format!("{f}::walk_calls")));
            expected_ids.push(id);
        }
        expected_ids.sort();

        let e = edge("caller", "walk_calls", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "AMBIGUOUS");
        assert_eq!(b.resolved_by, "r6");
        assert_eq!(b.candidates, Some(expected_ids));
    }

    /// polyglot fixture's case c: a bare-name collision between two
    /// DIFFERENT languages must never blend — R5's pool is scoped by
    /// language family, so this resolves cleanly within Python alone.
    #[test]
    fn cross_language_collision_does_not_blend() {
        let nodes = vec![
            node("py_run", "run", "function", "python", "api/consumer.py", "consumer::run"),
            node("py_connect", "connect", "function", "python", "api/app.py", "app::connect"),
            node("go_connect", "connect", "function", "go", "worker/main.go", "worker::connect"),
        ];
        let e = edge("py_run", "connect", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "RESOLVED");
        assert_eq!(b.resolved_by, "r5");
        assert_eq!(b.to_id.as_deref(), Some("py_connect"));
    }

    /// case e: a call to an external/stdlib symbol never defined in the
    /// project terminates as UNRESOLVED, not an error, and with no
    /// candidates.
    #[test]
    fn unresolved_external_call_has_no_candidates() {
        let nodes = vec![node("run", "run", "function", "rust", "src/main.rs", "run")];
        let e = edge("run", "std::env::args", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED");
        assert_eq!(b.resolved_by, "r6");
        assert_eq!(b.candidates, None);
        assert_eq!(b.to_id, None);
    }

    /// An edge whose source node isn't in the project's node set at all
    /// (shouldn't happen from a real extractor pass) fails safe instead
    /// of panicking.
    #[test]
    fn orphaned_edge_fails_safe() {
        let nodes = vec![node("other", "other", "function", "rust", "src/x.rs", "x::other")];
        let e = edge("missing", "other", "function");
        let b = resolve_one(&e, &nodes, &build_indices(&nodes));
        assert_eq!(b.confidence, "UNRESOLVED");
        assert_eq!(b.to_id, None);
    }

    /// Determinism property: shuffling node AND edge insertion order must
    /// never change any edge's binding. Re-associates results after the
    /// shuffle via each edge's own (from_id, to_name) identity, since
    /// positions move.
    /// Tiny deterministic xorshift64 PRNG, used only to drive the
    /// Fisher-Yates shuffle below — no crate dependency needed (and no
    /// test flakiness: every seed is fixed) for what's otherwise the same
    /// property a `rand`-based shuffle would give.
    fn xorshift64(seed: &mut u64) -> u64 {
        let mut x = *seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *seed = x;
        x
    }

    fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
        let mut v = items.to_vec();
        let mut state = seed.max(1); // xorshift64 is fixed at 0
        for i in (1..v.len()).rev() {
            let j = (xorshift64(&mut state) as usize) % (i + 1);
            v.swap(i, j);
        }
        v
    }

    /// Determinism property: shuffling node AND edge insertion order must
    /// never change any edge's binding. Re-associates results after the
    /// shuffle via each edge's own (from_id, to_name) identity, since
    /// positions move.
    #[test]
    fn determinism_under_shuffled_insertion_order() {
        let base_nodes = vec![
            node("run", "run", "function", "rust", "src/main.rs", "run"),
            node("log_startup", "log_startup", "function", "rust", "src/main.rs", "log_startup"),
            node("alpha_helper", "helper", "function", "rust", "src/alpha.rs", "alpha::helper"),
            node("beta_helper", "helper", "function", "rust", "src/beta.rs", "beta::helper"),
            node("connect", "connect", "function", "rust", "src/db/connection.rs", "db::connection::connect"),
            node("dispatch", "dispatch", "function", "rust", "src/gamma.rs", "gamma::dispatch"),
        ];
        let base_edges = vec![
            edge("run", "log_startup", "function"),
            edge("run", "db::connection::connect", "function"),
            edge("run", "std::env::args", "function"),
            edge("dispatch", "helper", "function"),
        ];

        let baseline: HashMap<(String, String), Binding> = {
            let indices = build_indices(&base_nodes);
            base_edges
                .iter()
                .map(|e| ((e.from_id.clone(), e.to_name.clone()), resolve_one(e, &base_nodes, &indices)))
                .collect()
        };

        for seed in 1u64..=20 {
            let nodes = shuffled(&base_nodes, seed);
            let edges = shuffled(&base_edges, seed.wrapping_mul(0x9E37_79B9));

            let indices = build_indices(&nodes);
            for e in &edges {
                let got = resolve_one(e, &nodes, &indices);
                let want = &baseline[&(e.from_id.clone(), e.to_name.clone())];
                assert_eq!(&got, want, "binding changed under shuffled insertion order for {e:?}");
            }
        }
    }

    // -- Rust `use` parsing ------------------------------------------------

    #[test]
    fn parses_named_use_stripping_crate_prefix() {
        let facts = parse_rust_use("use crate::alpha::helper;");
        assert_eq!(facts, vec![ImportFact { path: "alpha::helper".to_string(), glob: false }]);
    }

    #[test]
    fn parses_module_use() {
        let facts = parse_rust_use("use crate::db::connection;");
        assert_eq!(facts, vec![ImportFact { path: "db::connection".to_string(), glob: false }]);
    }

    #[test]
    fn parses_glob_use() {
        let facts = parse_rust_use("use crate::alpha::*;");
        assert_eq!(facts, vec![ImportFact { path: "alpha".to_string(), glob: true }]);
    }

    #[test]
    fn parses_grouped_use_into_multiple_facts() {
        let mut facts = parse_rust_use("use crate::foo::{bar, baz};");
        facts.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            facts,
            vec![
                ImportFact { path: "foo::bar".to_string(), glob: false },
                ImportFact { path: "foo::baz".to_string(), glob: false },
            ]
        );
    }

    #[test]
    fn parses_nested_grouped_use() {
        let mut facts = parse_rust_use("use crate::foo::{bar::{a, b}, baz};");
        facts.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(
            facts,
            vec![
                ImportFact { path: "foo::bar::a".to_string(), glob: false },
                ImportFact { path: "foo::bar::b".to_string(), glob: false },
                ImportFact { path: "foo::baz".to_string(), glob: false },
            ]
        );
    }

    #[test]
    fn pub_use_is_parsed_like_plain_use() {
        let facts = parse_rust_use("pub use crate::alpha::helper;");
        assert_eq!(facts, vec![ImportFact { path: "alpha::helper".to_string(), glob: false }]);
    }

    #[test]
    fn aliased_import_matches_by_real_path_not_alias() {
        let facts = parse_rust_use("use crate::alpha::helper as aliased;");
        assert_eq!(facts, vec![ImportFact { path: "alpha::helper".to_string(), glob: false }]);
    }

    #[test]
    fn import_fact_glob_matches_only_direct_children() {
        let fact = ImportFact { path: "alpha".to_string(), glob: true };
        assert!(fact.matches("alpha::helper"));
        assert!(!fact.matches("alpha::nested::deep")); // not a direct child
        assert!(!fact.matches("beta::helper"));
    }

    #[test]
    fn import_fact_named_matches_exact_or_suffix() {
        let fact = ImportFact { path: "alpha::helper".to_string(), glob: false };
        assert!(fact.matches("alpha::helper"));
        assert!(fact.matches("myapp::alpha::helper"));
        assert!(!fact.matches("alpha::other_helper"));
    }
}
