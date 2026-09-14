//! The roadmap as a first-class graph object.
//!
//! `.codegraph/planes.yaml` is the source of truth; the graph is an index
//! over it. A work item is a hyperedge whose incidence list is its `touches`
//! set, encoded in the store the standard bipartite way:
//!
//! | Table | Row |
//! |---|---|
//! | `work_plane` | one per plane |
//! | `work_item` | one per hyperedge |
//! | `work_touch` | one per (item, code entity) incidence |
//!
//! Module layout:
//!
//! - [`model`] holds the types, both the file shape and the resolved graph
//!   shape.
//! - [`schema`] parses and validates the file.
//! - [`ops`] implements the `codegraph plan ...` subcommands against the
//!   store. It is owned by lane L2.
//! - [`resolve`] binds one `touches:` entry to the graph, in the resolver's
//!   own RESOLVED / AMBIGUOUS / UNRESOLVED vocabulary. Owned by lane L2.
//! - [`store`] owns the three tables, their DDL, and the transactional
//!   replace a sync performs. Owned by lane L2.
//!
//! This module itself holds only what all three need: the id convention and
//! the canonical on-disk locations.

pub mod export;
pub mod model;
pub mod ops;
pub mod resolve;
pub mod schema;
pub mod store;

use std::path::{Path, PathBuf};

pub use model::{
    Horizon, Kind, Plane, PlaneFile, ResolvedTouch, Selector, Status, Touch, TouchConfidence,
    WorkItem, SUPPORTED_VERSION,
};
pub use schema::{load, load_unvalidated, parse_str, validate, Violation, ViolationCode};

/// Directory codegraph keeps its per-repo state in, relative to the repo
/// root. Already the home of `graph.db` and `context.md`.
pub const CODEGRAPH_DIR: &str = ".codegraph";

/// File name of the roadmap, inside [`CODEGRAPH_DIR`].
pub const PLANES_FILE: &str = "planes.yaml";

/// The `work_plane` table, one row per plane.
pub const PLANE_TABLE: &str = "work_plane";

/// The `work_item` table, one row per hyperedge.
pub const ITEM_TABLE: &str = "work_item";

/// The `work_touch` table, one row per incidence.
pub const TOUCH_TABLE: &str = "work_touch";

/// What a plane id or work item id is allowed to look like, phrased for an
/// error message.
pub const ID_PATTERN_DESCRIPTION: &str = "An id must start with a letter or digit and may then contain letters, digits, dots, underscores, and hyphens";

/// The canonical planes file for a repo root.
pub fn default_planes_path(root: &Path) -> PathBuf {
    root.join(CODEGRAPH_DIR).join(PLANES_FILE)
}

/// True when `id` is usable as a plane id or work item id.
///
/// The rule is deliberately narrow. These ids are concatenated into node ids
/// (see [`plane_record_id`]), typed on a command line by a human, and quoted
/// back inside error messages, so whitespace, slashes, and quoting characters
/// are rejected at the door rather than escaped forever after.
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        None => false,
        Some(first) if !first.is_ascii_alphanumeric() => false,
        Some(_) => chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
    }
}

/// Deterministic node id for a plane: `work_plane:<project>__<plane_id>`.
///
/// This follows the same convention as `code_node.node_id`: the id is a
/// plain string *field* on the row, not a SurrealDB record key, so it never
/// has to be quoted and it survives being copied into a JSON payload or an
/// agent's prompt unchanged. Like every other id in codegraph it is derived
/// purely from its identity tuple, so re-running `plan sync` over an
/// unchanged file produces byte-identical ids and upserts cleanly.
///
/// Unlike `index::node_id::generate_node_id` this is readable rather than
/// hashed, because the board's design contract fixes this format and because
/// a plan id is something a human types back at the tool. It carries the
/// same theoretical separator ambiguity that the hashed ids carry with their
/// own `|` separator, and it is bounded the same way: ids are validated by
/// [`is_valid_id`] and are unique per project by [`schema::validate`].
pub fn plane_record_id(project: &str, plane_id: &str) -> String {
    format!("{PLANE_TABLE}:{project}__{plane_id}")
}

/// Deterministic node id for a work item: `work_item:<project>__<item_id>`.
///
/// See [`plane_record_id`] for why this shape.
pub fn item_record_id(project: &str, item_id: &str) -> String {
    format!("{ITEM_TABLE}:{project}__{item_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_ids_match_the_documented_format() {
        assert_eq!(
            plane_record_id("codegraph", "run-anywhere"),
            "work_plane:codegraph__run-anywhere"
        );
        assert_eq!(item_record_id("codegraph", "RA-1"), "work_item:codegraph__RA-1");
    }

    #[test]
    fn record_ids_are_deterministic_and_project_scoped() {
        assert_eq!(item_record_id("a", "X"), item_record_id("a", "X"));
        assert_ne!(item_record_id("a", "X"), item_record_id("b", "X"));
    }

    #[test]
    fn id_rules_accept_real_ids_and_reject_unusable_ones() {
        for good in ["RA-1", "run-anywhere", "P1a.8", "x", "0day_fix"] {
            assert!(is_valid_id(good), "{good} should be accepted");
        }
        for bad in ["", "-leading", ".dot", "has space", "slash/es", "quote\"d", "new\nline"] {
            assert!(!is_valid_id(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn default_planes_path_is_under_the_codegraph_dir() {
        let p = default_planes_path(Path::new("/repo"));
        assert!(p.ends_with(".codegraph/planes.yaml"), "{}", p.display());
    }
}
