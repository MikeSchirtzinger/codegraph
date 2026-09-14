//! The roadmap data model: Rust types for `.codegraph/planes.yaml` and for
//! the graph rows that index it.
//!
//! Two shapes live here and they are deliberately distinct.
//!
//! **The file shape** ([`PlaneFile`], [`Plane`], [`WorkItem`], [`Touch`]) is
//! exactly what a human writes and a pull request diffs. It is the source of
//! truth. It is parsed by [`crate::plan::schema`] and never written back by
//! codegraph.
//!
//! **The graph shape** ([`ResolvedTouch`]) is what `codegraph plan sync`
//! materializes into the store after running every [`Touch`] through the
//! resolver. It is derived, throwaway, and rebuildable from the file at any
//! time, which is the same relationship codegraph already has with source
//! code: the code is the truth, the graph is an index over it.
//!
//! A [`WorkItem`] is a hyperedge. Its `touches` list is its incidence list:
//! one work item touches a *set* of code entities at once, which is the
//! whole reason this is a hypergraph and not a pile of pairs.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

/// The only `version:` value this build of codegraph understands.
///
/// A file carrying any other value is rejected by name rather than parsed
/// on a guess, so a future schema change cannot be silently half-read.
pub const SUPPORTED_VERSION: u32 = 1;

// ============================================================================
// File shape
// ============================================================================

/// A whole `.codegraph/planes.yaml` document.
///
/// `project` is optional in the file: when it is absent the project id comes
/// from the usual resolution order (see [`crate::config`]). When it is
/// present it is advisory, and a caller that already knows the project id
/// can use it as a cross-check rather than as an override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneFile {
    /// Schema version of the document. Must equal [`SUPPORTED_VERSION`].
    pub version: u32,

    /// Project the planes belong to. Advisory; see the type docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,

    /// The planes, in file order. File order is preserved everywhere so a
    /// human can control how the roadmap reads.
    #[serde(default)]
    pub planes: Vec<Plane>,
}

impl PlaneFile {
    /// Every work item in the document, in file order, paired with the id of
    /// the plane that contains it.
    pub fn items(&self) -> impl Iterator<Item = (&Plane, &WorkItem)> {
        self.planes
            .iter()
            .flat_map(|plane| plane.items.iter().map(move |item| (plane, item)))
    }

    /// Look up one work item by its id, with its owning plane.
    pub fn find_item(&self, item_id: &str) -> Option<(&Plane, &WorkItem)> {
        self.items().find(|(_, item)| item.id == item_id)
    }
}

/// One plane: a named band of related work with a status and a horizon.
///
/// A plane groups work items the way a milestone groups issues, except that
/// the grouping carries a horizon (`now` / `next` / `later`) so an agent can
/// tell what is in flight from what is merely recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plane {
    /// Stable identifier, unique within the document. See
    /// [`crate::plan::ID_PATTERN_DESCRIPTION`] for the accepted shape.
    pub id: String,

    /// One-line human title.
    pub title: String,

    /// Lifecycle state of the plane as a whole.
    pub status: Status,

    /// How far out the plane sits.
    pub horizon: Horizon,

    /// Optional prose summary. Free-form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    /// The plane's work items, in file order.
    #[serde(default)]
    pub items: Vec<WorkItem>,
}

/// One work item: the hyperedge itself.
///
/// `touches` is the incidence list. An item with no touches is rejected at
/// validation time, because a hyperedge with no incidence carries no
/// information and cannot be queried, collided, or blast-radiused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Stable identifier, unique across the whole document (not merely
    /// within its plane), because `plan show <id>` takes a bare item id.
    pub id: String,

    /// One-line human title.
    pub title: String,

    /// What sort of change this is.
    pub kind: Kind,

    /// Lifecycle state of this item.
    pub status: Status,

    /// The set of code entities this item touches. Never empty.
    #[serde(default)]
    pub touches: Vec<Touch>,

    /// Ids of other work items that must land first. Every id here must
    /// exist in the same document, and the dependency graph must be acyclic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,

    /// Optional path to the spec that governs this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,

    /// Free-form notes for whoever picks the item up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// How a [`Touch`] names the code it points at.
///
/// This is the `selector` column of the `work_touch` table. It decides which
/// lookup `plan sync` performs, and therefore how the touch can come back
/// RESOLVED, AMBIGUOUS, or UNRESOLVED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Selector {
    /// A repo-relative path to one file, matched against `code_node.file_path`.
    File,
    /// A symbol name or qualified name, matched the way the resolver matches
    /// a name-edge target.
    Symbol,
    /// A path glob, e.g. `src/index/**`, matched against file paths.
    Glob,
}

impl Selector {
    /// The exact string written to `work_touch.selector` and accepted as the
    /// key in a `touches:` entry.
    pub fn as_str(self) -> &'static str {
        match self {
            Selector::File => "file",
            Selector::Symbol => "symbol",
            Selector::Glob => "glob",
        }
    }

    /// Every accepted selector, in the order error messages list them.
    pub const ALL: [Selector; 3] = [Selector::File, Selector::Symbol, Selector::Glob];

    /// Comma-separated list of accepted selectors, for error messages.
    pub fn accepted() -> String {
        join_accepted(Selector::ALL.iter().map(|s| s.as_str()))
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Selector {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "file" => Ok(Selector::File),
            "symbol" => Ok(Selector::Symbol),
            "glob" => Ok(Selector::Glob),
            other => Err(format!(
                "unknown touch selector \"{other}\": accepted selectors are {}",
                Selector::accepted()
            )),
        }
    }
}

/// One incidence of a work item on a code entity, as written in the file.
///
/// In YAML a touch is a single-key map, which is what makes the file read
/// well: `- file: src/cli.rs`, `- symbol: Commands::Index`,
/// `- glob: "src/index/**"`. That single key becomes [`Touch::selector`] and
/// its value becomes [`Touch::raw`], which are the two columns the
/// `work_touch` row carries verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Touch {
    /// How `raw` names its target.
    pub selector: Selector,
    /// The selector's value, kept exactly as written. Never normalized, so
    /// an error message can quote the file back to its author and a
    /// re-sync can rewrite the same row.
    pub raw: String,
}

impl Touch {
    /// Build a touch directly, for tests and for callers assembling a plan
    /// programmatically.
    pub fn new(selector: Selector, raw: impl Into<String>) -> Self {
        Touch {
            selector,
            raw: raw.into(),
        }
    }
}

impl fmt::Display for Touch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.selector, self.raw)
    }
}

impl Serialize for Touch {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(self.selector.as_str(), &self.raw)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Touch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TouchVisitor;

        impl<'de> Visitor<'de> for TouchVisitor {
            type Value = Touch;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    "a touch entry with exactly one key ({})",
                    Selector::accepted()
                )
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Touch, M::Error> {
                let Some((key, raw)) = map.next_entry::<String, String>()? else {
                    return Err(de::Error::custom(format!(
                        "empty touch entry: a touch needs exactly one key, accepted selectors are {}",
                        Selector::accepted()
                    )));
                };
                let selector = Selector::from_str(&key).map_err(de::Error::custom)?;
                if let Some((extra, _)) = map.next_entry::<String, serde::de::IgnoredAny>()? {
                    return Err(de::Error::custom(format!(
                        "touch entry \"{key}\" carries a second key \"{extra}\": one touch names exactly one code entity, so write it as two entries"
                    )));
                }
                if raw.trim().is_empty() {
                    return Err(de::Error::custom(format!(
                        "touch entry \"{key}\" has an empty value: a {key} touch must name something"
                    )));
                }
                Ok(Touch { selector, raw })
            }
        }

        deserializer.deserialize_map(TouchVisitor)
    }
}

/// Lifecycle state of a plane or a work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Status {
    /// Recorded, not started.
    Planned,
    /// In flight. Only active items participate in collision detection.
    Active,
    /// Landed.
    Done,
    /// Deliberately dropped. Kept in the file so the decision stays visible.
    Abandoned,
}

/// How far out a plane sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Horizon {
    /// Being worked now.
    Now,
    /// Queued behind `now`.
    Next,
    /// Parked.
    Later,
}

/// What sort of change a work item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// New capability.
    Feature,
    /// Correcting broken behavior.
    Fix,
    /// Making existing behavior faster or cheaper.
    Perf,
    /// Changing structure without changing behavior.
    Refactor,
    /// Documentation.
    Docs,
    /// An open question to answer before committing to an approach.
    Research,
}

/// Generates `as_str` / `ALL` / `accepted` / `Display` / `FromStr` / serde
/// for a plain lowercase string enum, so every one of them rejects an
/// unknown value with a message that lists what is accepted.
macro_rules! string_enum {
    ($ty:ident, $label:literal, { $( $variant:ident => $text:literal ),+ $(,)? }) => {
        impl $ty {
            /// The exact string this value is written as, in the file and in
            /// the store.
            pub fn as_str(self) -> &'static str {
                match self { $( $ty::$variant => $text ),+ }
            }

            /// Every accepted value, in the order error messages list them.
            pub const ALL: &'static [$ty] = &[ $( $ty::$variant ),+ ];

            /// Comma-separated list of accepted values, for error messages.
            pub fn accepted() -> String {
                join_accepted($ty::ALL.iter().map(|v| v.as_str()))
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $ty {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $text => Ok($ty::$variant), )+
                    other => Err(format!(
                        concat!("unknown ", $label, " value \"{}\": accepted values are {}"),
                        other,
                        $ty::accepted()
                    )),
                }
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                $ty::from_str(&raw).map_err(de::Error::custom)
            }
        }
    };
}

string_enum!(Status, "status", {
    Planned => "planned",
    Active => "active",
    Done => "done",
    Abandoned => "abandoned",
});

string_enum!(Horizon, "horizon", {
    Now => "now",
    Next => "next",
    Later => "later",
});

string_enum!(Kind, "kind", {
    Feature => "feature",
    Fix => "fix",
    Perf => "perf",
    Refactor => "refactor",
    Docs => "docs",
    Research => "research",
});

/// Render an accepted-value list the way every error message in this module
/// renders it: comma separated, in declaration order, no trailing
/// conjunction.
fn join_accepted<'a>(values: impl Iterator<Item = &'a str>) -> String {
    values.collect::<Vec<_>>().join(", ")
}

// ============================================================================
// Graph shape
// ============================================================================

/// The resolver's verdict for one [`Touch`].
///
/// This reuses the code resolver's vocabulary exactly, on purpose. A touch is
/// a name that has to be bound to a node id, which is the same problem
/// `index::resolve` already solves for call and member edges, and the same
/// three answers are the honest ones. Reporting a plan's staleness in the
/// same words as a stale code reference is the point, not a coincidence:
/// both mean "this name no longer binds to anything".
///
/// The string forms (`RESOLVED` / `AMBIGUOUS` / `UNRESOLVED`) are byte for
/// byte what `code_edge.confidence` already carries, so `work_touch` rows and
/// `code_edge` rows can be filtered by the same predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TouchConfidence {
    /// Bound to exactly one node id. `to_id` is set.
    Resolved,
    /// Several candidates survived and none was silently chosen.
    /// `candidates` is set, `to_id` is not.
    Ambiguous,
    /// Nothing matched. Neither `to_id` nor `candidates` is set. On a touch
    /// this means a stale plan: it points at code that is not there.
    Unresolved,
}

string_enum!(TouchConfidence, "confidence", {
    Resolved => "RESOLVED",
    Ambiguous => "AMBIGUOUS",
    Unresolved => "UNRESOLVED",
});

/// One `work_touch` row: a [`Touch`] after `plan sync` has run it through
/// the resolver.
///
/// `selector` and `raw` are carried over from the file verbatim so the row
/// can always be traced back to the line a human wrote. Everything else is
/// derived and is rebuilt from scratch on every sync.
///
/// One [`Touch`] can produce more than one of these. A `glob:` touch that
/// matches four files is four incidences on the hyperedge, not one, so it
/// becomes four rows sharing the same `raw`. That is what keeps the
/// incidence list literally "one row per (item, code entity)" and keeps the
/// collision and blast maths a plain set intersection.
///
/// What `to_id` holds depends on `selector`, which is always present on the
/// row, so a reader always knows which namespace it is in: a `symbol:` touch
/// binds to a `code_node` id, while a `file:` or `glob:` touch binds to the
/// repo relative path it matched. A path has no node of its own in this
/// graph (`code_node` carries `file_path` as a field, there is no file
/// node), and inventing a synthetic one would put an id in the table that
/// nothing else in the store can join against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTouch {
    /// How `raw` named its target. Verbatim from the file.
    pub selector: Selector,

    /// The selector's value. Verbatim from the file.
    pub raw: String,

    /// The bound node id, set only when `confidence` is
    /// [`TouchConfidence::Resolved`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_id: Option<String>,

    /// The resolver's verdict.
    pub confidence: TouchConfidence,

    /// Surviving candidate node ids, set only when `confidence` is
    /// [`TouchConfidence::Ambiguous`], sorted for determinism. Empty
    /// otherwise, never used to smuggle a guess into a RESOLVED row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
}

impl ResolvedTouch {
    /// A touch that bound to exactly one node.
    pub fn resolved(touch: &Touch, to_id: impl Into<String>) -> Self {
        ResolvedTouch {
            selector: touch.selector,
            raw: touch.raw.clone(),
            to_id: Some(to_id.into()),
            confidence: TouchConfidence::Resolved,
            candidates: Vec::new(),
        }
    }

    /// A touch that matched several nodes. Candidates are sorted here so
    /// callers never have to remember to do it.
    pub fn ambiguous(touch: &Touch, mut candidates: Vec<String>) -> Self {
        candidates.sort();
        candidates.dedup();
        ResolvedTouch {
            selector: touch.selector,
            raw: touch.raw.clone(),
            to_id: None,
            confidence: TouchConfidence::Ambiguous,
            candidates,
        }
    }

    /// A touch that matched nothing. On a plan, this is the stale signal.
    pub fn unresolved(touch: &Touch) -> Self {
        ResolvedTouch {
            selector: touch.selector,
            raw: touch.raw.clone(),
            to_id: None,
            confidence: TouchConfidence::Unresolved,
            candidates: Vec::new(),
        }
    }

    /// True when this touch points at code that is not in the graph.
    pub fn is_stale(&self) -> bool {
        self.confidence == TouchConfidence::Unresolved
    }
}

impl fmt::Display for ResolvedTouch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} [{}]", self.selector, self.raw, self.confidence)?;
        match self.confidence {
            TouchConfidence::Resolved => {
                if let Some(id) = &self.to_id {
                    write!(f, " {id}")?;
                }
            }
            TouchConfidence::Ambiguous => {
                write!(f, " candidates: {}", self.candidates.join(", "))?;
            }
            TouchConfidence::Unresolved => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_round_trips_through_yaml() {
        let touch = Touch::new(Selector::Glob, "src/index/**");
        let yaml = serde_yaml::to_string(&touch).expect("serialize touch");
        assert_eq!(yaml.trim(), "glob: src/index/**");
        let back: Touch = serde_yaml::from_str(&yaml).expect("deserialize touch");
        assert_eq!(back, touch);
    }

    #[test]
    fn unknown_selector_lists_accepted_selectors() {
        let err = serde_yaml::from_str::<Touch>("module: foo").expect_err("must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown touch selector \"module\""),
            "message did not name the bad selector: {msg}"
        );
        assert!(
            msg.contains("accepted selectors are file, symbol, glob"),
            "message did not list accepted selectors: {msg}"
        );
    }

    #[test]
    fn unknown_status_lists_accepted_values() {
        let err = Status::from_str("wip").expect_err("must reject");
        assert_eq!(
            err,
            "unknown status value \"wip\": accepted values are planned, active, done, abandoned"
        );
    }

    #[test]
    fn confidence_strings_match_the_code_resolver_vocabulary() {
        assert_eq!(TouchConfidence::Resolved.as_str(), "RESOLVED");
        assert_eq!(TouchConfidence::Ambiguous.as_str(), "AMBIGUOUS");
        assert_eq!(TouchConfidence::Unresolved.as_str(), "UNRESOLVED");
    }

    #[test]
    fn ambiguous_sorts_and_dedups_candidates() {
        let touch = Touch::new(Selector::Symbol, "connect");
        let rt = ResolvedTouch::ambiguous(&touch, vec!["b".into(), "a".into(), "b".into()]);
        assert_eq!(rt.candidates, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(rt.to_id, None);
    }
}
