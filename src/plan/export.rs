//! Renderers for [`crate::landscape::Landscape`]. **Owned by lane L3.**
//!
//! Every function here is a pure function of the typed landscape. None of
//! them touches a store, which is what makes each format testable on a
//! hand-built model rather than only against a live index, and what keeps
//! all four renderings provably the same set of facts.
//!
//! Four output formats plus the brief:
//!
//! | Function | `--format` | Read by |
//! |---|---|---|
//! | [`markdown`] | `text` | a human, a pull request, an agent's context |
//! | [`json`] | `json` | an agent, a script |
//! | [`mermaid`] | `mermaid` | a markdown renderer |
//! | [`dot`] | `dot` | graphviz |
//! | [`brief`] | `codegraph plan brief` | an agent's context window |
//!
//! The graph formats are capped. A mermaid or DOT graph with one edge per
//! file pair is unreadable and, past a few hundred edges, unrenderable, so
//! the exporters keep the heaviest edges per subsystem and state the cap in
//! the output itself rather than silently dropping information.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::{Context, Result};

use crate::landscape::{
    Landscape, PlaneOverlay, RoadmapSource, Subsystem, SubsystemEdge, ItemOverlay,
};
use crate::plan::model::Status;

/// Outgoing edges kept per subsystem in [`mermaid`] and [`dot`], heaviest
/// first. Stated in the rendering.
pub const MAX_EDGES_PER_NODE: usize = 6;

/// Work items drawn on the graph formats as overlay nodes.
pub const MAX_OVERLAY_ITEMS: usize = 12;

/// Subsystem links drawn per overlay item.
pub const MAX_LINKS_PER_ITEM: usize = 2;

/// Subsystem rows [`brief`] prints before it summarizes the rest.
pub const BRIEF_MAX_SUBSYSTEMS: usize = 12;

/// Paths [`brief`] prints per work item.
pub const BRIEF_MAX_PATHS: usize = 6;

/// Lines in the brief's "work in flight" index.
pub const BRIEF_MAX_IN_FLIGHT: usize = 20;

/// One line telling a reader there is no roadmap and how to get one.
pub const NO_PLANES_NOTICE: &str =
    "No roadmap: .codegraph/planes.yaml is not present. Run codegraph init to scaffold one.";

// ============================================================================
// Markdown
// ============================================================================

/// The full landscape as markdown. This is what `--format text` emits and
/// what `codegraph context` appends under its Landscape heading.
pub fn markdown(l: &Landscape) -> String {
    let mut md = String::with_capacity(8192);

    let _ = writeln!(md, "# Landscape: {}", l.project_id);
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "{} files, {} subsystems, {} nodes, {} inter-subsystem dependencies.",
        l.file_count,
        l.subsystems.len(),
        l.node_count,
        l.edges.len()
    );
    let _ = writeln!(md);
    let _ = writeln!(md, "## How the boxes were drawn");
    let _ = writeln!(md);
    let _ = writeln!(md, "{}", l.rule_description);
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "Each subsystem is a hyperedge over its files. The table below is the incidence \
         structure between those hyperedges."
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## Subsystems");
    let _ = writeln!(md);
    if l.subsystems.is_empty() {
        let _ = writeln!(
            md,
            "Nothing indexed for this project. Run codegraph index first."
        );
        let _ = writeln!(md);
    } else {
        let _ = writeln!(
            md,
            "Ca counts distinct file-to-file dependency channels arriving from other \
             subsystems, Ce counts those leaving, I is Ce/(Ca+Ce). Internal counts channels \
             that stay inside the subsystem."
        );
        let _ = writeln!(md);
        let _ = writeln!(
            md,
            "| Subsystem | Files | Nodes | Internal | Ca | Ce | I | Top hubs | Planned work |"
        );
        let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|");
        for s in &l.subsystems {
            let _ = writeln!(
                md,
                "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} |",
                s.name,
                s.file_count,
                s.node_count,
                s.internal_channels,
                s.afferent,
                s.efferent,
                render_instability(s),
                render_hubs(s),
                render_items(s)
            );
        }
        let _ = writeln!(md);
    }

    let _ = writeln!(md, "## Dependencies between subsystems");
    let _ = writeln!(md);
    if l.edges.is_empty() {
        let _ = writeln!(md, "No resolved dependency crosses a subsystem boundary.");
        let _ = writeln!(md);
    } else {
        let _ = writeln!(
            md,
            "Channels counts distinct (from file, to file) pairs. References counts the \
             resolved name edges behind them, with multiplicity. They come from the same \
             bindings at two granularities and are reported separately rather than summed."
        );
        let _ = writeln!(md);
        let _ = writeln!(md, "| From | To | Channels | References | Via |");
        let _ = writeln!(md, "|---|---|---|---|---|");
        for e in &l.edges {
            let _ = writeln!(
                md,
                "| `{}` | `{}` | {} | {} | {} |",
                e.from,
                e.to,
                e.channels,
                e.references,
                e.via.join(", ")
            );
        }
        let _ = writeln!(md);
    }

    let _ = writeln!(md, "## Cycles between subsystems");
    let _ = writeln!(md);
    if l.cycles.is_empty() {
        let _ = writeln!(
            md,
            "None. No two subsystems depend on each other in both directions."
        );
        let _ = writeln!(md);
    } else {
        for c in &l.cycles {
            let _ = writeln!(
                md,
                "- `{}` and `{}` depend on each other: {} channels one way, {} the other.",
                c.a, c.b, c.a_to_b, c.b_to_a
            );
            for f in &c.file_cycles {
                let _ = writeln!(
                    md,
                    "  - file cycle `{}` and `{}` via {}",
                    f.file_a,
                    f.file_b,
                    f.via.join(", ")
                );
            }
        }
        let _ = writeln!(md);
    }

    let _ = writeln!(md, "## Roadmap overlay");
    let _ = writeln!(md);
    match &l.roadmap.source {
        RoadmapSource::Missing => {
            let _ = writeln!(md, "{NO_PLANES_NOTICE}");
            let _ = writeln!(md);
        }
        RoadmapSource::Invalid { reason } => {
            let _ = writeln!(
                md,
                "The roadmap at {} did not load, so the overlay is empty. The landscape above \
                 is unaffected.",
                l.roadmap.path
            );
            let _ = writeln!(md);
            let _ = writeln!(md, "```");
            let _ = writeln!(md, "{reason}");
            let _ = writeln!(md, "```");
            let _ = writeln!(md);
        }
        RoadmapSource::Loaded => {
            let items: usize = l.roadmap.planes.iter().map(|p| p.items.len()).sum();
            let touches: usize = l
                .roadmap
                .planes
                .iter()
                .flat_map(|p| p.items.iter())
                .map(|i| i.touches.len())
                .sum();
            let _ = writeln!(
                md,
                "Source: {}. {} planes, {} items, {} placed touches, {} unplaced.",
                l.roadmap.path,
                l.roadmap.planes.len(),
                items,
                touches,
                l.roadmap.unplaced.len()
            );
            let _ = writeln!(md);
            let _ = writeln!(
                md,
                "A work item is a hyperedge over the code entities it touches. Placing each \
                 touch into a subsystem overlays the roadmap on the map above."
            );
            let _ = writeln!(md);

            for plane in &l.roadmap.planes {
                let _ = writeln!(
                    md,
                    "### {} ({}, {})",
                    plane.title, plane.status, plane.horizon
                );
                let _ = writeln!(md);
                let _ = writeln!(md, "Plane id: `{}`", plane.id);
                let _ = writeln!(md);
                if let Some(summary) = &plane.summary {
                    let _ = writeln!(md, "{summary}");
                    let _ = writeln!(md);
                }
                if plane.items.is_empty() {
                    let _ = writeln!(md, "No items.");
                    let _ = writeln!(md);
                    continue;
                }
                for item in &plane.items {
                    let _ = writeln!(
                        md,
                        "- **{}** [{}, {}] {}",
                        item.id, item.status, item.kind, item.title
                    );
                    if !item.subsystems.is_empty() {
                        let _ = writeln!(
                            md,
                            "  - subsystems: {}",
                            item.subsystems
                                .iter()
                                .map(|s| format!("`{s}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    for t in &item.touches {
                        let _ = writeln!(
                            md,
                            "  - {}: `{}` to {}",
                            t.selector,
                            t.raw,
                            t.subsystems
                                .iter()
                                .map(|s| format!("`{s}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !item.depends_on.is_empty() {
                        let _ = writeln!(md, "  - depends on: {}", item.depends_on.join(", "));
                    }
                    if let Some(spec) = &item.spec {
                        let _ = writeln!(md, "  - spec: {spec}");
                    }
                }
                let _ = writeln!(md);
            }

            if !l.roadmap.unplaced.is_empty() {
                let stale = l.roadmap.unplaced.iter().filter(|u| u.cause.is_stale()).count();
                let _ = writeln!(md, "### Unplaced touches");
                let _ = writeln!(md);
                let _ = writeln!(
                    md,
                    "{} touches name something no subsystem covers, {} of them stale. They are \
                     reported rather than dropped, because the two causes call for different \
                     answers. A path outside the indexed tree is a sound plan this map cannot \
                     draw: codegraph indexes source files, so documentation, specification, and \
                     configuration paths land here. A path that is not on disk at all, or a \
                     symbol no definition answers to, is a stale plan, which is the same signal \
                     an UNRESOLVED code reference carries.",
                    l.roadmap.unplaced.len(),
                    stale
                );
                let _ = writeln!(md);
                let _ = writeln!(md, "| Item | Touch | Cause |");
                let _ = writeln!(md, "|---|---|---|");
                for u in &l.roadmap.unplaced {
                    let _ = writeln!(
                        md,
                        "| `{}` | {}: `{}` | {} |",
                        u.item, u.selector, u.raw, u.cause
                    );
                }
                let _ = writeln!(md);
            }
        }
    }

    md
}

fn render_instability(s: &Subsystem) -> String {
    match s.instability {
        Some(i) => format!("{i:.2}"),
        None => "n/a".to_string(),
    }
}

fn render_hubs(s: &Subsystem) -> String {
    if s.hubs.is_empty() {
        return "none".to_string();
    }
    s.hubs
        .iter()
        .map(|h| format!("`{}` ({}, {})", h.name, h.node_type, h.degree))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_items(s: &Subsystem) -> String {
    if s.items.is_empty() {
        return "none".to_string();
    }
    s.items.join(", ")
}

// ============================================================================
// JSON
// ============================================================================

/// The landscape as pretty-printed JSON. Field names are the ones declared
/// on [`Landscape`] and are part of the agent-facing contract.
pub fn json(l: &Landscape) -> Result<String> {
    let mut text =
        serde_json::to_string_pretty(l).context("serializing the landscape to JSON failed")?;
    text.push('\n');
    Ok(text)
}

// ============================================================================
// Mermaid
// ============================================================================

/// The landscape as a mermaid flowchart.
///
/// Subsystems are nodes, weighted by channel count. Active planes become
/// subgraphs of work item nodes with dashed links to the subsystems they
/// touch. Every cap applied is printed as a comment line so the reader knows
/// the picture is a summary and knows by how much.
pub fn mermaid(l: &Landscape) -> String {
    let mut out = String::with_capacity(4096);
    let ids = node_ids(l);

    let _ = writeln!(out, "%% codegraph landscape: {}", l.project_id);
    let _ = writeln!(out, "%% {}", l.rule_description);
    let _ = writeln!(
        out,
        "%% Edge labels are distinct file-to-file dependency channels."
    );
    let kept = capped_edges(l);
    let _ = writeln!(
        out,
        "%% Edges are capped at {MAX_EDGES_PER_NODE} per subsystem, heaviest first: showing {} of {}.",
        kept.len(),
        l.edges.len()
    );
    let overlay = overlay_items(l);
    let _ = writeln!(
        out,
        "%% Overlay is capped at {MAX_OVERLAY_ITEMS} work items and {MAX_LINKS_PER_ITEM} links each: showing {}.",
        overlay.len()
    );
    let _ = writeln!(out, "flowchart LR");

    for s in &l.subsystems {
        let _ = writeln!(
            out,
            "  {}[\"{}<br/>{} files, {} nodes\"]",
            ids[s.name.as_str()],
            safe_label(&s.name),
            s.file_count,
            s.node_count
        );
    }

    for e in &kept {
        let (Some(from), Some(to)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str())) else {
            continue;
        };
        let _ = writeln!(out, "  {from} -->|{}| {to}", e.channels);
    }

    if !overlay.is_empty() {
        for (plane_index, plane) in active_planes(l).iter().enumerate() {
            let drawn: Vec<&ItemOverlay> = overlay
                .iter()
                .copied()
                .filter(|i| i.plane == plane.id)
                .collect();
            if drawn.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "  subgraph plane{plane_index}[\"{} ({})\"]",
                safe_label(&plane.title),
                plane.status
            );
            for item in &drawn {
                let _ = writeln!(
                    out,
                    "    {}[\"{} {}\"]",
                    item_id(&item.id),
                    safe_label(&item.id),
                    item.status
                );
            }
            let _ = writeln!(out, "  end");
        }
        for item in &overlay {
            for sub in item.subsystems.iter().take(MAX_LINKS_PER_ITEM) {
                let Some(target) = ids.get(sub.as_str()) else {
                    continue;
                };
                let _ = writeln!(out, "  {} -.-> {target}", item_id(&item.id));
            }
        }
    }

    out
}

// ============================================================================
// DOT
// ============================================================================

/// The landscape as graphviz DOT, with the same content and the same caps as
/// [`mermaid`].
pub fn dot(l: &Landscape) -> String {
    let mut out = String::with_capacity(4096);
    let ids = node_ids(l);
    let kept = capped_edges(l);
    let overlay = overlay_items(l);

    let _ = writeln!(out, "// codegraph landscape: {}", l.project_id);
    let _ = writeln!(out, "// {}", l.rule_description);
    let _ = writeln!(
        out,
        "// Edges are capped at {MAX_EDGES_PER_NODE} per subsystem, heaviest first: showing {} of {}.",
        kept.len(),
        l.edges.len()
    );
    let _ = writeln!(out, "digraph landscape {{");
    let _ = writeln!(out, "  rankdir=LR;");
    let _ = writeln!(out, "  node [shape=box, fontname=\"Helvetica\"];");
    let _ = writeln!(out, "  edge [fontname=\"Helvetica\", fontsize=10];");

    for s in &l.subsystems {
        let _ = writeln!(
            out,
            "  {} [label=\"{}\\n{} files, {} nodes\"];",
            ids[s.name.as_str()],
            dot_escape(&s.name),
            s.file_count,
            s.node_count
        );
    }

    for e in &kept {
        let (Some(from), Some(to)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str())) else {
            continue;
        };
        let _ = writeln!(
            out,
            "  {from} -> {to} [label=\"{}\", penwidth={:.1}];",
            e.channels,
            pen_width(e.channels)
        );
    }

    for (plane_index, plane) in active_planes(l).iter().enumerate() {
        let drawn: Vec<&ItemOverlay> = overlay
            .iter()
            .copied()
            .filter(|i| i.plane == plane.id)
            .collect();
        if drawn.is_empty() {
            continue;
        }
        let _ = writeln!(out, "  subgraph cluster_plane{plane_index} {{");
        let _ = writeln!(
            out,
            "    label=\"{} ({})\";",
            dot_escape(&plane.title),
            plane.status
        );
        let _ = writeln!(out, "    style=dashed;");
        for item in &drawn {
            let _ = writeln!(
                out,
                "    {} [label=\"{} {}\", shape=note];",
                item_id(&item.id),
                dot_escape(&item.id),
                item.status
            );
        }
        let _ = writeln!(out, "  }}");
    }

    for item in &overlay {
        for sub in item.subsystems.iter().take(MAX_LINKS_PER_ITEM) {
            let Some(target) = ids.get(sub.as_str()) else {
                continue;
            };
            let _ = writeln!(
                out,
                "  {} -> {target} [style=dashed, arrowhead=open];",
                item_id(&item.id)
            );
        }
    }

    let _ = writeln!(out, "}}");
    out
}

// ============================================================================
// Brief
// ============================================================================

/// The compact, agent-pasteable markdown brief.
///
/// Built to be read inside a context window rather than looked at: the
/// subsystem map in a few lines, then every active plane's live items with
/// their touches as literal paths, then an index keyed by path so an agent
/// grepping for a file it is about to edit hits the work item covering it.
/// Headings stop at level three and every list is capped.
pub fn brief(l: &Landscape) -> String {
    let mut md = String::with_capacity(4096);

    let _ = writeln!(
        md,
        "### codegraph landscape: {} ({} files, {} subsystems, {} nodes)",
        l.project_id,
        l.file_count,
        l.subsystems.len(),
        l.node_count
    );
    let _ = writeln!(md, "{}", l.rule_description);
    let _ = writeln!(md);

    if l.subsystems.is_empty() {
        let _ = writeln!(md, "Nothing indexed. Run codegraph index first.");
        let _ = writeln!(md);
    } else {
        // Rank by how connected a subsystem is, not by how big it is. On
        // this repo the largest directories by file count are test fixture
        // trees with no edges at all, and putting those at the top of an
        // agent's context buries the core underneath them.
        let mut ranked: Vec<&Subsystem> = l.subsystems.iter().collect();
        ranked.sort_by(|a, b| {
            (b.afferent + b.efferent)
                .cmp(&(a.afferent + a.efferent))
                .then_with(|| b.file_count.cmp(&a.file_count))
                .then_with(|| a.name.cmp(&b.name))
        });
        for s in ranked.iter().take(BRIEF_MAX_SUBSYSTEMS) {
            let hub = s
                .hubs
                .first()
                .map(|h| format!(", top hub {}", h.name))
                .unwrap_or_default();
            let _ = writeln!(
                md,
                "- {}: {} files, {} nodes, Ca {} Ce {} I {}{}",
                s.name,
                s.file_count,
                s.node_count,
                s.afferent,
                s.efferent,
                render_instability(s),
                hub
            );
        }
        if ranked.len() > BRIEF_MAX_SUBSYSTEMS {
            let _ = writeln!(
                md,
                "- and {} smaller subsystems, see codegraph landscape",
                ranked.len() - BRIEF_MAX_SUBSYSTEMS
            );
        }
        if !l.cycles.is_empty() {
            let names: Vec<String> = l
                .cycles
                .iter()
                .map(|c| format!("{} and {}", c.a, c.b))
                .collect();
            let _ = writeln!(md, "- mutual dependencies: {}", names.join("; "));
        }
        let _ = writeln!(md);
    }

    match &l.roadmap.source {
        RoadmapSource::Missing => {
            let _ = writeln!(md, "### Roadmap");
            let _ = writeln!(md, "{NO_PLANES_NOTICE}");
            let _ = writeln!(md);
            return md;
        }
        RoadmapSource::Invalid { reason } => {
            let _ = writeln!(md, "### Roadmap");
            let _ = writeln!(
                md,
                "The roadmap at {} did not load: {}",
                l.roadmap.path,
                first_line(reason)
            );
            let _ = writeln!(md);
            return md;
        }
        RoadmapSource::Loaded => {}
    }

    let planes = active_planes(l);
    let _ = writeln!(md, "### Roadmap ({})", l.roadmap.path);
    if planes.is_empty() {
        let _ = writeln!(md, "No active planes.");
        let _ = writeln!(md);
    } else {
        let mut written = 0usize;
        for plane in &planes {
            let live: Vec<&ItemOverlay> = plane.items.iter().filter(|i| is_live(i)).collect();
            if live.is_empty() {
                continue;
            }
            written += 1;
            // Level three is the deepest heading the brief uses, so a plane
            // is a labelled line rather than a fourth-level heading. An
            // agent's context window does not need the outline, and a
            // document that embeds this one has room left underneath it.
            let _ = writeln!(
                md,
                "plane {}: {} ({}, {})",
                plane.id, plane.title, plane.status, plane.horizon
            );
            for item in live {
                let _ = writeln!(md, "- {} [{}] {}", item.id, item.status, item.title);
                let mut paths: Vec<&str> = item
                    .touches
                    .iter()
                    .flat_map(|t| t.paths.iter().map(String::as_str))
                    .collect();
                paths.sort_unstable();
                paths.dedup();
                if paths.is_empty() {
                    // Every touch this item declared was unplaceable. Say
                    // so: an item with no `touches:` line at all reads as
                    // an item with no touches, which the schema forbids.
                    let _ = writeln!(
                        md,
                        "  touches: none on the map, see codegraph plan show {}",
                        item.id
                    );
                } else {
                    let shown = paths
                        .iter()
                        .take(BRIEF_MAX_PATHS)
                        .copied()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let more = if paths.len() > BRIEF_MAX_PATHS {
                        format!(" and {} more", paths.len() - BRIEF_MAX_PATHS)
                    } else {
                        String::new()
                    };
                    let _ = writeln!(md, "  touches: {shown}{more}");
                }
            }
        }
        if written == 0 {
            // Every active plane holds only done or abandoned items. Say
            // that, rather than leaving a heading with nothing under it and
            // letting a reader guess whether the section failed.
            let _ = writeln!(
                md,
                "Every item in every active plane is done or abandoned."
            );
        }
        let _ = writeln!(md);
    }

    let in_flight = in_flight_index(l);
    if !in_flight.is_empty() {
        let _ = writeln!(md, "### If you touch this, work is in flight");
        for (path, items) in in_flight.iter().take(BRIEF_MAX_IN_FLIGHT) {
            let _ = writeln!(md, "- {path}: {}", items.join(", "));
        }
        if in_flight.len() > BRIEF_MAX_IN_FLIGHT {
            let _ = writeln!(
                md,
                "- and {} more paths, see codegraph plan touching <path>",
                in_flight.len() - BRIEF_MAX_IN_FLIGHT
            );
        }
        let _ = writeln!(md);
    }

    // The brief carries only the stale half. A documentation path outside
    // the indexed tree is not something an agent editing code needs told,
    // and 20 lines of it would crowd out the part that matters.
    let stale: Vec<&crate::landscape::UnplacedTouch> = l
        .roadmap
        .unplaced
        .iter()
        .filter(|u| u.cause.is_stale())
        .collect();
    if !stale.is_empty() {
        let _ = writeln!(md, "### Stale touches: the plan points at code that is gone");
        for u in stale.iter().take(BRIEF_MAX_IN_FLIGHT) {
            let _ = writeln!(md, "- {} {}: {} ({})", u.item, u.selector, u.raw, u.cause);
        }
        if stale.len() > BRIEF_MAX_IN_FLIGHT {
            let _ = writeln!(md, "- and {} more", stale.len() - BRIEF_MAX_IN_FLIGHT);
        }
        let _ = writeln!(md);
    }

    md
}

/// Paths covered by an item that is actually being worked, sorted by path.
///
/// "In flight" means the work item's own status is `active`. A `planned`
/// item in an active plane is listed in the roadmap section above but is not
/// a collision risk yet, and saying otherwise would make the index noisy
/// enough to ignore.
fn in_flight_index(l: &Landscape) -> Vec<(String, Vec<String>)> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, item) in l.roadmap.items() {
        if item.status != Status::Active {
            continue;
        }
        for touch in &item.touches {
            for path in &touch.paths {
                let entry = index.entry(path.clone()).or_default();
                if !entry.contains(&item.id) {
                    entry.push(item.id.clone());
                }
            }
        }
    }
    index.into_iter().collect()
}

/// Items an agent should know about: not finished, not dropped.
fn is_live(item: &ItemOverlay) -> bool {
    matches!(item.status, Status::Planned | Status::Active)
}

/// Planes that are being worked.
fn active_planes(l: &Landscape) -> Vec<&PlaneOverlay> {
    l.roadmap
        .planes
        .iter()
        .filter(|p| p.status == Status::Active)
        .collect()
}

/// Work items the graph formats draw, heaviest planes first, capped.
fn overlay_items(l: &Landscape) -> Vec<&ItemOverlay> {
    let mut picked: Vec<&ItemOverlay> = Vec::new();
    for plane in active_planes(l) {
        for item in &plane.items {
            if !is_live(item) || item.subsystems.is_empty() {
                continue;
            }
            picked.push(item);
            if picked.len() == MAX_OVERLAY_ITEMS {
                return picked;
            }
        }
    }
    picked
}

/// The edges the graph formats draw: the heaviest [`MAX_EDGES_PER_NODE`]
/// leaving each subsystem, deduplicated and back in (from, to) order.
fn capped_edges(l: &Landscape) -> Vec<&SubsystemEdge> {
    let mut by_source: BTreeMap<&str, Vec<&SubsystemEdge>> = BTreeMap::new();
    for e in &l.edges {
        by_source.entry(e.from.as_str()).or_default().push(e);
    }
    let mut kept: Vec<&SubsystemEdge> = Vec::new();
    for (_, mut group) in by_source {
        group.sort_by(|a, b| b.channels.cmp(&a.channels).then_with(|| a.to.cmp(&b.to)));
        kept.extend(group.into_iter().take(MAX_EDGES_PER_NODE));
    }
    kept.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    kept
}

/// Stable, collision-free graph ids. Subsystem names are paths, which are
/// not valid identifiers in either mermaid or DOT, so they are numbered in
/// the landscape's own subsystem order and carried as labels.
fn node_ids(l: &Landscape) -> BTreeMap<&str, String> {
    l.subsystems
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), format!("s{i}")))
        .collect()
}

/// Graph id for a work item. Item ids are validated by
/// [`crate::plan::is_valid_id`], so only dots and hyphens need replacing.
fn item_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + 5);
    out.push_str("item_");
    for c in id.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// A label safe to put inside a mermaid quoted string.
fn safe_label(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| match c {
            '"' | '\'' | '`' | '|' | '<' | '>' | '{' | '}' | '[' | ']' | '(' | ')' | '\\' => ' ',
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    truncate(cleaned.trim(), 60)
}

/// A label safe to put inside a DOT quoted string.
fn dot_escape(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' | '\r' | '\t' => vec![' '],
            other => vec![other],
        })
        .collect();
    truncate(&escaped, 60)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{head}...")
}

/// Line thickness for a DOT edge, bounded so one heavy edge cannot make the
/// rest invisible.
fn pen_width(channels: usize) -> f64 {
    1.0 + (channels as f64).sqrt().min(4.0)
}

/// First line of a multi-line error, for a one-line notice.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// [`demote_headings_by`] with one level.
pub fn demote_headings(text: &str) -> String {
    demote_headings_by(text, 1)
}

/// Push every markdown heading down `levels`, so a document embedded in
/// another one nests under its host's sections instead of competing with
/// them. Lines inside fenced code blocks are left alone.
///
/// `codegraph context` embeds [`markdown`], whose top heading is level one,
/// two levels down, and [`brief`], whose top heading is already level three,
/// not at all. Both then sit directly under the level-two section that
/// introduces them.
pub fn demote_headings_by(text: &str, levels: usize) -> String {
    if levels == 0 {
        return text.to_string();
    }
    let prefix = "#".repeat(levels);
    let mut out = String::with_capacity(text.len() + levels * 16);
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with('#') {
            out.push_str(&prefix);
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::landscape::{
        Hub, ItemOverlay, PlaneOverlay, Roadmap, Subsystem, SubsystemEdge, TouchPlacement,
    };
    use crate::plan::model::{Horizon, Kind, Selector};

    fn subsystem(name: &str, files: usize) -> Subsystem {
        Subsystem {
            name: name.to_string(),
            file_count: files,
            node_count: files * 10,
            internal_channels: 1,
            afferent: 2,
            efferent: 3,
            instability: Some(0.6),
            hubs: vec![Hub {
                name: format!("{name}_hub"),
                node_type: "function".to_string(),
                file_path: format!("{name}/a.rs"),
                degree: 9,
            }],
            most_coupled_file: Some(format!("{name}/a.rs")),
            items: Vec::new(),
        }
    }

    fn edge(from: &str, to: &str, channels: usize) -> SubsystemEdge {
        SubsystemEdge {
            from: from.to_string(),
            to: to.to_string(),
            channels,
            references: channels * 2,
            via: vec!["calls".to_string(), "file_ref".to_string()],
        }
    }

    fn item(id: &str, plane: &str, status: Status, paths: &[&str]) -> ItemOverlay {
        let placement = TouchPlacement {
            selector: Selector::File,
            raw: paths.first().copied().unwrap_or("src/a.rs").to_string(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            subsystems: vec!["src".to_string()],
        };
        ItemOverlay {
            id: id.to_string(),
            plane: plane.to_string(),
            title: format!("title for {id}"),
            kind: Kind::Feature,
            status,
            depends_on: Vec::new(),
            spec: None,
            touches: vec![placement],
            subsystems: vec!["src".to_string()],
        }
    }

    fn landscape() -> Landscape {
        Landscape {
            project_id: "fixture".to_string(),
            rule: crate::landscape::PartitionRule::default(),
            rule_description: crate::landscape::PartitionRule::default().describe(),
            file_count: 6,
            node_count: 60,
            subsystems: vec![subsystem("src", 4), subsystem("tests", 2)],
            edges: vec![edge("src", "tests", 3), edge("tests", "src", 1)],
            cycles: Vec::new(),
            roadmap: Roadmap {
                source: RoadmapSource::Loaded,
                path: ".codegraph/planes.yaml".to_string(),
                planes: vec![PlaneOverlay {
                    id: "p1".to_string(),
                    title: "First plane".to_string(),
                    status: Status::Active,
                    horizon: Horizon::Now,
                    summary: None,
                    items: vec![
                        item("A-1", "p1", Status::Active, &["src/a.rs"]),
                        item("A-2", "p1", Status::Done, &["src/b.rs"]),
                    ],
                }],
                unplaced: Vec::new(),
            },
        }
    }

    #[test]
    fn mermaid_declares_every_subsystem_and_no_duplicate_edge() {
        let text = mermaid(&landscape());
        assert!(text.contains("flowchart LR"), "{text}");
        assert!(text.contains("s0[\"src<br/>4 files, 40 nodes\"]"), "{text}");
        assert!(text.contains("s1[\"tests<br/>2 files, 20 nodes\"]"), "{text}");
        assert!(text.contains("s0 -->|3| s1"), "{text}");
        assert!(text.contains("s1 -->|1| s0"), "{text}");

        let edges: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("-->"))
            .collect();
        let mut unique = edges.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(edges.len(), unique.len(), "duplicate edge lines: {edges:?}");
    }

    #[test]
    fn mermaid_states_its_caps() {
        let text = mermaid(&landscape());
        assert!(text.contains("Edges are capped at 6 per subsystem"), "{text}");
        assert!(text.contains("Overlay is capped at 12 work items"), "{text}");
    }

    #[test]
    fn mermaid_draws_only_live_items_of_active_planes() {
        let text = mermaid(&landscape());
        // `item_id` replaces every non-alphanumeric character, so `A-1`
        // becomes `item_A_1`.
        assert!(text.contains("item_A_1[\"A-1 active\"]"), "{text}");
        assert!(
            !text.contains("item_A_2"),
            "a done item must not be drawn: {text}"
        );
    }

    #[test]
    fn the_edge_cap_keeps_the_heaviest_edges() {
        let mut l = landscape();
        l.subsystems = (0..9).map(|i| subsystem(&format!("s{i}"), 1)).collect();
        l.edges = (0..9).map(|i| edge("s0", &format!("s{i}"), i)).collect();
        let kept = capped_edges(&l);
        assert_eq!(kept.len(), MAX_EDGES_PER_NODE);
        let mut weights: Vec<usize> = kept.iter().map(|e| e.channels).collect();
        weights.sort_unstable();
        assert_eq!(weights, vec![3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn dot_is_a_closed_digraph() {
        let text = dot(&landscape());
        assert!(text.starts_with("// codegraph landscape: fixture"), "{text}");
        assert!(text.contains("digraph landscape {"), "{text}");
        assert!(text.trim_end().ends_with('}'), "{text}");
        assert!(text.contains("s0 -> s1 [label=\"3\""), "{text}");
    }

    #[test]
    fn json_round_trips_through_the_typed_model() {
        let l = landscape();
        let text = json(&l).expect("serializes");
        let back: Landscape = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(back, l);
    }

    #[test]
    fn the_brief_lists_live_items_and_omits_finished_ones() {
        let text = brief(&landscape());
        assert!(text.contains("A-1 [active]"), "{text}");
        assert!(!text.contains("A-2"), "done item leaked into the brief: {text}");
        assert!(text.contains("If you touch this, work is in flight"), "{text}");
        assert!(text.contains("src/a.rs: A-1"), "{text}");
    }

    #[test]
    fn the_brief_says_so_when_an_active_plane_has_nothing_live() {
        let mut l = landscape();
        for item in &mut l.roadmap.planes[0].items {
            item.status = Status::Done;
        }
        let text = brief(&l);
        assert!(
            text.contains("Every item in every active plane is done or abandoned."),
            "{text}"
        );
        assert!(!text.contains("A-1"), "{text}");
    }

    #[test]
    fn the_brief_says_so_when_there_is_no_roadmap() {
        let mut l = landscape();
        l.roadmap.source = RoadmapSource::Missing;
        l.roadmap.planes.clear();
        let text = brief(&l);
        assert!(text.contains(NO_PLANES_NOTICE), "{text}");
    }

    #[test]
    fn the_brief_stops_at_level_three_headings_and_uses_no_em_dash() {
        let text = brief(&landscape());
        for line in text.lines() {
            assert!(
                !line.starts_with("#### "),
                "the brief must stop at level three headings: {line}"
            );
            assert!(!line.contains('\u{2014}'), "em dash in the brief: {line}");
        }
    }

    #[test]
    fn markdown_reports_both_edge_numbers_separately() {
        let text = markdown(&landscape());
        assert!(text.contains("| Channels | References | Via |"), "{text}");
        assert!(text.contains("| `src` | `tests` | 3 | 6 |"), "{text}");
    }

    #[test]
    fn no_rendering_contains_an_em_dash() {
        let l = landscape();
        for text in [markdown(&l), mermaid(&l), dot(&l), brief(&l)] {
            assert!(!text.contains('\u{2014}'), "em dash in a rendering");
            assert!(!text.contains('\u{2013}'), "en dash in a rendering");
        }
    }

    #[test]
    fn headings_are_demoted_by_the_level_count_asked_for() {
        assert_eq!(demote_headings_by("# T\n## S\n", 2), "### T\n#### S\n");
        assert_eq!(demote_headings_by("# T\n", 0), "# T\n");
    }

    #[test]
    fn headings_are_demoted_one_level() {
        let text = "# Title\n\nbody\n\n## Section\n### Deeper\n";
        assert_eq!(
            demote_headings(text),
            "## Title\n\nbody\n\n### Section\n#### Deeper\n"
        );
    }

    #[test]
    fn a_hash_inside_a_fence_is_left_alone() {
        let text = "```\n# not a heading\n```\n# a heading\n";
        assert_eq!(
            demote_headings(text),
            "```\n# not a heading\n```\n## a heading\n"
        );
    }

    #[test]
    fn labels_survive_characters_that_would_break_the_graph_syntax() {
        assert_eq!(safe_label("a \"quoted\" [thing]"), "a  quoted   thing");
        assert_eq!(dot_escape("a \"quoted\" thing"), "a \\\"quoted\\\" thing");
    }
}
