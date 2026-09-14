//! `codegraph plan ...` against the store. **Owned by lane L2.**
//!
//! - [`sync`] reads `.codegraph/planes.yaml`, runs every touch through
//!   [`crate::plan::resolve`], and materializes `work_plane` / `work_item` /
//!   `work_touch`. The file is the truth and the tables are a rebuildable
//!   index, so a sync replaces a project's rows rather than merging into
//!   them, in one transaction.
//! - [`list`] / [`show`] / [`touching`] / [`stale`] / [`collisions`] /
//!   [`blast`] read those tables back.
//! - [`lint`] never touches the store at all: it parses and validates the
//!   file, which is why it can run before a first sync and on a machine
//!   with no database.
//!
//! ## Two ways to count a touch
//!
//! A `touches:` entry and an incidence are not the same thing. `- glob:
//! "src/index/**"` is one entry the author wrote and, if it matches four
//! files, four incidences on the hyperedge. Both numbers are reported and
//! neither is derivable from the other:
//!
//! - **Selectors** are entries as written. This is the number to quote when
//!   talking about the plan ("61 touches in the roadmap").
//! - **Touches**, on every struct in this file, are incidences, which is
//!   what board section 3.2 defines a `work_touch` row to be and what makes
//!   collision detection and blast radius a plain set intersection.
//!
//! ## Contract with `src/main.rs`
//!
//! `src/main.rs` and `src/cli.rs` are owned by lane P0 and dispatch straight
//! into the functions below. The CLI never reads a field of these structs:
//! it serializes the whole value for `--json` and prints the [`Display`]
//! rendering otherwise. Signatures are therefore fixed, and every result
//! type keeps its `Serialize`/`Display` pair.
//!
//! [`Display`]: std::fmt::Display

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::graph::dependencies::reverse_dependency_paths;
use crate::graph::{load_project_edges, load_project_nodes, NAME_EDGE_TYPES};
use crate::index::resolve::ResolverNode;
use crate::plan::model::{
    Horizon, Kind, PlaneFile, ResolvedTouch, Selector, Status, TouchConfidence,
};
use crate::plan::resolve::{normalize_repo_path, TouchIndex, UnresolvedReason};
use crate::plan::schema::Violation;
use crate::plan::store::{self, ItemRow, PlanSnapshot, PlaneRow, TouchRow};
use crate::plan::{item_record_id, plane_record_id, schema};

/// Traversal depth [`show`] reports its blast summary at.
///
/// The same default `codegraph plan blast` uses, so the number in a `show`
/// and the number in a `blast` of the same item agree unless the caller
/// asked `blast` for a different depth.
const SHOW_BLAST_DEPTH: usize = 3;

// ============================================================================
// Shared row projections
// ============================================================================

/// One `work_plane` row, as a reader sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneSummary {
    /// The plane id as written in the file.
    pub plane_id: String,
    /// `work_plane:<project>__<plane_id>`.
    pub node_id: String,
    /// One-line title.
    pub title: String,
    /// Lifecycle state.
    pub status: Status,
    /// How far out the plane sits.
    pub horizon: Horizon,
    /// Optional prose summary.
    pub summary: Option<String>,
    /// How many work items the plane holds.
    pub item_count: usize,
}

/// One `work_item` row with its touch tallies, which is what every listing
/// view needs and what makes a stale plan visible at a glance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemSummary {
    /// The item id as written in the file.
    pub item_id: String,
    /// `work_item:<project>__<item_id>`.
    pub node_id: String,
    /// The plane that owns the item.
    pub plane_id: String,
    /// One-line title.
    pub title: String,
    /// What sort of change this is.
    pub kind: Kind,
    /// Lifecycle state.
    pub status: Status,
    /// Total touches on the item, which is the size of its hyperedge.
    /// Incidences, so a glob counts once per file it matched.
    pub touch_count: usize,
    /// Touches that bound to exactly one node.
    pub resolved: usize,
    /// Touches with several surviving candidates.
    pub ambiguous: usize,
    /// Touches that matched nothing. Any value above zero means a stale plan.
    pub unresolved: usize,
    /// `touches:` entries as written in the file, before glob expansion.
    /// See the module docs on why both numbers exist.
    pub selector_count: usize,
    /// Touches that bound to a real path the code graph holds nothing for.
    /// A subset of `resolved`, not a fourth confidence: these are not stale,
    /// they are simply outside what an index of source code can reach.
    pub unindexed: usize,
}

/// How far the union of a touch set reaches through reverse dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastSummary {
    /// Distinct resolved node ids the item touches directly.
    pub seed_count: usize,
    /// Distinct nodes reached from those seeds, seeds excluded.
    pub reached_count: usize,
    /// Traversal depth the count was taken at.
    pub depth: usize,
    /// Distinct files those reached nodes live in.
    pub reached_file_count: usize,
}

/// One node reached by a blast radius walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastHit {
    /// The reached node's id.
    pub node_id: String,
    /// Its name.
    pub name: String,
    /// Its node type.
    pub node_type: String,
    /// The file it lives in.
    pub file_path: String,
    /// Hops from the nearest seed.
    pub depth: usize,
}

/// One touch as a reader sees it: the stored row, plus the two things a
/// [`ResolvedTouch`] has no field for.
///
/// `touch` is flattened, so the JSON of this type is the JSON of a
/// [`ResolvedTouch`] plus at most three keys. A consumer reading `selector`,
/// `raw`, `confidence`, or `candidates` sees exactly what it saw before any
/// of them existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchView {
    /// The touch itself, verbatim from the store.
    #[serde(flatten)]
    pub touch: ResolvedTouch,
    /// Whether the code graph holds a row for the bound target. `None` on a
    /// `symbol:` touch, where the target is a node id and the question does
    /// not arise, and on anything that did not bind. `Some(false)` marks a
    /// path that is real and carries no node: a spec, a README, a schema
    /// file. Such a touch is RESOLVED and is not stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed: Option<bool>,
    /// Why it did not bind to exactly one target. `None` on a RESOLVED
    /// touch, and on a row written by a build that knows a reason this one
    /// does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UnresolvedReason>,
    /// One sentence naming what was looked for and what was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What this type was called when it only ever described touches that did
/// not bind. Kept so callers written against that name keep compiling.
pub type UnboundTouch = TouchView;

impl TouchView {
    fn from_row(row: &TouchRow) -> Self {
        TouchView {
            touch: row.to_resolved(),
            indexed: row.indexed,
            reason: row.reason(),
            detail: row.detail.clone(),
        }
    }

    /// True when this touch bound to a path the code graph holds nothing
    /// for.
    pub fn is_unindexed(&self) -> bool {
        self.indexed == Some(false)
    }
}

impl fmt::Display for TouchView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.touch)?;
        if self.is_unindexed() {
            write!(f, " (not indexed)")?;
        }
        if let Some(reason) = self.reason {
            write!(f, " ({reason})")?;
        }
        if let Some(detail) = &self.detail {
            write!(f, "\n        {detail}")?;
        }
        Ok(())
    }
}

/// How many unbound touches carry one reason.
///
/// The headline a reader acts on, so `plan stale` can say what kind of
/// problem it found before listing them one by one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonCount {
    /// The reason, as written to `work_touch.reason`.
    pub reason: String,
    /// How many touches carry it.
    pub count: usize,
    /// Whether the reader can do anything about it.
    ///
    /// True for every reason this build produces. It was not always: a
    /// `file:` touch on a path with no grammar used to come back
    /// UNRESOLVED and unactionable, which made this report mostly noise on
    /// codegraph's own roadmap. Such a touch now binds RESOLVED with
    /// `indexed: false` and never reaches this report at all. The field
    /// stays because the guarantee it encodes, that everything listed here
    /// is worth someone's time, is worth stating rather than assuming.
    pub actionable: bool,
}

impl fmt::Display for ReasonCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.count, self.reason)?;
        if !self.actionable {
            write!(f, " (no action possible)")?;
        }
        Ok(())
        // The branch above is dead for every reason this build produces and
        // is kept so a reason added later cannot silently claim to be
        // actionable in the rendering while saying otherwise in the JSON.
    }
}

/// Tally reasons in a stable order, heaviest first then alphabetical.
fn tally_reasons(reasons: impl Iterator<Item = Option<UnresolvedReason>>) -> Vec<ReasonCount> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unknown = 0usize;
    for reason in reasons {
        match reason {
            Some(r) => *counts.entry(r.as_str()).or_default() += 1,
            None => unknown += 1,
        }
    }
    let mut out: Vec<ReasonCount> = counts
        .into_iter()
        .map(|(reason, count)| ReasonCount {
            reason: reason.to_string(),
            count,
            actionable: true,
        })
        .collect();
    if unknown > 0 {
        out.push(ReasonCount {
            reason: "unrecorded".to_string(),
            count: unknown,
            actionable: true,
        });
    }
    out.sort_by(|a, b| b.count.cmp(&a.count).then(a.reason.cmp(&b.reason)));
    out
}

/// An unbound touch together with the item that wrote it, for the reports
/// that span the whole roadmap rather than one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemTouch {
    /// The work item id as written in the file.
    pub item_id: String,
    /// The touch and why it did not bind.
    #[serde(flatten)]
    pub touch: TouchView,
}

impl fmt::Display for ItemTouch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.item_id, self.touch)
    }
}

// ============================================================================
// sync
// ============================================================================

/// What one `plan sync` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    /// Project the rows were written under.
    pub project_id: String,
    /// The planes file that was read.
    pub source: PathBuf,
    /// Planes written.
    pub planes: usize,
    /// Work items written.
    pub items: usize,
    /// Touch incidences written.
    pub touches: usize,
    /// Touches that bound to exactly one node.
    pub resolved: usize,
    /// Touches with several surviving candidates.
    pub ambiguous: usize,
    /// Touches that matched nothing.
    pub unresolved: usize,
    /// Rows dropped because the plane is no longer in the file.
    pub removed_planes: usize,
    /// Rows dropped because the item is no longer in the file.
    pub removed_items: usize,
    /// `touches:` entries as written, before glob expansion.
    pub selectors: usize,
    /// Entries that bound. A glob that matched four files counts once here
    /// and four times in `resolved`.
    pub selectors_resolved: usize,
    /// Entries with several candidates and no pick.
    pub selectors_ambiguous: usize,
    /// Entries that matched nothing.
    pub selectors_unresolved: usize,
    /// Indexed files the touches were resolved against. Zero means the
    /// project has never been indexed, which makes every touch unresolved
    /// for a reason that has nothing to do with the plan.
    pub indexed_files: usize,
    /// Indexed symbols the touches were resolved against.
    pub indexed_symbols: usize,
    /// Working tree files the `file:` and `glob:` touches were also matched
    /// against, so a path that codegraph has no grammar for still binds.
    pub working_tree_files: usize,
    /// How that working tree list was found.
    pub discovery: String,
    /// Touch rows that bound to a real path the code graph holds nothing
    /// for. A subset of `resolved`.
    pub unindexed: usize,
    /// How many unbound entries carry each reason, heaviest first. The
    /// line a reader acts on, since not every reason is actionable.
    pub unresolved_by_reason: Vec<ReasonCount>,
    /// Every entry that matched nothing, with the reason.
    pub unresolved_touches: Vec<ItemTouch>,
    /// Every entry with several candidates and no pick.
    pub ambiguous_touches: Vec<ItemTouch>,
}

impl fmt::Display for SyncReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Plan Sync ===")?;
        writeln!(f, "  Project:     {}", self.project_id)?;
        writeln!(f, "  Source:      {}", self.source.display())?;
        writeln!(
            f,
            "  Index:       {} files, {} symbols",
            self.indexed_files, self.indexed_symbols
        )?;
        writeln!(
            f,
            "  Worktree:    {} files, found by {}",
            self.working_tree_files, self.discovery
        )?;
        writeln!(
            f,
            "  Written:     {} planes, {} items, {} touch rows from {} selectors",
            self.planes, self.items, self.touches, self.selectors
        )?;
        writeln!(
            f,
            "  Selectors:   {} resolved, {} ambiguous, {} unresolved",
            self.selectors_resolved, self.selectors_ambiguous, self.selectors_unresolved
        )?;
        writeln!(
            f,
            "  Touch rows:  {} resolved, {} ambiguous, {} unresolved",
            self.resolved, self.ambiguous, self.unresolved
        )?;
        if self.unindexed > 0 {
            writeln!(
                f,
                "  Not indexed: {} of those resolved rows bound to a path the code graph holds nothing for",
                self.unindexed
            )?;
        }
        if self.removed_planes > 0 || self.removed_items > 0 {
            writeln!(
                f,
                "  Removed:     {} planes, {} items no longer in the file",
                self.removed_planes, self.removed_items
            )?;
        }
        if !self.ambiguous_touches.is_empty() {
            writeln!(f, "\n  Ambiguous ({}):", self.ambiguous_touches.len())?;
            for t in &self.ambiguous_touches {
                writeln!(f, "    {t}")?;
            }
        }
        if !self.unresolved_by_reason.is_empty() {
            let rendered: Vec<String> =
                self.unresolved_by_reason.iter().map(|r| r.to_string()).collect();
            writeln!(f, "  Why:         {}", rendered.join(", "))?;
        }
        if !self.unresolved_touches.is_empty() {
            writeln!(f, "\n  Unresolved ({}):", self.unresolved_touches.len())?;
            for t in &self.unresolved_touches {
                writeln!(f, "    {t}")?;
            }
        }
        Ok(())
    }
}

/// Ingest a planes file and materialize it as hyperedges over the code graph.
///
/// Validation runs first and nothing is written if it fails, so a malformed
/// roadmap can never half replace a good one. Every touch is resolved
/// before the write and the verdict is stored verbatim: a selector that
/// matches several nodes is AMBIGUOUS with its candidates listed, never a
/// silent pick of the first one.
pub async fn sync(db: &Surreal<Any>, project_id: &str, planes_path: &Path) -> Result<SyncReport> {
    // `load` refuses to hand back a document with any violation, and its
    // error lists every one of them, so a bad file stops here with the
    // whole list rather than after the first problem or after a partial
    // write.
    let file = schema::load(planes_path)?;

    if let Some(declared) = &file.project {
        if declared != project_id {
            tracing::warn!(
                "{} declares project \"{declared}\" but this sync is writing project \"{project_id}\". \
                 The file's project key is advisory; the resolved project id wins",
                planes_path.display()
            );
        }
    }

    let index = TouchIndex::build(db, project_id, planes_root_hint(planes_path).as_deref()).await?;
    let (old_planes, old_items) = store::load_ids(db, project_id).await?;

    let mut plane_rows: Vec<PlaneRow> = Vec::with_capacity(file.planes.len());
    let mut item_rows: Vec<ItemRow> = Vec::new();
    let mut touch_rows: Vec<TouchRow> = Vec::new();
    let mut report = SyncReport {
        project_id: project_id.to_string(),
        source: planes_path.to_path_buf(),
        planes: 0,
        items: 0,
        touches: 0,
        resolved: 0,
        ambiguous: 0,
        unresolved: 0,
        removed_planes: 0,
        removed_items: 0,
        selectors: 0,
        selectors_resolved: 0,
        selectors_ambiguous: 0,
        selectors_unresolved: 0,
        indexed_files: index.file_count(),
        indexed_symbols: index.symbol_count(),
        working_tree_files: index.disk_file_count(),
        discovery: index.disk_strategy().to_string(),
        unindexed: 0,
        unresolved_by_reason: Vec::new(),
        unresolved_touches: Vec::new(),
        ambiguous_touches: Vec::new(),
    };

    for (plane_index, plane) in file.planes.iter().enumerate() {
        let plane_node_id = plane_record_id(project_id, &plane.id);
        plane_rows.push(PlaneRow {
            project_id: project_id.to_string(),
            plane_id: plane.id.clone(),
            node_id: plane_node_id.clone(),
            title: plane.title.clone(),
            status: plane.status.as_str().to_string(),
            horizon: plane.horizon.as_str().to_string(),
            summary: plane.summary.clone(),
            ordinal: plane_index as i64,
        });

        for item in &plane.items {
            let item_node_id = item_record_id(project_id, &item.id);
            item_rows.push(ItemRow {
                project_id: project_id.to_string(),
                item_id: item.id.clone(),
                node_id: item_node_id.clone(),
                plane: plane.id.clone(),
                plane_node_id: plane_node_id.clone(),
                title: item.title.clone(),
                kind: item.kind.as_str().to_string(),
                status: item.status.as_str().to_string(),
                depends_on: item.depends_on.clone(),
                spec: item.spec.clone(),
                notes: item.notes.clone(),
                ordinal: item_rows.len() as i64,
            });

            let mut ordinal = 0i64;
            for (selector_ordinal, touch) in item.touches.iter().enumerate() {
                let binding = index.resolve(touch);
                report.selectors += 1;
                match binding.confidence {
                    TouchConfidence::Resolved => report.selectors_resolved += 1,
                    TouchConfidence::Ambiguous => report.selectors_ambiguous += 1,
                    TouchConfidence::Unresolved => report.selectors_unresolved += 1,
                }

                for bound in &binding.rows {
                    let row = &bound.touch;
                    match row.confidence {
                        TouchConfidence::Resolved => report.resolved += 1,
                        TouchConfidence::Ambiguous => report.ambiguous += 1,
                        TouchConfidence::Unresolved => report.unresolved += 1,
                    }
                    if bound.indexed == Some(false) {
                        report.unindexed += 1;
                    }
                    touch_rows.push(TouchRow {
                        project_id: project_id.to_string(),
                        item: item_node_id.clone(),
                        item_id: item.id.clone(),
                        selector: row.selector.as_str().to_string(),
                        raw: row.raw.clone(),
                        to_id: row.to_id.clone(),
                        confidence: row.confidence.as_str().to_string(),
                        candidates: if row.candidates.is_empty() {
                            None
                        } else {
                            Some(row.candidates.clone())
                        },
                        reason: binding.reason.map(|r| r.as_str().to_string()),
                        detail: binding.detail.clone(),
                        indexed: bound.indexed,
                        ordinal,
                        selector_ordinal: selector_ordinal as i64,
                    });
                    ordinal += 1;
                }

                // One entry that did not bind is reported once, whatever
                // its row count, because the author fixes entries.
                if binding.confidence != TouchConfidence::Resolved {
                    let entry = ItemTouch {
                        item_id: item.id.clone(),
                        touch: TouchView {
                            touch: binding.rows[0].touch.clone(),
                            indexed: binding.rows[0].indexed,
                            reason: binding.reason,
                            detail: binding.detail.clone(),
                        },
                    };
                    if binding.confidence == TouchConfidence::Ambiguous {
                        report.ambiguous_touches.push(entry);
                    } else {
                        report.unresolved_touches.push(entry);
                    }
                }
            }
        }
    }

    report.planes = plane_rows.len();
    report.items = item_rows.len();
    report.touches = touch_rows.len();
    report.unresolved_by_reason =
        tally_reasons(report.unresolved_touches.iter().map(|t| t.touch.reason));

    let new_planes: HashSet<&str> = plane_rows.iter().map(|p| p.plane_id.as_str()).collect();
    let new_items: HashSet<&str> = item_rows.iter().map(|i| i.item_id.as_str()).collect();
    report.removed_planes = old_planes
        .iter()
        .filter(|id| !new_planes.contains(id.as_str()))
        .count();
    report.removed_items = old_items
        .iter()
        .filter(|id| !new_items.contains(id.as_str()))
        .count();

    store::replace_project(db, project_id, &plane_rows, &item_rows, &touch_rows).await?;

    Ok(report)
}

/// The directory a planes file's paths are relative to, used only as a
/// fallback when the project was never registered in this store.
///
/// `.codegraph/planes.yaml` sits one directory below the repo root, so the
/// root is the grandparent. A `--planes-file` pointing somewhere else falls
/// back to that file's own directory, which is the only defensible guess
/// and is never used when `project_registry` has the real answer.
fn planes_root_hint(planes_path: &Path) -> Option<PathBuf> {
    let dir = planes_path.parent()?;
    if dir.file_name().map(|n| n == crate::plan::CODEGRAPH_DIR) == Some(true) {
        return dir.parent().map(Path::to_path_buf);
    }
    Some(dir.to_path_buf())
}

// ============================================================================
// list
// ============================================================================

/// Which planes and items a listing should include. Every field is an AND,
/// and `None` means no constraint on that field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilter {
    /// Restrict to one plane id.
    pub plane: Option<String>,
    /// Restrict to items in one lifecycle state.
    pub status: Option<Status>,
    /// Restrict to planes at one horizon.
    pub horizon: Option<Horizon>,
}

/// One plane and the items that survived the filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneListing {
    /// The plane.
    pub plane: PlaneSummary,
    /// Its matching items, in file order.
    pub items: Vec<ItemSummary>,
}

/// The roadmap, filtered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReport {
    /// Project the rows came from.
    pub project_id: String,
    /// The filter that produced this listing.
    pub filter: ListFilter,
    /// Matching planes, in file order.
    pub planes: Vec<PlaneListing>,
}

impl fmt::Display for ListReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Planes: {} ===", self.project_id)?;
        if self.planes.is_empty() {
            return writeln!(
                f,
                "No planes matched. Run \"codegraph plan sync\" if the roadmap has never been ingested."
            );
        }
        for listing in &self.planes {
            let p = &listing.plane;
            writeln!(f, "\n{} [{}/{}] {}", p.plane_id, p.status, p.horizon, p.title)?;
            if let Some(summary) = &p.summary {
                writeln!(f, "  {summary}")?;
            }
            for item in &listing.items {
                writeln!(f, "  {item}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for ItemSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}/{}] {} ({} touches: {} resolved, {} ambiguous, {} unresolved",
            self.item_id,
            self.kind,
            self.status,
            self.title,
            self.touch_count,
            self.resolved,
            self.ambiguous,
            self.unresolved
        )?;
        if self.unindexed > 0 {
            write!(f, "; {} not indexed", self.unindexed)?;
        }
        f.write_str(")")
    }
}

/// List planes and their items, filtered.
///
/// A plane that survives the plane-level filters is listed even when the
/// status filter empties its item list, so a filter never makes a plane
/// look like it does not exist.
pub async fn list(db: &Surreal<Any>, project_id: &str, filter: &ListFilter) -> Result<ListReport> {
    let snapshot = store::load_snapshot(db, project_id).await?;
    let summaries = summarize(&snapshot);

    let mut planes = Vec::new();
    for plane in &snapshot.planes {
        if let Some(want) = &filter.plane {
            if &plane.plane_id != want {
                continue;
            }
        }
        if let Some(want) = filter.horizon {
            if plane.horizon() != want {
                continue;
            }
        }
        let items: Vec<ItemSummary> = snapshot
            .items
            .iter()
            .filter(|i| i.plane == plane.plane_id)
            .filter(|i| filter.status.is_none_or(|want| i.status() == want))
            .filter_map(|i| summaries.get(&i.item_id).cloned())
            .collect();
        planes.push(PlaneListing {
            plane: plane_summary(plane, &snapshot),
            items,
        });
    }

    Ok(ListReport {
        project_id: project_id.to_string(),
        filter: filter.clone(),
        planes,
    })
}

// ============================================================================
// show
// ============================================================================

/// One work item in full: its touches with confidence, its dependency edges
/// both ways, and how far it reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowReport {
    /// Project the item belongs to.
    pub project_id: String,
    /// The plane that owns it.
    pub plane: PlaneSummary,
    /// The item itself.
    pub item: ItemSummary,
    /// Its incidence list, in file order, each carrying its `indexed` flag
    /// and, when it did not bind, its reason.
    pub touches: Vec<TouchView>,
    /// Why each touch that did not bind failed, in file order.
    pub unbound: Vec<TouchView>,
    /// Item ids this item waits on.
    pub depends_on: Vec<String>,
    /// Item ids waiting on this item. The inverse of `depends_on`, computed
    /// so an agent can see what it unblocks without scanning the file.
    pub blocks: Vec<String>,
    /// The spec that governs the item.
    pub spec: Option<String>,
    /// Free-form notes from the file.
    pub notes: Option<String>,
    /// How far the touch set reaches.
    pub blast: BlastSummary,
}

impl fmt::Display for ShowReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== {} ===", self.item.item_id)?;
        writeln!(f, "  Title:       {}", self.item.title)?;
        writeln!(f, "  Plane:       {} ({})", self.plane.plane_id, self.plane.title)?;
        writeln!(f, "  Kind:        {}", self.item.kind)?;
        writeln!(f, "  Status:      {}", self.item.status)?;
        if let Some(spec) = &self.spec {
            writeln!(f, "  Spec:        {spec}")?;
        }
        if !self.depends_on.is_empty() {
            writeln!(f, "  Depends on:  {}", self.depends_on.join(", "))?;
        }
        if !self.blocks.is_empty() {
            writeln!(f, "  Blocks:      {}", self.blocks.join(", "))?;
        }
        writeln!(f, "\n  Touches ({}):", self.touches.len())?;
        for t in &self.touches {
            writeln!(f, "    {t}")?;
        }
        if !self.unbound.is_empty() {
            writeln!(f, "\n  Did not bind ({}):", self.unbound.len())?;
            for t in &self.unbound {
                writeln!(f, "    {t}")?;
            }
        }
        writeln!(
            f,
            "\n  Blast:       {} seeds reach {} nodes in {} files at depth {}",
            self.blast.seed_count,
            self.blast.reached_count,
            self.blast.reached_file_count,
            self.blast.depth
        )?;
        if let Some(notes) = &self.notes {
            writeln!(f, "\n  Notes: {notes}")?;
        }
        Ok(())
    }
}

/// Everything known about one work item.
pub async fn show(db: &Surreal<Any>, project_id: &str, item_id: &str) -> Result<ShowReport> {
    let snapshot = store::load_snapshot(db, project_id).await?;
    let item = snapshot
        .item(item_id)
        .ok_or_else(|| unknown_item(&snapshot, project_id, item_id))?;
    let plane = snapshot.plane(&item.plane).ok_or_else(|| {
        anyhow!(
            "work item \"{item_id}\" names plane \"{}\", which has no row in project {project_id}. Re-run \"codegraph plan sync\"",
            item.plane
        )
    })?;
    let summaries = summarize(&snapshot);
    let rows = snapshot.touches_of(&item.node_id);

    let blast = blast_report(db, project_id, &snapshot, item, SHOW_BLAST_DEPTH).await?;

    let blocks: Vec<String> = snapshot
        .items
        .iter()
        .filter(|other| other.depends_on.iter().any(|d| d == item_id))
        .map(|other| other.item_id.clone())
        .collect();

    Ok(ShowReport {
        project_id: project_id.to_string(),
        plane: plane_summary(plane, &snapshot),
        item: summaries
            .get(item_id)
            .cloned()
            .expect("every stored item has a summary"),
        touches: rows.iter().map(|r| TouchView::from_row(r)).collect(),
        unbound: rows
            .iter()
            .filter(|r| !r.is_resolved())
            .map(|r| TouchView::from_row(r))
            .collect(),
        depends_on: item.depends_on.clone(),
        blocks,
        spec: item.spec.clone(),
        notes: item.notes.clone(),
        blast: BlastSummary {
            seed_count: blast.seeds.len(),
            reached_count: blast.reached.len(),
            depth: blast.depth,
            reached_file_count: blast.reached_files,
        },
    })
}

// ============================================================================
// touching
// ============================================================================

/// One work item that covers the queried target, and the touch that does it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchingHit {
    /// The item whose hyperedge covers the target.
    pub item: ItemSummary,
    /// The touch that matched, with its `indexed` flag.
    pub touch: TouchView,
    /// Why this touch was considered a match for the query.
    pub matched_by: MatchKind,
}

/// How a `touching` query matched a stored touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The touch is RESOLVED and bound to exactly what was asked about.
    /// The only kind that is evidence of real coverage.
    Bound,
    /// The touch is AMBIGUOUS and the target is one of its candidates, so
    /// the plan may or may not have meant this.
    Candidate,
    /// The touch's `raw` is the queried string verbatim. Reported because a
    /// plan that names a file by a path that no longer resolves is still a
    /// plan about that path, and hiding it would hide the stale ones.
    Literal,
}

impl fmt::Display for MatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MatchKind::Bound => "bound",
            MatchKind::Candidate => "candidate",
            MatchKind::Literal => "literal",
        })
    }
}

/// Inverse incidence: what planned work covers this file or symbol.
///
/// This is the query the collaboration story rests on. An agent about to
/// edit a file asks what is already planned there before it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchingReport {
    /// Project searched.
    pub project_id: String,
    /// The file path or symbol that was asked about, verbatim.
    pub target: String,
    /// What the target itself resolved to, so a reader can tell an empty
    /// result caused by "nothing is planned here" from one caused by "this
    /// name does not name anything".
    pub target_ids: Vec<String>,
    /// Every item covering it, in file order.
    pub hits: Vec<TouchingHit>,
}

impl fmt::Display for TouchingReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "=== Planned work touching '{}' ({} items) ===",
            self.target,
            self.hits.len()
        )?;
        if self.hits.is_empty() {
            if self.target_ids.is_empty() {
                return writeln!(
                    f,
                    "Nothing planned covers this target, and nothing in the index answers to it either."
                );
            }
            return writeln!(f, "Nothing planned covers this target.");
        }
        for hit in &self.hits {
            writeln!(
                f,
                "  {} [{}/{}] {}",
                hit.item.item_id, hit.item.kind, hit.item.status, hit.item.title
            )?;
            writeln!(f, "    via {} [{}]", hit.touch, hit.matched_by)?;
        }
        Ok(())
    }
}

/// What planned work covers a path or a symbol.
///
/// The target is resolved the same way a touch is, so `src/canon.rs` and
/// `canon_search` are both accepted and neither needs a flag. Matching is
/// deliberately a superset of "bound to exactly this node": a plan naming a
/// path verbatim is reported even when that path no longer resolves, since
/// suppressing it would hide exactly the stale plans an agent needs to see.
/// Every hit says which kind it is.
pub async fn touching(db: &Surreal<Any>, project_id: &str, target: &str) -> Result<TouchingReport> {
    let snapshot = store::load_snapshot(db, project_id).await?;
    let index = TouchIndex::build(db, project_id, None).await?;
    let summaries = summarize(&snapshot);

    // A target is resolved into the same id space touches bind to: a path
    // for a file, a node id for a symbol. Both lookups run, because a
    // string like `canon` is a legal file stem and a legal symbol name and
    // guessing which the caller meant would drop real hits.
    let mut target_ids: BTreeSet<String> = BTreeSet::new();
    let as_path = normalize_repo_path(target);
    if index.has_file(&as_path) || index.on_disk(&as_path) {
        target_ids.insert(as_path.clone());
    }
    for binding in [
        index.resolve(&crate::plan::model::Touch::new(Selector::Symbol, target)),
    ] {
        for bound in binding.rows {
            if let Some(id) = bound.touch.to_id {
                target_ids.insert(id);
            }
            for candidate in bound.touch.candidates {
                target_ids.insert(candidate);
            }
        }
    }

    let literal = normalize_repo_path(target);
    let mut hits = Vec::new();
    for row in &snapshot.touches {
        let Some(item) = summaries.get(&row.item_id) else {
            continue;
        };
        let matched = if row.is_resolved()
            && row.to_id.as_deref().is_some_and(|id| target_ids.contains(id))
        {
            Some(MatchKind::Bound)
        } else if row
            .candidates
            .as_ref()
            .is_some_and(|c| c.iter().any(|id| target_ids.contains(id)))
        {
            Some(MatchKind::Candidate)
        } else if row.raw == target || normalize_repo_path(&row.raw) == literal {
            Some(MatchKind::Literal)
        } else {
            None
        };
        if let Some(matched_by) = matched {
            hits.push(TouchingHit {
                item: item.clone(),
                touch: TouchView::from_row(row),
                matched_by,
            });
        }
    }

    Ok(TouchingReport {
        project_id: project_id.to_string(),
        target: target.to_string(),
        target_ids: target_ids.into_iter().collect(),
        hits,
    })
}

// ============================================================================
// collisions
// ============================================================================

/// Two active items whose touch sets intersect: two agents about to edit the
/// same thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collision {
    /// One item.
    pub a: ItemSummary,
    /// The other.
    pub b: ItemSummary,
    /// Node ids both items touch.
    pub shared_nodes: Vec<String>,
    /// The raw selectors that produced the overlap, for the message a human
    /// reads. Ambiguous and unresolved touches never contribute here, since
    /// an unbound name is not evidence of a real overlap.
    pub shared_selectors: Vec<String>,
}

/// Every intersecting pair among the active items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollisionsReport {
    /// Project searched.
    pub project_id: String,
    /// Intersecting pairs, each reported once.
    pub collisions: Vec<Collision>,
    /// How many items were eligible. Only `active` items collide, so a zero
    /// here is why an empty report is empty.
    pub active_items: usize,
}

impl fmt::Display for CollisionsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "=== Collisions among active work ({} pairs, {} active items) ===",
            self.collisions.len(),
            self.active_items
        )?;
        if self.collisions.is_empty() {
            return writeln!(f, "No two active items touch the same code.");
        }
        for c in &self.collisions {
            writeln!(
                f,
                "\n  {} and {} share {} node(s)",
                c.a.item_id,
                c.b.item_id,
                c.shared_nodes.len()
            )?;
            writeln!(f, "    {} {}", c.a.item_id, c.a.title)?;
            writeln!(f, "    {} {}", c.b.item_id, c.b.title)?;
            for s in &c.shared_selectors {
                writeln!(f, "    via {s}")?;
            }
        }
        Ok(())
    }
}

/// Active items whose touch sets intersect.
///
/// Only `active` items are considered, which is the model's own rule
/// (`Status::Active`: "Only active items participate in collision
/// detection"). `planned` is deliberately excluded: a roadmap records far
/// more planned work than anyone is doing, so including it would make
/// almost every pair collide and the signal an agent is checking for, that
/// someone is editing this code right now, would be buried.
///
/// Glob touches need no special case because a glob is stored expanded, one
/// row per matched file, so the intersection is the same plain set
/// operation as for a file touch.
pub async fn collisions(db: &Surreal<Any>, project_id: &str) -> Result<CollisionsReport> {
    let snapshot = store::load_snapshot(db, project_id).await?;
    let summaries = summarize(&snapshot);

    let active: Vec<&ItemRow> = snapshot
        .items
        .iter()
        .filter(|i| i.status() == Status::Active)
        .collect();

    // node id -> the rows of each active item that bound to it.
    let mut bound: BTreeMap<&str, BTreeMap<&str, Vec<&TouchRow>>> = BTreeMap::new();
    for row in &snapshot.touches {
        if !row.is_resolved() {
            continue;
        }
        let Some(to_id) = row.to_id.as_deref() else {
            continue;
        };
        if !active.iter().any(|i| i.item_id == row.item_id) {
            continue;
        }
        bound
            .entry(to_id)
            .or_default()
            .entry(row.item_id.as_str())
            .or_default()
            .push(row);
    }

    // pair -> shared node ids, then shared selector renderings.
    let mut pairs: BTreeMap<(&str, &str), (BTreeSet<&str>, BTreeSet<String>)> = BTreeMap::new();
    for (to_id, per_item) in &bound {
        let ids: Vec<&&str> = per_item.keys().collect();
        for (i, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(i + 1) {
                let key = (**a, **b);
                let entry = pairs.entry(key).or_default();
                entry.0.insert(to_id);
                for row_a in &per_item[**a] {
                    for row_b in &per_item[**b] {
                        entry.1.insert(format!(
                            "{to_id} via {} {}: {} and {} {}: {}",
                            a, row_a.selector, row_a.raw, b, row_b.selector, row_b.raw
                        ));
                    }
                }
            }
        }
    }

    let collisions = pairs
        .into_iter()
        .filter_map(|((a, b), (nodes, selectors))| {
            Some(Collision {
                a: summaries.get(a)?.clone(),
                b: summaries.get(b)?.clone(),
                shared_nodes: nodes.into_iter().map(str::to_string).collect(),
                shared_selectors: selectors.into_iter().collect(),
            })
        })
        .collect();

    Ok(CollisionsReport {
        project_id: project_id.to_string(),
        collisions,
        active_items: active.len(),
    })
}

// ============================================================================
// stale
// ============================================================================

/// One item whose plan points at code that is not there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleItem {
    /// The item.
    pub item: ItemSummary,
    /// Its UNRESOLVED touches, which are the stale part, each with the
    /// reason it did not bind.
    pub unresolved: Vec<TouchView>,
}

/// Every `planned` or `active` item carrying an UNRESOLVED touch.
///
/// A plan whose touches no longer bind is exactly as serious as a stale code
/// reference, and it is reported in the same vocabulary, so the same
/// judgment applies. `done` and `abandoned` items are excluded: see
/// [`stale`] for why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleReport {
    /// Project searched.
    pub project_id: String,
    /// Stale items, in file order.
    pub items: Vec<StaleItem>,
    /// How many unbound touches carry each reason across the whole
    /// roadmap, heaviest first.
    pub by_reason: Vec<ReasonCount>,
    /// Unbound touches whose reason the reader can actually do something
    /// about. A roadmap that cites its own specs carries permanently
    /// unbindable touches, and counting those as stale work would make
    /// this report unreadable on the repo it was built for.
    pub actionable: usize,
}

impl fmt::Display for StaleReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "=== Stale plans ({} items, {} actionable touches) ===",
            self.items.len(),
            self.actionable
        )?;
        if self.items.is_empty() {
            return writeln!(f, "Every touch in the roadmap still binds to live code.");
        }
        if !self.by_reason.is_empty() {
            let rendered: Vec<String> = self.by_reason.iter().map(|r| r.to_string()).collect();
            writeln!(f, "  {}", rendered.join(", "))?;
        }
        for s in &self.items {
            writeln!(f, "\n  {} {}", s.item.item_id, s.item.title)?;
            for t in &s.unresolved {
                writeln!(f, "    {t}")?;
            }
        }
        Ok(())
    }
}

/// Items with UNRESOLVED touches, among the items a stale touch means
/// something for.
///
/// `planned` and `active` items only. A `done` item describes work that has
/// already landed, and the code it named has every right to have moved on
/// since: a finished item whose touch no longer binds is a record of
/// history, not a plan pointing at nothing. `abandoned` is the same case
/// for the opposite reason, work deliberately dropped and kept in the file
/// so the decision stays visible. Reporting either as stale asks the reader
/// to fix something that is already settled, and it is how a staleness
/// report accumulates entries nobody can close.
///
/// This is the same status discipline [`collisions`] applies, for the same
/// reason: the report is only useful if everything in it is worth acting
/// on. Found by the integration pass on this repository, where item RF-8 is
/// `done` and touches `write_updates`, a symbol commit `3a517b2` deleted.
pub async fn stale(db: &Surreal<Any>, project_id: &str) -> Result<StaleReport> {
    let snapshot = store::load_snapshot(db, project_id).await?;
    let summaries = summarize(&snapshot);

    let mut items = Vec::new();
    for item in &snapshot.items {
        if matches!(item.status(), Status::Done | Status::Abandoned) {
            continue;
        }
        let unresolved: Vec<TouchView> = snapshot
            .touches_of(&item.node_id)
            .into_iter()
            .filter(|r| r.is_unresolved())
            .map(TouchView::from_row)
            .collect();
        if unresolved.is_empty() {
            continue;
        }
        let Some(summary) = summaries.get(&item.item_id) else {
            continue;
        };
        items.push(StaleItem {
            item: summary.clone(),
            unresolved,
        });
    }

    let by_reason = tally_reasons(
        items
            .iter()
            .flat_map(|i| i.unresolved.iter())
            .map(|t| t.reason),
    );
    let actionable = by_reason.iter().filter(|r| r.actionable).map(|r| r.count).sum();

    Ok(StaleReport {
        project_id: project_id.to_string(),
        items,
        by_reason,
        actionable,
    })
}

// ============================================================================
// blast
// ============================================================================

/// How far one work item reaches: the union of its touch set, then reverse
/// dependencies over that union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastReport {
    /// Project searched.
    pub project_id: String,
    /// The item whose reach this is.
    pub item_id: String,
    /// Resolved node ids the item touches directly. A `file:` or `glob:`
    /// touch contributes every symbol defined in the file it matched, since
    /// a path has no node of its own to walk from.
    pub seeds: Vec<String>,
    /// Touches that could not be used as seeds, because they never bound.
    /// Listed rather than dropped: a blast radius computed over an
    /// incomplete seed set is an underestimate, and the reader has to know.
    pub unbound_touches: Vec<TouchView>,
    /// Traversal depth used.
    pub depth: usize,
    /// Nodes reached, seeds excluded.
    pub reached: Vec<BlastHit>,
    /// Distinct files the reached nodes live in.
    pub reached_files: usize,
    /// Touches that bound to a real path the code graph holds nothing for.
    /// Skipped as seeds because there is no node to walk from, and counted
    /// here rather than dropped: like `unbound_touches`, they make this
    /// reach a lower bound, and the reader has to know.
    pub skipped_unindexed: usize,
}

impl fmt::Display for BlastReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "=== Blast radius of {} (depth {}) ===",
            self.item_id, self.depth
        )?;
        writeln!(
            f,
            "  {} seed(s) reach {} node(s) in {} file(s)",
            self.seeds.len(),
            self.reached.len(),
            self.reached_files
        )?;
        if self.skipped_unindexed > 0 {
            writeln!(
                f,
                "  {} touch(es) bound to a path the code graph holds nothing for, and were skipped as seeds",
                self.skipped_unindexed
            )?;
        }
        if !self.unbound_touches.is_empty() {
            writeln!(
                f,
                "\n  {} touch(es) never bound, so this reach is a lower bound:",
                self.unbound_touches.len()
            )?;
            for t in &self.unbound_touches {
                writeln!(f, "    {t}")?;
            }
        }
        if !self.reached.is_empty() {
            writeln!(f)?;
            for h in &self.reached {
                let indent = "  ".repeat(h.depth);
                writeln!(f, "  {indent}{} ({}) {}", h.name, h.node_type, h.file_path)?;
            }
        }
        Ok(())
    }
}

/// Reverse dependencies over the union of an item's touch set.
pub async fn blast(
    db: &Surreal<Any>,
    project_id: &str,
    item_id: &str,
    depth: usize,
) -> Result<BlastReport> {
    let snapshot = store::load_snapshot(db, project_id).await?;
    let item = snapshot
        .item(item_id)
        .ok_or_else(|| unknown_item(&snapshot, project_id, item_id))?;
    blast_report(db, project_id, &snapshot, item, depth).await
}

/// The shared body of [`blast`] and the blast summary [`show`] reports.
///
/// ## Which reverse-dependency walk this calls, and why
///
/// `graph::dependencies` exposes two entry points onto one BFS. The async
/// [`crate::graph::dependencies::get_reverse_dependencies`] is a thin
/// adapter that loads every node and every edge of the project **on each
/// call** and then runs the walk; the pure
/// [`reverse_dependency_paths`] takes the already-loaded slices. Its own
/// documentation states that it mirrors the adapter's policy exactly: the
/// same `matching_roots` seeding, RESOLVED edges only, the same depth
/// cutoff, the same first-visit-wins rule, and it lives beside the other so
/// the two can be diffed in review rather than drifting.
///
/// This calls the pure one because the walk here is per seed and the seed
/// set is unbounded: a single `- glob: "tests/fixtures/**"` touch expands
/// to every symbol in every fixture file, so the adapter would reload the
/// whole project once per seed. One load and N in-memory walks is the same
/// answer at a fraction of the cost, and it is still the codebase's own
/// BFS rather than a second copy of it.
async fn blast_report(
    db: &Surreal<Any>,
    project_id: &str,
    snapshot: &PlanSnapshot,
    item: &ItemRow,
    depth: usize,
) -> Result<BlastReport> {
    let nodes = load_project_nodes(db, project_id)
        .await
        .context("loading the code graph for a blast radius failed")?;
    let edges = load_project_edges(db, project_id, &NAME_EDGE_TYPES)
        .await
        .context("loading the code graph for a blast radius failed")?;

    let by_id: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        by_file.entry(n.file_path.as_str()).or_default().push(i);
    }

    let rows = snapshot.touches_of(&item.node_id);

    // Seeds, in a deterministic order. A `symbol:` touch names a node
    // directly; a `file:` or `glob:` touch names a path, whose reverse
    // dependencies are the reverse dependencies of the symbols defined in
    // it, because a path has no node of its own in this graph.
    let mut seed_idx: BTreeSet<usize> = BTreeSet::new();
    let mut skipped_unindexed = 0usize;
    for row in &rows {
        if !row.is_resolved() {
            continue;
        }
        // A path the graph holds nothing for has no node to walk from. It
        // is a real target for `touching` and `collisions`, and a dead end
        // here, so it is counted rather than silently dropped.
        if row.is_unindexed() {
            skipped_unindexed += 1;
            continue;
        }
        let Some(to_id) = row.to_id.as_deref() else {
            continue;
        };
        match row.selector.parse::<Selector>() {
            Ok(Selector::Symbol) => {
                if let Some(&i) = by_id.get(to_id) {
                    seed_idx.insert(i);
                }
            }
            _ => {
                if let Some(list) = by_file.get(to_id) {
                    seed_idx.extend(list.iter().copied());
                }
            }
        }
    }

    let seed_ids: HashSet<&str> = seed_idx.iter().map(|&i| nodes[i].id.as_str()).collect();
    let seeds: Vec<String> = seed_idx.iter().map(|&i| nodes[i].id.clone()).collect();

    // The walk is seeded by bare name, so distinct names are the unit of
    // work: two seeds sharing a name are one walk whose results are then
    // filtered back to this item's own roots.
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for &i in &seed_idx {
        if !nodes[i].name.is_empty() {
            names.insert(nodes[i].name.as_str());
        }
    }

    // Reached node id -> shallowest depth it was reached at.
    let mut reached: BTreeMap<&str, usize> = BTreeMap::new();
    for name in names {
        for path in reverse_dependency_paths(&nodes, &edges, name, depth) {
            if !seed_ids.contains(path.root_node_id.as_str()) {
                continue;
            }
            if seed_ids.contains(path.dependent_node_id.as_str()) {
                continue;
            }
            let Some(&i) = by_id.get(path.dependent_node_id.as_str()) else {
                continue;
            };
            let hit_depth = path.hops.len();
            reached
                .entry(nodes[i].id.as_str())
                .and_modify(|d| *d = (*d).min(hit_depth))
                .or_insert(hit_depth);
        }
    }

    let mut hits: Vec<BlastHit> = reached
        .iter()
        .filter_map(|(id, &d)| {
            let &i = by_id.get(id)?;
            Some(hit(&nodes[i], d))
        })
        .collect();
    hits.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.file_path.cmp(&b.file_path))
            .then(a.name.cmp(&b.name))
            .then(a.node_id.cmp(&b.node_id))
    });
    let reached_files = hits
        .iter()
        .map(|h| h.file_path.as_str())
        .collect::<BTreeSet<_>>()
        .len();

    Ok(BlastReport {
        project_id: project_id.to_string(),
        item_id: item.item_id.clone(),
        seeds,
        unbound_touches: rows
            .iter()
            .filter(|r| !r.is_resolved())
            .map(|r| TouchView::from_row(r))
            .collect(),
        depth,
        reached: hits,
        reached_files,
        skipped_unindexed,
    })
}

fn hit(node: &ResolverNode, depth: usize) -> BlastHit {
    BlastHit {
        node_id: node.id.clone(),
        name: node.name.clone(),
        node_type: node.node_type.clone(),
        file_path: node.file_path.clone(),
        depth,
    }
}

// ============================================================================
// lint
// ============================================================================

/// The result of checking a planes file against the schema rules.
///
/// This reads only the file, never the store, so it is the one plan command
/// that works before a first `sync` and on a machine with no database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintReport {
    /// The file that was checked.
    pub source: PathBuf,
    /// Planes parsed.
    pub planes: usize,
    /// Work items parsed.
    pub items: usize,
    /// Touch incidences parsed.
    pub touches: usize,
    /// Every rule violation found, in the order [`crate::plan::validate`]
    /// reports them.
    pub violations: Vec<Violation>,
}

impl LintReport {
    /// True when the file broke no rule.
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

impl fmt::Display for LintReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Plan lint: {} ===", self.source.display())?;
        writeln!(
            f,
            "  {} planes, {} items, {} touches",
            self.planes, self.items, self.touches
        )?;
        if self.is_clean() {
            return writeln!(f, "  No problems found.");
        }
        writeln!(f, "\n  {} problem(s):", self.violations.len())?;
        for (i, v) in self.violations.iter().enumerate() {
            writeln!(f, "    {}. {v}", i + 1)?;
        }
        Ok(())
    }
}

/// Validate a planes file without touching the store.
///
/// Every rule violation is reported together, which is what
/// [`crate::plan::validate`] is built to do and what makes one run fix a
/// whole file. A file that cannot be *parsed* is a different thing and
/// comes back as an error rather than as violations: there is no document
/// to enumerate rules over, and `ViolationCode` has no variant that could
/// honestly describe a YAML syntax error.
pub fn lint(planes_path: &Path) -> Result<LintReport> {
    let file: PlaneFile = schema::load_unvalidated(planes_path)?;
    Ok(LintReport {
        source: planes_path.to_path_buf(),
        planes: file.planes.len(),
        items: file.planes.iter().map(|p| p.items.len()).sum(),
        touches: file
            .planes
            .iter()
            .flat_map(|p| p.items.iter())
            .map(|i| i.touches.len())
            .sum(),
        violations: schema::validate(&file),
    })
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Every item's summary, keyed by item id, computed once per command.
fn summarize(snapshot: &PlanSnapshot) -> HashMap<String, ItemSummary> {
    // (rows, resolved, ambiguous, unresolved, distinct selectors, unindexed)
    let mut tallies: HashMap<&str, (usize, usize, usize, usize, BTreeSet<i64>, usize)> =
        HashMap::new();
    for row in &snapshot.touches {
        let entry = tallies.entry(row.item_id.as_str()).or_default();
        entry.0 += 1;
        if row.is_unindexed() {
            entry.5 += 1;
        }
        match row.confidence.parse::<TouchConfidence>() {
            Ok(TouchConfidence::Resolved) => entry.1 += 1,
            Ok(TouchConfidence::Ambiguous) => entry.2 += 1,
            // An unreadable confidence understates rather than overstates,
            // exactly as `TouchRow::to_resolved` does.
            _ => entry.3 += 1,
        }
        entry.4.insert(row.selector_ordinal);
    }

    snapshot
        .items
        .iter()
        .map(|item| {
            let t = tallies.get(item.item_id.as_str());
            (
                item.item_id.clone(),
                ItemSummary {
                    item_id: item.item_id.clone(),
                    node_id: item.node_id.clone(),
                    plane_id: item.plane.clone(),
                    title: item.title.clone(),
                    kind: item.kind(),
                    status: item.status(),
                    touch_count: t.map_or(0, |t| t.0),
                    resolved: t.map_or(0, |t| t.1),
                    ambiguous: t.map_or(0, |t| t.2),
                    unresolved: t.map_or(0, |t| t.3),
                    selector_count: t.map_or(0, |t| t.4.len()),
                    unindexed: t.map_or(0, |t| t.5),
                },
            )
        })
        .collect()
}

fn plane_summary(plane: &PlaneRow, snapshot: &PlanSnapshot) -> PlaneSummary {
    PlaneSummary {
        plane_id: plane.plane_id.clone(),
        node_id: plane.node_id.clone(),
        title: plane.title.clone(),
        status: plane.status(),
        horizon: plane.horizon(),
        summary: plane.summary.clone(),
        item_count: snapshot
            .items
            .iter()
            .filter(|i| i.plane == plane.plane_id)
            .count(),
    }
}

/// The error for an item id that is not in the store, phrased so the reader
/// can tell "you typed it wrong" apart from "this project was never synced".
fn unknown_item(snapshot: &PlanSnapshot, project_id: &str, item_id: &str) -> anyhow::Error {
    if snapshot.is_empty() {
        return anyhow!(
            "project {project_id} has no roadmap in the store. Run \"codegraph plan sync\" first"
        );
    }
    let known: Vec<&str> = snapshot
        .items
        .iter()
        .map(|i| i.item_id.as_str())
        .take(12)
        .collect();
    anyhow!(
        "no work item \"{item_id}\" in project {project_id}. Known item ids include: {}",
        known.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::schema::ViolationCode;

    #[test]
    fn lint_report_round_trips_as_json() {
        let report = LintReport {
            source: PathBuf::from(".codegraph/planes.yaml"),
            planes: 1,
            items: 1,
            touches: 2,
            violations: vec![Violation {
                code: ViolationCode::EmptyTouches,
                message: "work item \"A-1\" has an empty \"touches\" list".to_string(),
            }],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let back: LintReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
        assert!(!back.is_clean());
    }

    #[test]
    fn lint_reads_the_file_and_reports_every_rule_it_breaks() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/planes/dependency-cycle.yaml");
        let report = lint(&path).expect("a parseable file lints rather than erroring");
        assert!(!report.is_clean(), "the cycle fixture must report a problem");
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.code == ViolationCode::DependencyCycle),
            "expected a dependency-cycle violation, got {:?}",
            report.violations
        );
    }

    #[test]
    fn lint_counts_a_clean_file_without_complaining() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/planes/valid-multi-plane.yaml");
        let report = lint(&path).expect("the reference fixture must lint");
        assert!(report.is_clean(), "{:?}", report.violations);
        assert_eq!(report.planes, 2);
        assert_eq!(report.items, 4);
        assert_eq!(report.touches, 6);
    }

    #[test]
    fn an_unbound_touch_serializes_as_a_touch_plus_its_reason() {
        let touch = crate::plan::model::Touch::new(Selector::File, "src/gone.rs");
        let unbound = TouchView {
            touch: ResolvedTouch::unresolved(&touch),
            indexed: None,
            reason: Some(UnresolvedReason::NoSuchFile),
            detail: Some("src/gone.rs is not there".to_string()),
        };
        let json: serde_json::Value = serde_json::to_value(&unbound).expect("serialize");
        // The flattened shape is what makes this additive: a reader of the
        // touch fields sees exactly what it saw before the reason existed.
        assert_eq!(json["selector"], "file");
        assert_eq!(json["raw"], "src/gone.rs");
        assert_eq!(json["confidence"], "UNRESOLVED");
        assert_eq!(json["reason"], "no_such_file");
        assert!(json.get("indexed").is_none(), "absent, not null: {json}");
        let back: TouchView = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, unbound);
    }

    #[test]
    fn a_real_but_unindexed_path_renders_as_bound_and_not_indexed() {
        let touch = crate::plan::model::Touch::new(Selector::File, "specs/plan.md");
        let view = TouchView {
            touch: ResolvedTouch::resolved(&touch, "specs/plan.md"),
            indexed: Some(false),
            reason: None,
            detail: None,
        };
        assert!(view.is_unindexed());
        let rendered = view.to_string();
        assert!(rendered.contains("RESOLVED"), "{rendered}");
        assert!(rendered.contains("(not indexed)"), "{rendered}");
        let json: serde_json::Value = serde_json::to_value(&view).expect("serialize");
        assert_eq!(json["confidence"], "RESOLVED");
        assert_eq!(json["indexed"], false);
    }
}
