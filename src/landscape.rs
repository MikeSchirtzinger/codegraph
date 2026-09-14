//! `codegraph landscape` and `codegraph plan brief`. **Owned by lane L3.**
//!
//! ## What this renders
//!
//! A hypergraph of the codebase as it is, with the roadmap overlaid on it.
//!
//! The code half is a partition of the indexed files into **subsystems**.
//! Each subsystem is a hyperedge over a set of files: one box, many files,
//! named by the directory prefix they share. The edges between subsystems
//! are the incidence structure between those hyperedges, counted from the
//! resolved cross-file bindings and from the derived `file_ref` graph the
//! resolver writes from those same bindings.
//!
//! The roadmap half is the same shape from the other direction. A work item
//! in `.codegraph/planes.yaml` is a hyperedge over a set of code entities
//! (its `touches` list). Placing each touch into a subsystem overlays one
//! hypergraph on the other, which is what lets an agent ask "what planned
//! work lands in the part of the tree I am about to edit".
//!
//! ## How the boxes are drawn
//!
//! The partition rule is deterministic and stated in the output rather than
//! inferred by the reader. See [`PartitionRule`] for the rule itself and
//! [`PartitionRule::describe`] for the sentence the renderings print.
//!
//! ## Two numbers per subsystem edge, and why
//!
//! `channels` counts distinct (from file, to file) dependency pairs between
//! two subsystems. `references` counts the resolved name edges behind those
//! pairs, with multiplicity. They come from the same fact base at two
//! granularities, so they are reported side by side rather than summed:
//! adding them would double count, and reporting only one would hide either
//! how wide the coupling is or how heavy it is.
//!
//! ## Contract with `src/main.rs`
//!
//! `src/main.rs` is owned by lane P0. For [`run`] it prints
//! [`LandscapeOutput::rendered`] when [`LandscapeOutput::written_to`] is
//! `None`, and a one-line confirmation otherwise. For [`brief`] it prints
//! the returned string. Neither signature changes here, and neither does
//! [`LandscapeOptions`], which `main.rs` builds as a struct literal.
//!
//! `LandscapeFormat::Text` renders markdown. The CLI calls the format
//! `text` because it is the default a terminal gets; the bytes are markdown
//! so the same rendering can be pasted into a document or a pull request
//! without a second exporter.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::graph::{self, NAME_EDGE_TYPES};
use crate::plan::export;
use crate::plan::model::{Horizon, Kind, PlaneFile, Selector, Status};
use crate::plan::{self, schema};

/// How to render a landscape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LandscapeFormat {
    /// Human-readable text for a terminal.
    Text,
    /// A typed JSON document, for an agent.
    Json,
    /// A mermaid graph, for a markdown document that renders it.
    Mermaid,
    /// Graphviz DOT, for anything that draws a real layout.
    Dot,
}

impl LandscapeFormat {
    /// The exact string this format is written as.
    pub fn as_str(self) -> &'static str {
        match self {
            LandscapeFormat::Text => "text",
            LandscapeFormat::Json => "json",
            LandscapeFormat::Mermaid => "mermaid",
            LandscapeFormat::Dot => "dot",
        }
    }

    /// Every accepted format, in the order error messages list them.
    pub const ALL: [LandscapeFormat; 4] = [
        LandscapeFormat::Text,
        LandscapeFormat::Json,
        LandscapeFormat::Mermaid,
        LandscapeFormat::Dot,
    ];

    /// Comma-separated list of accepted formats, for error messages.
    pub fn accepted() -> String {
        LandscapeFormat::ALL
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for LandscapeFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LandscapeFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(LandscapeFormat::Text),
            "json" => Ok(LandscapeFormat::Json),
            "mermaid" => Ok(LandscapeFormat::Mermaid),
            "dot" => Ok(LandscapeFormat::Dot),
            other => Err(format!(
                "unknown landscape format \"{other}\": accepted formats are {}",
                LandscapeFormat::accepted()
            )),
        }
    }
}

/// What to render and where to put it.
///
/// `src/main.rs` builds this as a struct literal, so its field set is part
/// of the P0 contract and does not grow here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandscapeOptions {
    /// Project to render.
    pub project_id: String,
    /// Output format.
    pub format: LandscapeFormat,
    /// File to write to. `None` means write to stdout.
    pub output: Option<PathBuf>,
}

/// A rendered landscape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandscapeOutput {
    /// The rendering, always produced, whether or not it was also written to
    /// a file. Held rather than streamed so a caller can hash it, diff it,
    /// or hand it to something else.
    pub rendered: String,
    /// Where it was written, when [`LandscapeOptions::output`] was set.
    pub written_to: Option<PathBuf>,
}

// ============================================================================
// The partition rule
// ============================================================================

/// Name of the subsystem holding files that sit directly at the repo root.
pub const ROOT_SUBSYSTEM: &str = ".";

/// How the directory tree is cut into subsystems.
///
/// The rule is deliberately dull. A reader must be able to look at a box on
/// the map and know why it is there without reading this file, which is why
/// [`describe`](PartitionRule::describe) is printed in every rendering's
/// header.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PartitionRule {
    /// A subsystem holding a greater share of the project's files than this
    /// is split one level deeper.
    pub max_share: f64,
    /// Hard limit on how many path segments a subsystem name may have. Stops
    /// the split loop regardless of share.
    pub depth_cap: usize,
    /// A subsystem with fewer files than this folds into its parent, when
    /// its parent is also a subsystem.
    pub min_files: usize,
}

impl Default for PartitionRule {
    /// A map whose biggest box holds more than a quarter of the codebase
    /// has not partitioned anything, so the share threshold is 25%: at
    /// most four subsystems can be "big", and the rest of the tree has to
    /// earn its boxes. Measured on this repo at 35% the whole of `src`
    /// stayed one box at 31% of the files while the test fixture
    /// directories got eight boxes between them, which is the map upside
    /// down.
    fn default() -> Self {
        PartitionRule {
            max_share: 0.25,
            depth_cap: 3,
            min_files: 2,
        }
    }
}

impl PartitionRule {
    /// The rule as one paragraph, printed in every rendering's header so the
    /// boxes never have to be reverse engineered.
    pub fn describe(&self) -> String {
        format!(
            "Subsystems are directory prefixes. Every file starts in its top level directory. \
             Any subsystem holding more than {:.0}% of the project's files is split one level \
             deeper, to a cap of {} path segments. A subsystem left with fewer than {} files \
             folds into its parent when the parent is also a subsystem. Files at the repo root \
             are grouped as \"{}\".",
            self.max_share * 100.0,
            self.depth_cap,
            self.min_files,
            ROOT_SUBSYSTEM
        )
    }
}

/// Directory segments of a repo-relative path, file name dropped.
fn dir_segments(path: &str) -> Vec<&str> {
    let mut segs: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    segs.pop();
    segs
}

/// The subsystem name a path would carry if cut at `depth` segments.
fn prefix_at(path: &str, depth: usize) -> String {
    let segs = dir_segments(path);
    if segs.is_empty() {
        return ROOT_SUBSYSTEM.to_string();
    }
    segs[..depth.min(segs.len())].join("/")
}

/// Segment count of a subsystem name. The root pseudo-directory is depth 0.
fn name_depth(name: &str) -> usize {
    if name == ROOT_SUBSYSTEM {
        0
    } else {
        name.split('/').count()
    }
}

/// The subsystem name one level up, or `None` at depth 1 or 0.
fn parent_name(name: &str) -> Option<String> {
    if name_depth(name) < 2 {
        return None;
    }
    name.rsplit_once('/').map(|(head, _)| head.to_string())
}

/// A file set cut into subsystems by [`PartitionRule`].
///
/// Every file lands in exactly one subsystem. That is what makes the
/// subsystems a partition and therefore a well-formed set of hyperedges: an
/// incidence between two subsystems is then an unambiguous fact about two
/// disjoint file sets, not an artifact of a file being counted twice.
#[derive(Debug, Clone)]
pub struct Partition {
    rule: PartitionRule,
    members: BTreeMap<String, Vec<String>>,
    owner: HashMap<String, String>,
    /// Subsystem names, longest first, for prefix placement of paths that
    /// were never indexed (a `.surql` file, a spec, a directory glob).
    by_length: Vec<String>,
}

impl Partition {
    /// Cut `files` into subsystems. Input order does not matter: the file
    /// list is sorted and deduplicated first, and every internal map is
    /// ordered, so two runs over the same set produce the same partition.
    pub fn build(files: &[String], rule: PartitionRule) -> Partition {
        let mut unique: Vec<String> = files.to_vec();
        unique.sort();
        unique.dedup();
        let total = unique.len();

        let mut names: Vec<String> = unique.iter().map(|f| prefix_at(f, 1)).collect();

        if total > 0 {
            // Split oversized subsystems one level at a time. Each round
            // strictly increases the depth of at least one subsystem, and
            // depth is capped, so this terminates.
            for _ in 0..rule.depth_cap {
                let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for (i, nm) in names.iter().enumerate() {
                    groups.entry(nm.clone()).or_default().push(i);
                }

                let mut split_any = false;
                for (nm, idxs) in &groups {
                    if (idxs.len() as f64) / (total as f64) <= rule.max_share {
                        continue;
                    }
                    let depth = name_depth(nm);
                    if depth == 0 || depth >= rule.depth_cap {
                        continue;
                    }
                    // A descent only means something if some member file
                    // actually has a deeper directory segment. Without this
                    // guard a directory whose files all sit directly in it
                    // would be "split" into itself forever.
                    if !idxs
                        .iter()
                        .any(|&i| dir_segments(&unique[i]).len() > depth)
                    {
                        continue;
                    }
                    for &i in idxs {
                        if dir_segments(&unique[i]).len() > depth {
                            names[i] = prefix_at(&unique[i], depth + 1);
                        }
                    }
                    split_any = true;
                }
                if !split_any {
                    break;
                }
            }
        }

        let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (i, nm) in names.iter().enumerate() {
            members.entry(nm.clone()).or_default().push(unique[i].clone());
        }

        // Fold undersized subsystems upward, deepest first so a chain folds
        // all the way. A depth-1 subsystem has no parent on the map and
        // stays: folding it into the root group would claim its files sit at
        // the repo root, which is false.
        loop {
            let mut candidates: Vec<String> = members
                .iter()
                .filter(|(nm, fs)| fs.len() < rule.min_files && name_depth(nm) >= 2)
                .map(|(nm, _)| nm.clone())
                .collect();
            candidates.sort_by_key(|nm| std::cmp::Reverse(name_depth(nm)));

            let mut folded = false;
            for nm in candidates {
                let still_small = members
                    .get(&nm)
                    .is_some_and(|fs| fs.len() < rule.min_files);
                if !still_small {
                    continue;
                }
                let Some(parent) = parent_name(&nm) else {
                    continue;
                };
                if !members.contains_key(&parent) {
                    continue;
                }
                let moved = members.remove(&nm).unwrap_or_default();
                if let Some(target) = members.get_mut(&parent) {
                    target.extend(moved);
                }
                folded = true;
            }
            if !folded {
                break;
            }
        }

        let mut owner = HashMap::with_capacity(total);
        for (nm, fs) in members.iter_mut() {
            fs.sort();
            for f in fs.iter() {
                owner.insert(f.clone(), nm.clone());
            }
        }

        let mut by_length: Vec<String> = members.keys().cloned().collect();
        by_length.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

        Partition {
            rule,
            members,
            owner,
            by_length,
        }
    }

    /// The rule this partition was built with.
    pub fn rule(&self) -> &PartitionRule {
        &self.rule
    }

    /// Subsystem names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.members.keys()
    }

    /// How many subsystems the partition has.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// True when nothing was indexed.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The files of one subsystem, sorted.
    pub fn members(&self, name: &str) -> &[String] {
        self.members.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The subsystem owning an indexed file. Exact lookup only.
    pub fn subsystem_of(&self, path: &str) -> Option<&str> {
        self.owner.get(path).map(String::as_str)
    }

    /// The subsystem a path belongs to, whether or not the file was indexed.
    ///
    /// Falls back to the longest subsystem name that is a directory prefix
    /// of the path, so a touch naming a file codegraph does not parse (a
    /// `.surql` schema, a spec, a config) still lands on the map instead of
    /// being dropped.
    pub fn place_path(&self, path: &str) -> Option<&str> {
        let path = path.trim_start_matches("./");
        if let Some(found) = self.owner.get(path) {
            return Some(found.as_str());
        }
        for name in &self.by_length {
            if name == ROOT_SUBSYSTEM {
                continue;
            }
            if path.len() > name.len()
                && path.starts_with(name.as_str())
                && path.as_bytes()[name.len()] == b'/'
            {
                return Some(name.as_str());
            }
        }
        if !path.contains('/') && self.members.contains_key(ROOT_SUBSYSTEM) {
            return Some(ROOT_SUBSYSTEM);
        }
        None
    }
}

// ============================================================================
// The typed model every format renders
// ============================================================================

/// The whole landscape, code half and roadmap half.
///
/// This is what `--format json` serializes and what every other exporter in
/// [`crate::plan::export`] is a pure function of. Field names are part of
/// the agent-facing contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Landscape {
    /// Project this describes.
    pub project_id: String,
    /// The partition rule, echoed so a rendering can print it and a reader
    /// of the JSON can reproduce the boxes.
    pub rule: PartitionRule,
    /// One sentence version of `rule`.
    pub rule_description: String,
    /// Indexed files.
    pub file_count: usize,
    /// `code_node` rows.
    pub node_count: usize,
    /// The subsystems, sorted by name. Each is a hyperedge over its files.
    pub subsystems: Vec<Subsystem>,
    /// Directed incidences between subsystems, sorted by (from, to).
    pub edges: Vec<SubsystemEdge>,
    /// Subsystem pairs that depend on each other in both directions.
    pub cycles: Vec<SubsystemCycle>,
    /// The roadmap overlaid on the subsystems.
    pub roadmap: Roadmap,
}

/// One subsystem: a set of files, and what that set looks like from outside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subsystem {
    /// Directory prefix that names it.
    pub name: String,
    /// Files in the set.
    pub file_count: usize,
    /// `code_node` rows across those files.
    pub node_count: usize,
    /// Distinct file-to-file dependency pairs that stay inside the set.
    pub internal_channels: usize,
    /// Distinct dependency pairs arriving from other subsystems.
    pub afferent: usize,
    /// Distinct dependency pairs leaving for other subsystems.
    pub efferent: usize,
    /// `efferent / (afferent + efferent)`, the same instability ratio
    /// [`crate::graph::coupling`] computes per file, computed here over the
    /// subsystem's inter-subsystem channels. 0.0 is maximally stable, 1.0
    /// maximally unstable. `None` when the subsystem has no channels at all
    /// and the ratio would be undefined rather than zero.
    pub instability: Option<f64>,
    /// Highest-degree nodes inside the set, by total degree.
    pub hubs: Vec<Hub>,
    /// The most coupled file in the set, from [`crate::graph::coupling`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub most_coupled_file: Option<String>,
    /// Work item ids whose touches land here, sorted.
    pub items: Vec<String>,
}

/// One hub node, projected from [`crate::graph::hub_nodes::HubNode`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hub {
    /// Symbol name.
    pub name: String,
    /// `function`, `struct`, and so on.
    pub node_type: String,
    /// File it is defined in.
    pub file_path: String,
    /// In-degree plus out-degree over name edges.
    pub degree: i64,
}

/// A directed dependency from one subsystem to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemEdge {
    /// Depending subsystem.
    pub from: String,
    /// Depended-on subsystem.
    pub to: String,
    /// Distinct (from file, to file) pairs. This is the incidence count.
    pub channels: usize,
    /// Resolved name edges behind those pairs, with multiplicity.
    pub references: usize,
    /// Which edge kinds contributed, sorted. `file_ref` means the pair was
    /// present in the derived file-level graph.
    pub via: Vec<String>,
}

/// Two subsystems that depend on each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemCycle {
    /// First subsystem, the lexicographically smaller of the pair.
    pub a: String,
    /// Second subsystem.
    pub b: String,
    /// Channels from `a` to `b`.
    pub a_to_b: usize,
    /// Channels from `b` to `a`.
    pub b_to_a: usize,
    /// File-level cycles inside this subsystem pair, from
    /// [`crate::graph::circular`]. Empty means the subsystems form a cycle
    /// without any single file pair doing so on its own.
    pub file_cycles: Vec<FileCycle>,
}

/// One file-level cycle, straight from [`crate::graph::circular`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCycle {
    /// One file.
    pub file_a: String,
    /// The other.
    pub file_b: String,
    /// Edge kinds that produced it.
    pub via: Vec<String>,
}

/// The roadmap half of the landscape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Roadmap {
    /// Where the roadmap came from, or why there is none.
    pub source: RoadmapSource,
    /// Path consulted, whether or not it was there.
    pub path: String,
    /// The planes, in file order.
    pub planes: Vec<PlaneOverlay>,
    /// Touches that named nothing the partition could place.
    pub unplaced: Vec<UnplacedTouch>,
}

impl Roadmap {
    /// An empty roadmap that records why it is empty.
    fn absent(path: &Path, source: RoadmapSource) -> Roadmap {
        Roadmap {
            source,
            path: path.display().to_string(),
            planes: Vec::new(),
            unplaced: Vec::new(),
        }
    }

    /// Every item across every plane, in file order.
    pub fn items(&self) -> impl Iterator<Item = (&PlaneOverlay, &ItemOverlay)> {
        self.planes
            .iter()
            .flat_map(|p| p.items.iter().map(move |i| (p, i)))
    }
}

/// Why the roadmap section says what it says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum RoadmapSource {
    /// The file was read and validated.
    Loaded,
    /// There is no planes file. Not an error: a project may not have one.
    Missing,
    /// There is a planes file and it did not load. The landscape still
    /// renders, and says so rather than failing the command.
    Invalid {
        /// The parser's own message, unedited.
        reason: String,
    },
}

/// One plane, with its items placed onto the subsystem map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneOverlay {
    /// Plane id from the file.
    pub id: String,
    /// Human title.
    pub title: String,
    /// `planned` / `active` / `done` / `abandoned`.
    pub status: Status,
    /// `now` / `next` / `later`.
    pub horizon: Horizon,
    /// Optional prose summary from the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The plane's work items, in file order.
    pub items: Vec<ItemOverlay>,
}

/// One work item: the hyperedge, with each incidence placed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemOverlay {
    /// Work item id from the file.
    pub id: String,
    /// Plane the item belongs to.
    pub plane: String,
    /// Human title.
    pub title: String,
    /// `feature` / `fix` / `perf` / `refactor` / `docs` / `research`.
    pub kind: Kind,
    /// Lifecycle state.
    pub status: Status,
    /// Items that must land first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Spec that governs the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// The incidence list, in file order.
    pub touches: Vec<TouchPlacement>,
    /// Subsystems this item lands in, sorted and deduplicated.
    pub subsystems: Vec<String>,
}

/// One touch, and where on the map it landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchPlacement {
    /// `file` / `symbol` / `glob`.
    pub selector: Selector,
    /// Verbatim from the file.
    pub raw: String,
    /// Concrete repo-relative paths this touch resolved to. A `file:` touch
    /// gives one, a `glob:` gives every indexed file it matched, a `symbol:`
    /// gives the file of every definition that answered to the name.
    pub paths: Vec<String>,
    /// Subsystems those paths fall in, sorted and deduplicated.
    pub subsystems: Vec<String>,
}

/// Why a touch could not be placed.
///
/// The distinction is the point of reporting these rather than dropping
/// them, and it decides what to do about each one. A path that is on disk
/// but outside the indexed tree (a README, a spec, a design doc) is a
/// perfectly good plan this map cannot draw, because the map is built from
/// the code graph. A path that is not on disk at all is a stale plan, which
/// is the same signal an UNRESOLVED code reference carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnplacedCause {
    /// On disk, not indexed. Documentation, specs, configuration.
    OutsideIndexedTree,
    /// Not on disk. A stale plan.
    NotOnDisk,
    /// A `symbol:` touch no indexed definition answers to.
    NoDefinition,
}

impl UnplacedCause {
    /// The short label a report prints next to the touch.
    pub fn as_str(self) -> &'static str {
        match self {
            UnplacedCause::OutsideIndexedTree => "outside the indexed tree",
            UnplacedCause::NotOnDisk => "not on disk, so this plan is stale",
            UnplacedCause::NoDefinition => "no indexed definition answers to this name",
        }
    }

    /// True when this is the staleness signal rather than a coverage gap.
    pub fn is_stale(self) -> bool {
        matches!(
            self,
            UnplacedCause::NotOnDisk | UnplacedCause::NoDefinition
        )
    }
}

impl fmt::Display for UnplacedCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A touch the partition could not place, kept rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnplacedTouch {
    /// Item that owns the touch.
    pub item: String,
    /// `file` / `symbol` / `glob`.
    pub selector: Selector,
    /// Verbatim from the file.
    pub raw: String,
    /// Which of the three reasons it is.
    pub cause: UnplacedCause,
}

// ============================================================================
// Entry points
// ============================================================================

/// Render the codebase hypergraph with the planes overlaid.
pub async fn run(db: &Surreal<Any>, options: &LandscapeOptions) -> Result<LandscapeOutput> {
    // `main.rs` hands this a `&Surreal<Any>` (deref of the `Arc` that
    // `db::connect` returns) while `graph::hub_nodes`, `graph::coupling`,
    // and `graph::circular` all take `&Arc<Surreal<Any>>`. Cloning the
    // handle re-wraps the same underlying connection rather than opening a
    // second one, which keeps the P0 signature and the graph module's
    // signatures both untouched.
    let handle = Arc::new(db.clone());
    let landscape = build(&handle, &options.project_id).await?;

    let rendered = match options.format {
        LandscapeFormat::Text => export::markdown(&landscape),
        LandscapeFormat::Json => export::json(&landscape)?,
        LandscapeFormat::Mermaid => export::mermaid(&landscape),
        LandscapeFormat::Dot => export::dot(&landscape),
    };

    let written_to = match &options.output {
        None => None,
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("cannot create the directory for {}", path.display())
                    })?;
                }
            }
            std::fs::write(path, &rendered)
                .with_context(|| format!("cannot write the landscape to {}", path.display()))?;
            Some(path.clone())
        }
    };

    Ok(LandscapeOutput {
        rendered,
        written_to,
    })
}

/// Render the compact, agent-pasteable markdown brief of the landscape.
pub async fn brief(db: &Surreal<Any>, project_id: &str) -> Result<String> {
    let handle = Arc::new(db.clone());
    let landscape = build(&handle, project_id).await?;
    Ok(export::brief(&landscape))
}

/// Build the typed landscape for a project, reading the roadmap from the
/// default `.codegraph/planes.yaml` under the current directory.
pub async fn build(db: &Arc<Surreal<Any>>, project_id: &str) -> Result<Landscape> {
    let planes_path = plan::default_planes_path(Path::new("."));
    build_with_planes(db, project_id, &planes_path, PartitionRule::default()).await
}

/// [`build`], with the planes file and the partition rule named explicitly.
///
/// Tests point this at a fixture; `codegraph context` points it at the same
/// default path `build` uses.
pub async fn build_with_planes(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    planes_path: &Path,
    rule: PartitionRule,
) -> Result<Landscape> {
    let nodes = graph::load_project_nodes(db, project_id)
        .await
        .context("loading project nodes for the landscape failed")?;

    let mut files: BTreeSet<String> = nodes
        .iter()
        .filter(|n| !n.file_path.is_empty())
        .map(|n| n.file_path.clone())
        .collect();
    files.extend(indexed_files(db, project_id).await?);

    let file_list: Vec<String> = files.into_iter().collect();
    let partition = Partition::build(&file_list, rule);

    // ---- the code half -------------------------------------------------
    let node_file: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.file_path.as_str()))
        .collect();

    let name_edges = graph::load_project_edges(db, project_id, &NAME_EDGE_TYPES)
        .await
        .context("loading project edges for the landscape failed")?;

    // (from subsystem, to subsystem) -> channels, references, edge kinds.
    let mut pair_channels: BTreeMap<(String, String), BTreeSet<(String, String)>> = BTreeMap::new();
    let mut pair_refs: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut pair_via: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();

    for edge in &name_edges {
        if edge.confidence != "RESOLVED" || edge.to_id.is_empty() {
            continue;
        }
        let (Some(from_file), Some(to_file)) = (
            node_file.get(edge.from_id.as_str()),
            node_file.get(edge.to_id.as_str()),
        ) else {
            continue;
        };
        if from_file == to_file {
            continue;
        }
        let (Some(from_sub), Some(to_sub)) = (
            partition.subsystem_of(from_file),
            partition.subsystem_of(to_file),
        ) else {
            continue;
        };
        let key = (from_sub.to_string(), to_sub.to_string());
        pair_channels
            .entry(key.clone())
            .or_default()
            .insert((from_file.to_string(), to_file.to_string()));
        *pair_refs.entry(key.clone()).or_default() += 1;
        pair_via
            .entry(key)
            .or_default()
            .insert(edge.edge_type.clone());
    }

    for (from_file, to_file) in file_ref_pairs(db, project_id).await? {
        if from_file == to_file {
            continue;
        }
        let (Some(from_sub), Some(to_sub)) = (
            partition.subsystem_of(&from_file),
            partition.subsystem_of(&to_file),
        ) else {
            continue;
        };
        let key = (from_sub.to_string(), to_sub.to_string());
        pair_channels
            .entry(key.clone())
            .or_default()
            .insert((from_file, to_file));
        pair_via
            .entry(key)
            .or_default()
            .insert("file_ref".to_string());
    }

    let mut edges = Vec::new();
    let mut internal: BTreeMap<String, usize> = BTreeMap::new();
    let mut afferent: BTreeMap<String, usize> = BTreeMap::new();
    let mut efferent: BTreeMap<String, usize> = BTreeMap::new();

    for ((from, to), channels) in &pair_channels {
        let count = channels.len();
        if from == to {
            *internal.entry(from.clone()).or_default() += count;
            continue;
        }
        *efferent.entry(from.clone()).or_default() += count;
        *afferent.entry(to.clone()).or_default() += count;
        edges.push(SubsystemEdge {
            from: from.clone(),
            to: to.clone(),
            channels: count,
            references: pair_refs.get(&(from.clone(), to.clone())).copied().unwrap_or(0),
            via: pair_via
                .get(&(from.clone(), to.clone()))
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default(),
        });
    }
    edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));

    // ---- cycles ---------------------------------------------------------
    let file_cycles = graph::circular::detect_circular_deps(db, project_id)
        .await
        .context("circular dependency query failed")?;

    let mut cycle_files: BTreeMap<(String, String), Vec<FileCycle>> = BTreeMap::new();
    for c in &file_cycles {
        let (Some(sub_a), Some(sub_b)) = (
            partition.subsystem_of(&c.file_a),
            partition.subsystem_of(&c.file_b),
        ) else {
            continue;
        };
        if sub_a == sub_b {
            continue;
        }
        let key = ordered_pair(sub_a, sub_b);
        cycle_files.entry(key).or_default().push(FileCycle {
            file_a: c.file_a.clone(),
            file_b: c.file_b.clone(),
            via: c.via.clone(),
        });
    }

    let mut cycles = Vec::new();
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    for edge in &edges {
        let key = ordered_pair(&edge.from, &edge.to);
        if !seen_pairs.insert(key.clone()) {
            continue;
        }
        let forward = pair_channels
            .get(&(key.0.clone(), key.1.clone()))
            .map(BTreeSet::len)
            .unwrap_or(0);
        let backward = pair_channels
            .get(&(key.1.clone(), key.0.clone()))
            .map(BTreeSet::len)
            .unwrap_or(0);
        if forward == 0 || backward == 0 {
            continue;
        }
        let mut found = cycle_files.get(&key).cloned().unwrap_or_default();
        found.sort_by(|x, y| {
            x.file_a
                .cmp(&y.file_a)
                .then_with(|| x.file_b.cmp(&y.file_b))
        });
        cycles.push(SubsystemCycle {
            a: key.0.clone(),
            b: key.1.clone(),
            a_to_b: forward,
            b_to_a: backward,
            file_cycles: found,
        });
    }
    cycles.sort_by(|x, y| x.a.cmp(&y.a).then_with(|| x.b.cmp(&y.b)));

    // ---- per-subsystem metrics -------------------------------------------
    let mut nodes_per_file: HashMap<&str, usize> = HashMap::new();
    for n in &nodes {
        *nodes_per_file.entry(n.file_path.as_str()).or_default() += 1;
    }

    let hub_limit = nodes.len().max(1);
    let hubs = graph::hub_nodes::find_hub_nodes(db, project_id, hub_limit)
        .await
        .context("hub node query failed")?;
    // `find_hub_nodes` sorts by degree alone, and its input row order is
    // whatever the store returned, so equal-degree nodes can come back in
    // either order. Re-sort on the full key before bucketing: a landscape
    // that changes between two runs of the same index is not a map.
    let mut ranked: Vec<&graph::hub_nodes::HubNode> =
        hubs.iter().filter(|h| h.total_degree > 0).collect();
    ranked.sort_by(|a, b| {
        b.total_degree
            .cmp(&a.total_degree)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.file_path.cmp(&b.file_path))
    });

    let mut hubs_by_subsystem: HashMap<&str, Vec<Hub>> = HashMap::new();
    for h in ranked {
        let Some(sub) = partition.subsystem_of(&h.file_path) else {
            continue;
        };
        let bucket = hubs_by_subsystem.entry(sub).or_default();
        if bucket.len() < HUBS_PER_SUBSYSTEM {
            bucket.push(Hub {
                name: h.name.clone(),
                node_type: h.node_type.clone(),
                file_path: h.file_path.clone(),
                degree: h.total_degree,
            });
        }
    }

    let coupling = graph::coupling::calculate_file_coupling(db, project_id, file_list.len().max(1))
        .await
        .context("file coupling query failed")?;
    // `calculate_file_coupling` returns ties in whatever order its internal
    // maps produced, so the winner is broken on the path as well as the
    // score. Without that tie-break two runs over one index disagree, which
    // `tests/landscape.rs` catches.
    let mut top_coupled: HashMap<&str, (&str, i64)> = HashMap::new();
    for c in &coupling {
        let Some(sub) = partition.subsystem_of(&c.file_path) else {
            continue;
        };
        let score = c.afferent + c.efferent;
        let candidate = (c.file_path.as_str(), score);
        let entry = top_coupled.entry(sub).or_insert(candidate);
        if score > entry.1 || (score == entry.1 && candidate.0 < entry.0) {
            *entry = candidate;
        }
    }

    // ---- the roadmap half -------------------------------------------------
    let roadmap = load_roadmap(db, project_id, planes_path, &partition, &nodes).await?;

    let mut items_by_subsystem: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (_, item) in roadmap.items() {
        for sub in &item.subsystems {
            items_by_subsystem
                .entry(sub.as_str())
                .or_default()
                .insert(item.id.as_str());
        }
    }

    let subsystems = partition
        .names()
        .map(|name| {
            let files = partition.members(name);
            let node_count = files
                .iter()
                .map(|f| nodes_per_file.get(f.as_str()).copied().unwrap_or(0))
                .sum();
            let aff = afferent.get(name).copied().unwrap_or(0);
            let eff = efferent.get(name).copied().unwrap_or(0);
            Subsystem {
                name: name.clone(),
                file_count: files.len(),
                node_count,
                internal_channels: internal.get(name).copied().unwrap_or(0),
                afferent: aff,
                efferent: eff,
                instability: (aff + eff > 0).then(|| eff as f64 / (aff + eff) as f64),
                hubs: hubs_by_subsystem
                    .get(name.as_str())
                    .cloned()
                    .unwrap_or_default(),
                most_coupled_file: top_coupled
                    .get(name.as_str())
                    .map(|(path, _)| (*path).to_string()),
                items: items_by_subsystem
                    .get(name.as_str())
                    .map(|ids| ids.iter().map(|s| (*s).to_string()).collect())
                    .unwrap_or_default(),
            }
        })
        .collect();

    Ok(Landscape {
        project_id: project_id.to_string(),
        rule,
        rule_description: rule.describe(),
        file_count: file_list.len(),
        node_count: nodes.len(),
        subsystems,
        edges,
        cycles,
        roadmap,
    })
}

/// How many hub nodes each subsystem reports.
pub const HUBS_PER_SUBSYSTEM: usize = 3;

/// Rows in the context file's most-coupled-files table.
pub const CONTEXT_COUPLING_ROWS: usize = 15;

/// Build the body of `.codegraph/context.md`: the session bootstrap
/// document an agent reads before it starts work.
///
/// Five sections, in the order an agent needs them. The first three are the
/// pure `code_node` / `code_edge` summary this file has always produced, so
/// nothing that already reads them breaks. The last two are the board's
/// section 3.4 integration: the landscape, then the roadmap overlaid on it.
///
/// Both new sections come from one build of the landscape model, so the map
/// an agent reads and the roadmap it reads can never describe two different
/// partitions of the same tree.
///
/// This lives here rather than in `src/context.rs` because the landscape and
/// its embedding must not be able to drift into two different partitions of
/// one tree. `src/context.rs` writes what this returns and nothing else.
pub async fn context_markdown(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    planes_path: &Path,
) -> Result<String> {
    use std::fmt::Write as _;

    let mut md = String::with_capacity(8192);

    // Not "generated by": this file is read by people as well as agents,
    // and the house style bans generated-by trailers on anything a person
    // reads. The line still has to say the same thing, which is that the
    // file is derived and hand edits are lost.
    let _ = writeln!(
        md,
        "<!-- Derived file. codegraph writes it, and rewrites it whole on the next run. -->"
    );
    let _ = writeln!(
        md,
        "<!-- Regenerate: codegraph context --project-id {project_id} -->"
    );
    let _ = writeln!(md, "# Codegraph Context: {project_id}\n");

    let summary = graph::search::project_summary(db, project_id)
        .await
        .context("project_summary query failed")?;

    let _ = writeln!(md, "## Project Summary\n");
    let _ = writeln!(md, "**Node types:**\n");
    for (nt, c) in &summary.node_types {
        let _ = writeln!(md, "- {nt}: {c}");
    }
    let _ = writeln!(md, "\n**Languages:**\n");
    for (l, c) in &summary.languages {
        let _ = writeln!(md, "- {l}: {c}");
    }
    let _ = writeln!(md, "\n**Edge types:**\n");
    for (et, c) in &summary.edge_types {
        let _ = writeln!(md, "- {et}: {c}");
    }
    let _ = writeln!(md);

    let _ = writeln!(md, "## Hub Nodes (top 20)\n");
    let hubs = graph::hub_nodes::find_hub_nodes(db, project_id, 20)
        .await
        .context("find_hub_nodes query failed")?;
    for h in &hubs {
        let _ = writeln!(
            md,
            "- `{}` ({}) in:{} out:{} total:{} in `{}`",
            h.name, h.node_type, h.in_degree, h.out_degree, h.total_degree, h.file_path
        );
    }
    let _ = writeln!(md);

    let _ = writeln!(
        md,
        "## Most Coupled Files (top {CONTEXT_COUPLING_ROWS})\n"
    );
    // `calculate_file_coupling` orders by score alone and truncates inside
    // itself, over rows that arrive in hash-map order. Ties therefore come
    // back in a different order on each call, and because the truncation
    // happens before this code sees the list, a tie straddling the cut
    // changes *which* files appear, not merely their order. That made
    // `.codegraph/context.md` produce a spurious diff on every regeneration
    // of an unchanged repo, which is the opposite of what a committed,
    // agent-read file should do.
    //
    // Fixed here rather than at the source because `src/graph/coupling.rs`
    // belongs to another lane. Ask for every file, impose a total order, and
    // cut afterwards. The root cause is reported in this lane's receipt: the
    // CLI's own `coupling` output has the same exposure and this does not
    // fix that one.
    let mut coupling = graph::coupling::calculate_file_coupling(db, project_id, usize::MAX)
        .await
        .context("calculate_file_coupling query failed")?;
    coupling.sort_by(|a, b| {
        (b.afferent + b.efferent)
            .cmp(&(a.afferent + a.efferent))
            .then_with(|| a.file_path.cmp(&b.file_path))
    });
    coupling.truncate(CONTEXT_COUPLING_ROWS);
    let _ = writeln!(
        md,
        "Ca=afferent (depended on by), Ce=efferent (depends on), I=instability\n"
    );
    for c in &coupling {
        let _ = writeln!(
            md,
            "- `{}` Ca:{} Ce:{} I:{:.2} ({} nodes)",
            c.file_path, c.afferent, c.efferent, c.instability, c.node_count
        );
    }
    let _ = writeln!(md);

    let built = build_with_planes(db, project_id, planes_path, PartitionRule::default())
        .await
        .context("building the landscape for the context file failed")?;

    let _ = writeln!(md, "## Landscape\n");
    let _ = writeln!(
        md,
        "The codebase as a hypergraph: each subsystem is a set of files, and the table of \
         dependencies below is the incidence structure between those sets.\n"
    );
    let _ = writeln!(md, "{}", export::demote_headings_by(&export::markdown(&built), 2));

    let _ = writeln!(md, "## Roadmap\n");
    let _ = writeln!(
        md,
        "Planned work as hyperedges over the same code. An item's touches are the files and \
         symbols it will change, so a grep for a path you are about to edit finds the work \
         already covering it.\n"
    );
    let _ = writeln!(md, "{}", export::brief(&built));

    Ok(md)
}

/// Order a subsystem pair so a cycle is recorded once, not twice.
fn ordered_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Every file `file_metadata` recorded for the project.
///
/// `code_node` alone would miss a file that parsed to nothing, which is
/// exactly the kind of file a landscape should still draw.
async fn indexed_files(db: &Surreal<Any>, project_id: &str) -> Result<Vec<String>> {
    let mut resp = db
        .query("SELECT file_path FROM file_metadata WHERE project_id = $pid")
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading indexed file list failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;
    Ok(rows.iter().filter_map(|v| row_str(v, "file_path")).collect())
}

/// The derived file-level graph: one row per distinct cross-file dependency.
async fn file_ref_pairs(db: &Surreal<Any>, project_id: &str) -> Result<Vec<(String, String)>> {
    let mut resp = db
        .query(
            "SELECT from_file, to_file FROM code_edge \
             WHERE project_id = $pid AND edge_type = 'file_ref'",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading file_ref edges failed")?;
    let rows: Vec<surrealdb_types::Value> = resp.take(0)?;
    Ok(rows
        .iter()
        .filter_map(|v| Some((row_str(v, "from_file")?, row_str(v, "to_file")?)))
        .collect())
}

/// One non-empty string field out of a row.
fn row_str(value: &surrealdb_types::Value, key: &str) -> Option<String> {
    let surrealdb_types::Value::Object(obj) = value else {
        return None;
    };
    match obj.get(key) {
        Some(surrealdb_types::Value::String(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

// ============================================================================
// The roadmap overlay
// ============================================================================

/// Read the planes file and place every touch onto the subsystem map.
///
/// This reads `.codegraph/planes.yaml` directly rather than the
/// `work_touch` rows, so `landscape` works before `plan sync` has ever run.
/// Seam for lane L2: once `work_touch` rows exist, their persisted
/// `to_id`/`confidence` replace [`place_touch`]'s lookup, and the placement
/// becomes a join instead of a search. The rendering above this line does
/// not change when that happens.
async fn load_roadmap(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    planes_path: &Path,
    partition: &Partition,
    nodes: &[crate::index::resolve::ResolverNode],
) -> Result<Roadmap> {
    let root = repo_root_for(planes_path);
    if !planes_path.exists() {
        return Ok(Roadmap::absent(planes_path, RoadmapSource::Missing));
    }
    let file: PlaneFile = match schema::load(planes_path) {
        Ok(file) => file,
        Err(err) => {
            return Ok(Roadmap::absent(
                planes_path,
                RoadmapSource::Invalid {
                    reason: format!("{err:#}"),
                },
            ))
        }
    };

    let by_id: HashMap<&str, &crate::index::resolve::ResolverNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut planes = Vec::with_capacity(file.planes.len());
    let mut unplaced = Vec::new();

    for plane in &file.planes {
        let mut items = Vec::with_capacity(plane.items.len());
        for item in &plane.items {
            let mut touches = Vec::with_capacity(item.touches.len());
            let mut subsystems: BTreeSet<String> = BTreeSet::new();

            for touch in &item.touches {
                let placement =
                    place_touch(db, project_id, touch, partition, &by_id, &root).await?;
                match placement {
                    Ok(p) => {
                        subsystems.extend(p.subsystems.iter().cloned());
                        touches.push(p);
                    }
                    Err(cause) => unplaced.push(UnplacedTouch {
                        item: item.id.clone(),
                        selector: touch.selector,
                        raw: touch.raw.clone(),
                        cause,
                    }),
                }
            }

            items.push(ItemOverlay {
                id: item.id.clone(),
                plane: plane.id.clone(),
                title: item.title.clone(),
                kind: item.kind,
                status: item.status,
                depends_on: item.depends_on.clone(),
                spec: item.spec.clone(),
                touches,
                subsystems: subsystems.into_iter().collect(),
            });
        }

        planes.push(PlaneOverlay {
            id: plane.id.clone(),
            title: plane.title.clone(),
            status: plane.status,
            horizon: plane.horizon,
            summary: plane.summary.clone(),
            items,
        });
    }

    Ok(Roadmap {
        source: RoadmapSource::Loaded,
        path: planes_path.display().to_string(),
        planes,
        unplaced,
    })
}

/// Place one touch, or say in one line why it could not be placed.
async fn place_touch(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    touch: &plan::Touch,
    partition: &Partition,
    by_id: &HashMap<&str, &crate::index::resolve::ResolverNode>,
    root: &Path,
) -> Result<std::result::Result<TouchPlacement, UnplacedCause>> {
    let paths: Vec<String> = match touch.selector {
        Selector::File => vec![touch.raw.trim_start_matches("./").to_string()],
        Selector::Glob => {
            let matched = match_glob(&touch.raw, partition);
            if matched.is_empty() {
                // Fall back to the literal prefix, so `src/index/**` still
                // lands on `src/index` when nothing under it was indexed.
                match glob_literal_prefix(&touch.raw) {
                    Some(prefix) => vec![prefix],
                    None => Vec::new(),
                }
            } else {
                matched
            }
        }
        Selector::Symbol => symbol_paths(db, project_id, &touch.raw, by_id).await?,
    };

    if paths.is_empty() {
        return Ok(Err(match touch.selector {
            Selector::Symbol => UnplacedCause::NoDefinition,
            Selector::File | Selector::Glob => UnplacedCause::NotOnDisk,
        }));
    }

    let mut subsystems: BTreeSet<String> = BTreeSet::new();
    for p in &paths {
        if let Some(sub) = partition.place_path(p) {
            subsystems.insert(sub.to_string());
        }
    }

    if subsystems.is_empty() {
        return Ok(Err(unplaced_cause(&paths, root)));
    }

    let mut paths: Vec<String> = paths;
    paths.sort();
    paths.dedup();

    Ok(Ok(TouchPlacement {
        selector: touch.selector,
        raw: touch.raw.clone(),
        paths,
        subsystems: subsystems.into_iter().collect(),
    }))
}

/// Repo root implied by the planes file's own location.
///
/// `.codegraph/planes.yaml` sits two levels under the root by construction
/// (see [`crate::plan::default_planes_path`]), so the file that defines the
/// roadmap also defines what its relative paths are relative to. Anchoring
/// here rather than on the process working directory means the same roadmap
/// reads the same way whatever directory the command was run from.
fn repo_root_for(planes_path: &Path) -> PathBuf {
    planes_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Classify a path-shaped touch that no subsystem covers.
///
/// A touch that matched at least one path on disk is a coverage gap in the
/// map; one that matched nothing on disk is a stale plan. See
/// [`UnplacedCause`].
fn unplaced_cause(paths: &[String], root: &Path) -> UnplacedCause {
    if paths.iter().any(|p| root.join(p).exists()) {
        UnplacedCause::OutsideIndexedTree
    } else {
        UnplacedCause::NotOnDisk
    }
}

/// Files holding a definition that answers to `symbol`.
///
/// The candidate set comes from [`crate::graph::search::search_nodes`], the
/// same substring search every other lookup in codegraph uses, and is then
/// narrowed: the node's bare name must equal the symbol's last segment, and
/// if the symbol was written with a path (`Commands::Index`) the node's
/// qualified name must end with it. Without that narrowing a touch on `run`
/// would claim every symbol whose name contains "run".
async fn symbol_paths(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    symbol: &str,
    by_id: &HashMap<&str, &crate::index::resolve::ResolverNode>,
) -> Result<Vec<String>> {
    let normalized = symbol.replace('.', "::");
    let last = normalized.rsplit("::").next().unwrap_or(&normalized);
    if last.is_empty() {
        return Ok(Vec::new());
    }

    let candidates = graph::search::search_nodes(db, project_id, last, None, SYMBOL_SEARCH_LIMIT)
        .await
        .with_context(|| format!("symbol search for {symbol:?} failed"))?;

    let mut paths: BTreeSet<String> = BTreeSet::new();
    for c in &candidates {
        if c.name != last {
            continue;
        }
        if normalized.contains("::") {
            let Some(node) = by_id.get(c.node_id.as_str()) else {
                continue;
            };
            let qn = node.qualified_name.as_str();
            if qn != normalized && !qn.ends_with(&format!("::{normalized}")) {
                continue;
            }
        }
        if !c.file_path.is_empty() {
            paths.insert(c.file_path.clone());
        }
    }
    Ok(paths.into_iter().collect())
}

/// Candidate ceiling for one `symbol:` touch. A name with more definitions
/// than this is too common to be a useful touch, and the report says so by
/// showing the subsystems it did land in.
pub const SYMBOL_SEARCH_LIMIT: usize = 500;

/// Indexed files matching a glob.
fn match_glob(pattern: &str, partition: &Partition) -> Vec<String> {
    let Some(re) = glob_regex(pattern) else {
        return Vec::new();
    };
    let mut hits: Vec<String> = Vec::new();
    for name in partition.names() {
        for file in partition.members(name) {
            if re.is_match(file) {
                hits.push(file.clone());
            }
        }
    }
    hits.sort();
    hits
}

/// Compile a path glob to a regex.
///
/// `**` crosses directory separators, `*` and `?` do not. Built on the
/// `regex` crate rather than a hand-rolled matcher so the matching itself is
/// something already tested. Kept local to this module: lane L4 carries its
/// own matcher for the MCP file filter, and unifying the two is a later
/// cleanup, not a cross-lane edit tonight.
fn glob_regex(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::with_capacity(pattern.len() * 2 + 4);
    out.push('^');
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    // `**/` matches zero or more leading directories.
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// The literal directory prefix of a glob, for placing a glob that matched
/// no indexed file. `src/index/**` gives `src/index`.
fn glob_literal_prefix(pattern: &str) -> Option<String> {
    let head = pattern
        .split('/')
        .take_while(|seg| !seg.contains('*') && !seg.contains('?'))
        .collect::<Vec<_>>()
        .join("/");
    (!head.is_empty()).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_format_lists_accepted_formats() {
        let err = LandscapeFormat::from_str("svg").expect_err("must reject");
        assert_eq!(
            err,
            "unknown landscape format \"svg\": accepted formats are text, json, mermaid, dot"
        );
    }

    #[test]
    fn top_level_directories_are_the_first_cut() {
        let p = Partition::build(
            &files(&["api/a.py", "api/b.py", "worker/main.go", "worker/util.go"]),
            PartitionRule::default(),
        );
        assert_eq!(
            p.names().cloned().collect::<Vec<_>>(),
            vec!["api".to_string(), "worker".to_string()]
        );
    }

    #[test]
    fn an_oversized_subsystem_is_split_one_level_deeper() {
        // src holds 6 of 7 files, well over the 35% share, so it splits.
        let p = Partition::build(
            &files(&[
                "src/graph/a.rs",
                "src/graph/b.rs",
                "src/index/c.rs",
                "src/index/d.rs",
                "src/plan/e.rs",
                "src/plan/f.rs",
                "README.md",
            ]),
            PartitionRule::default(),
        );
        let names: Vec<String> = p.names().cloned().collect();
        assert!(names.contains(&"src/graph".to_string()), "{names:?}");
        assert!(names.contains(&"src/index".to_string()), "{names:?}");
        assert!(names.contains(&"src/plan".to_string()), "{names:?}");
        assert!(names.contains(&ROOT_SUBSYSTEM.to_string()), "{names:?}");
        assert!(!names.contains(&"src".to_string()), "{names:?}");
    }

    #[test]
    fn a_directory_with_no_deeper_files_is_never_split_forever() {
        // Every file sits directly in src/, so no descent is possible even
        // though src/ holds 100% of the files.
        let p = Partition::build(
            &files(&["src/a.rs", "src/b.rs", "src/c.rs"]),
            PartitionRule::default(),
        );
        assert_eq!(p.names().cloned().collect::<Vec<_>>(), vec!["src".to_string()]);
    }

    #[test]
    fn undersized_children_fold_into_their_parent() {
        let p = Partition::build(
            &files(&[
                "src/a.rs",
                "src/b.rs",
                "src/c.rs",
                "src/d.rs",
                "src/one/only.rs",
                "other/x.rs",
                "other/y.rs",
            ]),
            PartitionRule::default(),
        );
        let names: Vec<String> = p.names().cloned().collect();
        assert!(!names.contains(&"src/one".to_string()), "{names:?}");
        assert_eq!(p.subsystem_of("src/one/only.rs"), Some("src"));
    }

    #[test]
    fn every_file_lands_in_exactly_one_subsystem() {
        let list = files(&[
            "src/graph/a.rs",
            "src/index/b.rs",
            "src/index/deep/c.rs",
            "tests/t.rs",
            "README.md",
            "Cargo.toml",
        ]);
        let p = Partition::build(&list, PartitionRule::default());
        let mut seen = 0;
        for name in p.names() {
            seen += p.members(name).len();
        }
        assert_eq!(seen, list.len());
        for f in &list {
            assert!(p.subsystem_of(f).is_some(), "{f} was not placed");
        }
    }

    #[test]
    fn the_partition_is_stable_across_input_order() {
        let forward = files(&["b/one.rs", "b/two.rs", "a/one.rs", "a/two.rs"]);
        let mut reverse = forward.clone();
        reverse.reverse();
        let p1 = Partition::build(&forward, PartitionRule::default());
        let p2 = Partition::build(&reverse, PartitionRule::default());
        assert_eq!(
            p1.names().cloned().collect::<Vec<_>>(),
            p2.names().cloned().collect::<Vec<_>>()
        );
        for name in p1.names() {
            assert_eq!(p1.members(name), p2.members(name));
        }
    }

    #[test]
    fn a_path_that_was_never_indexed_still_places_by_prefix() {
        let p = Partition::build(
            &files(&["src/plan/a.rs", "src/plan/b.rs", "src/graph/c.rs", "src/graph/d.rs"]),
            PartitionRule::default(),
        );
        assert_eq!(p.place_path("src/plan/schema.surql"), Some("src/plan"));
        assert_eq!(p.place_path("docs/notes/offer.md"), None);
    }

    #[test]
    fn root_files_group_under_the_root_name() {
        let p = Partition::build(
            &files(&["README.md", "Cargo.toml", "src/a.rs", "src/b.rs"]),
            PartitionRule::default(),
        );
        assert_eq!(p.subsystem_of("README.md"), Some(ROOT_SUBSYSTEM));
        assert_eq!(p.place_path("LICENSE"), Some(ROOT_SUBSYSTEM));
    }

    #[test]
    fn globs_match_the_way_a_path_glob_should() {
        let re = glob_regex("src/index/**").expect("compiles");
        assert!(re.is_match("src/index/mod.rs"));
        assert!(re.is_match("src/index/extractors/rust.rs"));
        assert!(!re.is_match("src/graph/mod.rs"));

        let star = glob_regex("src/*.rs").expect("compiles");
        assert!(star.is_match("src/main.rs"));
        assert!(!star.is_match("src/graph/mod.rs"));

        let anywhere = glob_regex("**/*.rs").expect("compiles");
        assert!(anywhere.is_match("a.rs"));
        assert!(anywhere.is_match("src/graph/mod.rs"));
    }

    #[test]
    fn a_glob_prefix_survives_an_empty_match() {
        assert_eq!(
            glob_literal_prefix("src/index/**"),
            Some("src/index".to_string())
        );
        assert_eq!(glob_literal_prefix("**/*.rs"), None);
    }

    #[test]
    fn the_rule_description_names_every_number_it_uses() {
        let text = PartitionRule::default().describe();
        assert!(text.contains("25%"), "{text}");
        assert!(text.contains(" 3 path segments"), "{text}");
        assert!(text.contains("fewer than 2 files"), "{text}");
        assert!(!text.contains('\u{2014}'), "em dash in rule description");
    }
}
