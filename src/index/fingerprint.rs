//! Per-symbol structural fingerprints, persisted at index time.
//!
//! For every definition-bearing symbol node, extract its 1-hop structural
//! neighborhood — the typed subgraph of edges incident to it — and hash the
//! canonical certificate of that neighborhood with the CENTER vertex
//! individualized (`canon::canonical` + `canon::certificate_hash`, the same
//! I-R kernel proven in the bend-multiway spike). Names never enter the
//! fingerprint — structure, edge types, and node kinds only — so a pure
//! rename (definition + callers updated together) preserves every hash.
//! That is the entire point: fingerprint deltas separate structure-neutral
//! refactors from structural change (`facade::structural_delta_for_paths`).
//!
//! DESIGN DECISIONS (owned here, per the lane brief):
//!
//! * Which edges participate: `calls` / `member_of` / `implements` /
//!   `contains` rows whose `to_id` is materialized — i.e. `contains` and
//!   same-file EXTRACTED edges (real target known at parse time) plus
//!   name-edges the R1 resolver bound to RESOLVED. AMBIGUOUS and UNRESOLVED
//!   edges are excluded: their targets are guesses or external noise
//!   (stdlib calls, in-flight renames), and including them would make
//!   fingerprints churn with resolution weather rather than structure.
//!   `imports` and derived `file_ref` edges are excluded too — references /
//!   file-level aggregates, not symbol structure.
//! * Which nodes are centers: every node except `import` and `macro_call` —
//!   both are reference SITES, not definitions (the same import exclusion
//!   deletion tracking and the facade apply). Excluded kinds still appear as
//!   *neighbors* in other centers' neighborhoods, where they are structure.
//! * Neighbors contribute their kind as seed color; the center carries a
//!   distinguished class-0 color so the certificate is rooted at it.
//! * Edge multiplicity is kept: calling a helper twice is structurally
//!   different from calling it once.
//! * Row key is `(qualified_name, file_path)`, exactly like deletion
//!   tracking. Distinct nodes can legally collide on that key (a Rust
//!   `struct Foo` + `impl Foo` both map to `module::Foo`); such a key gets
//!   ONE row whose hash is FNV-1a over the SORTED multiset of the member
//!   hashes — order-free and rename-invariant, since each member hash is.
//! * Every index run recomputes the whole project's fingerprints from the
//!   post-resolution graph and shifts `hash` → `prev_hash` (one prior
//!   generation, per the brief). Correctness-first: a 1-hop neighborhood
//!   can change through edits in OTHER files (a new caller), so a targeted
//!   incremental recompute would need the resolver's two-sided affected-set
//!   analysis for marginal savings — the pure pass is in-memory and cheap
//!   relative to parsing. A symbol that disappears leaves a one-generation
//!   tombstone (`hash = NONE`, `prev_hash` = its last value) so deltas can
//!   classify removals and pair renames; tombstones self-clean on the next
//!   run. `--force` wipes the table first — the same spec'd degradation as
//!   `deleted_symbol` (a force run re-baselines, history resets).

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use surrealdb_types::SurrealValue;

use crate::canon::{self, Color, TypedEdge, TypedGraph, Vertex};
use crate::graph::{self, QueryEdge};
use crate::index::resolve::ResolverNode;

/// Edge types a fingerprint neighborhood may draw from (see module docs for
/// the inclusion rule applied on top: `to_id` must be materialized).
pub const FINGERPRINT_EDGE_TYPES: [&str; 4] = ["calls", "member_of", "implements", "contains"];

/// Node kinds that never get their own fingerprint row: reference sites,
/// not definitions.
const NON_DEFINITION_KINDS: [&str; 2] = ["import", "macro_call"];

/// One computed fingerprint, keyed like deletion tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolFingerprint {
    pub qualified_name: String,
    pub file_path: String,
    /// Distinct member kinds under this key, `+`-joined in sorted order
    /// (usually a single kind; `impl+struct` for a Rust struct with an
    /// impl block in the same file).
    pub node_type: String,
    pub hash: u64,
    /// Total neighborhood edges (summed across key-colliding members).
    pub edge_count: usize,
    /// Neighborhood edges that are NOT `contains` — the count clone
    /// detection uses to reject pure-containment shapes.
    pub name_edge_count: usize,
}

/// Pure core: fingerprints for every definition-bearing node, from the
/// post-resolution node/edge sets. Deterministic: output sorted by key.
pub fn compute_fingerprints(nodes: &[ResolverNode], edges: &[QueryEdge]) -> Vec<SymbolFingerprint> {
    let node_ix: HashMap<&str, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

    // Included edges: type-filtered by the caller's load, target
    // materialized, both endpoints alive in this project's node set.
    let included: Vec<(usize, usize, &str)> = edges
        .iter()
        .filter(|e| !e.to_id.is_empty() && !e.from_id.is_empty())
        .filter_map(|e| {
            let f = *node_ix.get(e.from_id.as_str())?;
            let t = *node_ix.get(e.to_id.as_str())?;
            Some((f, t, e.edge_type.as_str()))
        })
        .collect();

    // node index -> indices into `included` (each incident edge once, even
    // for self-loops).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (ei, &(f, t, _)) in included.iter().enumerate() {
        adj[f].push(ei);
        if t != f {
            adj[t].push(ei);
        }
    }

    let kind_color = |node_type: &str| canon::fnv1a64(node_type.as_bytes());

    // Per-key member hashes, grouped for the collision-combining rule:
    // (hash, edge_count, name_edge_count, node_type) per member node.
    type Member = (u64, usize, usize, String);
    let mut by_key: BTreeMap<(String, String), Vec<Member>> = BTreeMap::new();

    for (ci, center) in nodes.iter().enumerate() {
        if NON_DEFINITION_KINDS.contains(&center.node_type.as_str()) {
            continue;
        }

        // Build the rooted 1-hop typed neighborhood.
        let mut vid: HashMap<usize, Vertex> = HashMap::new();
        let mut colors: BTreeMap<Vertex, Color> = BTreeMap::new();
        let mut next: Vertex = 0;
        let mut intern = |ix: usize, vid: &mut HashMap<usize, Vertex>, colors: &mut BTreeMap<Vertex, Color>| {
            *vid.entry(ix).or_insert_with(|| {
                let v = next;
                next += 1;
                let class = if ix == ci { 0 } else { 1 };
                colors.insert(v, (class, kind_color(&nodes[ix].node_type)));
                v
            })
        };
        // The center always exists, even with an empty neighborhood — a
        // leaf symbol's fingerprint is then just its own rooted kind.
        intern(ci, &mut vid, &mut colors);

        let mut tedges: Vec<TypedEdge> = Vec::with_capacity(adj[ci].len());
        let mut name_edges = 0usize;
        for &ei in &adj[ci] {
            let (f, t, etype) = included[ei];
            let fv = intern(f, &mut vid, &mut colors);
            let tv = intern(t, &mut vid, &mut colors);
            tedges.push(TypedEdge {
                label: canon::fnv1a64(etype.as_bytes()),
                verts: vec![fv, tv],
            });
            if etype != "contains" {
                name_edges += 1;
            }
        }
        let edge_count = tedges.len();

        let hash = canon::certificate_hash(&canon::canonical(&TypedGraph { edges: tedges, colors }));

        by_key
            .entry((center.qualified_name.clone(), center.file_path.clone()))
            .or_default()
            .push((hash, edge_count, name_edges, center.node_type.clone()));
    }

    by_key
        .into_iter()
        .map(|((qualified_name, file_path), mut members)| {
            members.sort();
            let hash = if members.len() == 1 {
                members[0].0
            } else {
                // Key collision (e.g. struct + impl): FNV over the sorted
                // member-hash multiset — order-free, rename-invariant.
                let mut bytes = Vec::with_capacity(8 * (members.len() + 1));
                bytes.extend_from_slice(&(members.len() as u64).to_le_bytes());
                for &(h, _, _, _) in &members {
                    bytes.extend_from_slice(&h.to_le_bytes());
                }
                canon::fnv1a64(&bytes)
            };
            let edge_count = members.iter().map(|m| m.1).sum();
            let name_edge_count = members.iter().map(|m| m.2).sum();
            let mut kinds: Vec<&str> = members.iter().map(|m| m.3.as_str()).collect();
            kinds.sort_unstable();
            kinds.dedup();
            SymbolFingerprint {
                qualified_name,
                file_path,
                node_type: kinds.join("+"),
                hash,
                edge_count,
                name_edge_count,
            }
        })
        .collect()
}

/// `u64` fingerprint ⇄ the fixed-width hex form persisted in SurrealDB
/// (strings sidestep any i64-signedness bending of the top bit).
pub fn hash_to_hex(h: u64) -> String {
    format!("{h:016x}")
}

// ============================================================================
// SurrealDB adapter
// ============================================================================

/// Outcome of one fingerprint pass, reported alongside `ResolveStats`.
#[derive(Debug, Default, Clone, Copy)]
pub struct FingerprintStats {
    /// Live symbol keys fingerprinted this run.
    pub symbols: usize,
    /// Keys whose hash differs from the previous generation.
    pub changed: usize,
    /// Keys with no previous generation (new symbols, or everything after
    /// a `--force` wipe).
    pub added: usize,
    /// Keys tombstoned this run (symbol disappeared from the graph).
    pub removed: usize,
    /// The generation stamp written this run.
    pub generation: i64,
}

/// One `fingerprint` row for a bulk `INSERT` (see `src/schema.surql`).
/// `updated_at` is intentionally absent — its schema DEFAULT fills it.
#[derive(SurrealValue)]
struct FingerprintRow {
    project_id: String,
    qualified_name: String,
    file_path: String,
    node_type: String,
    hash: Option<String>,
    edge_count: i64,
    name_edge_count: i64,
    generation: i64,
    prev_hash: Option<String>,
    prev_generation: Option<i64>,
}

#[derive(Debug)]
struct ExistingRow {
    node_type: String,
    hash: Option<String>,
    generation: i64,
}

const WRITE_CHUNK_SIZE: usize = 250;

/// Recompute and persist the whole project's fingerprints — step 5b of
/// `index::index_project`, after the resolver pass (fingerprints read
/// RESOLVED bindings). Shifts every row's `hash` into `prev_hash` (the one
/// retained prior generation), tombstones disappeared symbols for exactly
/// one generation, and on `force` starts history from scratch (the spec'd
/// degradation — matching `deleted_symbol`'s force wipe).
pub async fn update_fingerprints(
    db: &Surreal<Any>,
    project_id: &str,
    force: bool,
) -> Result<FingerprintStats> {
    let nodes = graph::load_project_nodes(db, project_id).await?;
    let edges = graph::load_project_edges(db, project_id, &FINGERPRINT_EDGE_TYPES).await?;
    let computed = compute_fingerprints(&nodes, &edges);

    let mut existing: BTreeMap<(String, String), ExistingRow> = if force {
        BTreeMap::new() // force re-baselines: no history carried across
    } else {
        load_existing(db, project_id).await?
    };

    let generation = 1 + existing.values().map(|r| r.generation).max().unwrap_or(0);

    let mut stats = FingerprintStats { symbols: computed.len(), generation, ..Default::default() };
    let mut rows: Vec<FingerprintRow> = Vec::with_capacity(computed.len());

    for fp in &computed {
        let key = (fp.qualified_name.clone(), fp.file_path.clone());
        let hex = hash_to_hex(fp.hash);
        let (prev_hash, prev_generation) = match existing.remove(&key) {
            Some(old) => (old.hash, Some(old.generation)),
            None => (None, None),
        };
        match &prev_hash {
            None => stats.added += 1,
            Some(h) if *h != hex => stats.changed += 1,
            _ => {}
        }
        rows.push(FingerprintRow {
            project_id: project_id.to_string(),
            qualified_name: fp.qualified_name.clone(),
            file_path: fp.file_path.clone(),
            node_type: fp.node_type.clone(),
            hash: Some(hex),
            edge_count: fp.edge_count as i64,
            name_edge_count: fp.name_edge_count as i64,
            generation,
            prev_hash,
            prev_generation,
        });
    }

    // Whatever's left in `existing` is no longer defined. Rows that were
    // still live become one-generation tombstones; rows that were ALREADY
    // tombstones (hash NONE) have now been gone two runs and drop out —
    // self-cleaning, and exactly the "one prior generation" retention spec.
    for ((qualified_name, file_path), old) in existing {
        let Some(last_hash) = old.hash else { continue };
        stats.removed += 1;
        rows.push(FingerprintRow {
            project_id: project_id.to_string(),
            qualified_name,
            file_path,
            node_type: old.node_type,
            hash: None,
            edge_count: 0,
            name_edge_count: 0,
            generation,
            prev_hash: Some(last_hash),
            prev_generation: Some(old.generation),
        });
    }

    // Delete-and-reinsert, the resolver's write pattern: every surviving
    // row's generation shifted anyway, so a wholesale rewrite is both the
    // simplest and the cheapest shape on surrealkv (INSERT appends beat
    // per-row UPDATEs ~3.5×, see resolve.rs).
    db.query("DELETE fingerprint WHERE project_id = $pid")
        .bind(("pid", project_id.to_string()))
        .await
        .context("fingerprint wipe failed")?
        .check()
        .context("fingerprint wipe validation failed")?;

    for chunk in rows.chunks(WRITE_CHUNK_SIZE) {
        // `chunks` has no owned iterator; a per-chunk Vec is the price of
        // the bulk bind.
        let owned: Vec<FingerprintRow> = chunk
            .iter()
            .map(|r| FingerprintRow {
                project_id: r.project_id.clone(),
                qualified_name: r.qualified_name.clone(),
                file_path: r.file_path.clone(),
                node_type: r.node_type.clone(),
                hash: r.hash.clone(),
                edge_count: r.edge_count,
                name_edge_count: r.name_edge_count,
                generation: r.generation,
                prev_hash: r.prev_hash.clone(),
                prev_generation: r.prev_generation,
            })
            .collect();
        db.query("INSERT INTO fingerprint $rows")
            .bind(("rows", owned))
            .await
            .context("fingerprint bulk insert failed")?
            .check()
            .context("fingerprint bulk insert validation failed")?;
    }

    tracing::debug!(
        symbols = stats.symbols,
        changed = stats.changed,
        added = stats.added,
        removed = stats.removed,
        generation = stats.generation,
        "fingerprint pass complete"
    );
    Ok(stats)
}

async fn load_existing(
    db: &Surreal<Any>,
    project_id: &str,
) -> Result<BTreeMap<(String, String), ExistingRow>> {
    let mut resp = db
        .query(
            "SELECT qualified_name, file_path, node_type, hash, generation \
             FROM fingerprint WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading existing fingerprints failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    let mut out = BTreeMap::new();
    for v in &rows {
        let surrealdb_types::Value::Object(obj) = v else { continue };
        let get_str = |key: &str| {
            obj.get(key)
                .and_then(|v| match v {
                    surrealdb_types::Value::String(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let generation = obj
            .get("generation")
            .and_then(|v| match v {
                surrealdb_types::Value::Number(n) => n.to_int(),
                _ => None,
            })
            .unwrap_or(0);
        let hash = match get_str("hash") {
            h if h.is_empty() => None, // NONE in the DB (tombstone)
            h => Some(h),
        };
        out.insert(
            (get_str("qualified_name"), get_str("file_path")),
            ExistingRow { node_type: get_str("node_type"), hash, generation },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, kind: &str, file: &str, qn: &str) -> ResolverNode {
        ResolverNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: kind.to_string(),
            language: "rust".to_string(),
            file_path: file.to_string(),
            qualified_name: qn.to_string(),
        }
    }

    fn edge(from: &str, to: &str, etype: &str, confidence: &str) -> QueryEdge {
        QueryEdge {
            from_id: from.to_string(),
            to_id: to.to_string(),
            to_name: String::new(),
            to_type: String::new(),
            edge_type: etype.to_string(),
            confidence: confidence.to_string(),
            candidates: Vec::new(),
        }
    }

    fn hash_of<'a>(fps: &'a [SymbolFingerprint], qn: &str) -> &'a SymbolFingerprint {
        fps.iter()
            .find(|f| f.qualified_name == qn)
            .unwrap_or_else(|| panic!("no fingerprint for {qn} in {fps:?}"))
    }

    /// THE property: a pure rename (ids, names, qualified names all change;
    /// structure and kinds identical) preserves every hash.
    #[test]
    fn pure_rename_preserves_hashes() {
        let before_nodes = vec![
            node("n1", "foo", "function", "src/a.rs", "a::foo"),
            node("n2", "caller", "function", "src/b.rs", "b::caller"),
            node("n3", "other", "function", "src/b.rs", "b::other"),
        ];
        let before_edges = vec![
            edge("n2", "n1", "calls", "RESOLVED"),
            edge("n3", "n1", "calls", "RESOLVED"),
        ];
        let after_nodes = vec![
            node("x9", "bar", "function", "src/a.rs", "a::bar"),
            node("x7", "caller", "function", "src/b.rs", "b::caller"),
            node("x8", "other", "function", "src/b.rs", "b::other"),
        ];
        let after_edges = vec![
            edge("x7", "x9", "calls", "RESOLVED"),
            edge("x8", "x9", "calls", "RESOLVED"),
        ];

        let before = compute_fingerprints(&before_nodes, &before_edges);
        let after = compute_fingerprints(&after_nodes, &after_edges);

        assert_eq!(
            hash_of(&before, "a::foo").hash,
            hash_of(&after, "a::bar").hash,
            "a pure rename must not move the target's fingerprint"
        );
        assert_eq!(hash_of(&before, "b::caller").hash, hash_of(&after, "b::caller").hash);
        // …and adding a call is structural: hash must move.
        let mut grown = after_edges.clone();
        grown.push(edge("x9", "x8", "calls", "RESOLVED"));
        let grown_fps = compute_fingerprints(&after_nodes, &grown);
        assert_ne!(
            hash_of(&after, "a::bar").hash,
            hash_of(&grown_fps, "a::bar").hash,
            "an added call is structure — the fingerprint must change"
        );
    }

    /// Unresolved/ambiguous edges (empty to_id) must not enter — they are
    /// external noise and would churn fingerprints without structural change.
    #[test]
    fn unmaterialized_edges_are_excluded() {
        let nodes = vec![node("n1", "f", "function", "src/a.rs", "a::f")];
        let quiet = compute_fingerprints(&nodes, &[]);
        let noisy = compute_fingerprints(
            &nodes,
            &[edge("n1", "", "calls", "UNRESOLVED"), edge("n1", "", "calls", "AMBIGUOUS")],
        );
        assert_eq!(quiet, noisy, "edges without a materialized to_id must be invisible");
    }

    /// import / macro_call nodes are reference sites: never centers, but
    /// still visible as neighbors (their kind is part of a caller's shape).
    #[test]
    fn reference_site_kinds_are_neighbors_not_centers() {
        let nodes = vec![
            node("n1", "f", "function", "src/a.rs", "a::f"),
            node("n2", "println", "macro_call", "src/a.rs", "a::println"),
            node("n3", "std::fmt", "import", "src/a.rs", "a::std::fmt"),
        ];
        let edges = vec![edge("n1", "n2", "calls", "EXTRACTED")];
        let fps = compute_fingerprints(&nodes, &edges);
        assert_eq!(fps.len(), 1, "only the function is a center, got {fps:?}");
        assert_eq!(fps[0].qualified_name, "a::f");
        assert_eq!(fps[0].edge_count, 1, "the macro_call edge is the function's structure");
    }

    /// Kinds matter: same shape, different center kind → different hash.
    #[test]
    fn center_kind_enters_the_fingerprint() {
        let f = vec![node("n1", "x", "function", "src/a.rs", "a::x")];
        let s = vec![node("n1", "x", "struct", "src/a.rs", "a::x")];
        assert_ne!(
            compute_fingerprints(&f, &[])[0].hash,
            compute_fingerprints(&s, &[])[0].hash,
            "a bare function and a bare struct are different structures"
        );
    }

    /// A (qualified_name, file_path) key collision (struct + impl) yields
    /// one combined row whose hash is member-order-free and rename-stable.
    #[test]
    fn key_collisions_combine_deterministically() {
        let mk = |sid: &str, iid: &str, qn: &str| {
            vec![
                node(sid, "Foo", "struct", "src/a.rs", qn),
                node(iid, "Foo", "impl", "src/a.rs", qn),
            ]
        };
        let a = compute_fingerprints(&mk("n1", "n2", "a::Foo"), &[]);
        let b = compute_fingerprints(&mk("z2", "z1", "a::Bar"), &[]);
        assert_eq!(a.len(), 1, "one row per key, got {a:?}");
        assert_eq!(a[0].node_type, "impl+struct");
        assert_eq!(a[0].hash, b[0].hash, "combined hash must survive rename + node order");
    }

    #[test]
    fn name_edge_count_excludes_contains() {
        let nodes = vec![
            node("m", "mod_a", "module", "src/a.rs", "a"),
            node("f", "f", "function", "src/a.rs", "a::f"),
            node("g", "g", "function", "src/a.rs", "a::g"),
        ];
        let edges = vec![
            edge("m", "f", "contains", "EXTRACTED"),
            edge("m", "g", "contains", "EXTRACTED"),
            edge("f", "g", "calls", "RESOLVED"),
        ];
        let fps = compute_fingerprints(&nodes, &edges);
        let f = hash_of(&fps, "a::f");
        assert_eq!(f.edge_count, 2, "contains + calls both count as structure");
        assert_eq!(f.name_edge_count, 1, "only the calls edge is a name-edge");
    }
}
