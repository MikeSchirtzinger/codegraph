//! Load and validate `.codegraph/planes.yaml`.
//!
//! Parsing is [`serde_yaml`] over the types in [`crate::plan::model`].
//! Validation is everything serde cannot express: uniqueness, referential
//! integrity across `depends_on`, acyclicity, and the rule that a hyperedge
//! must have at least one incidence.
//!
//! Both halves report in the same voice, because both are read by a human
//! fixing a file and by an agent deciding whether it may proceed: say what is
//! wrong, say where it is, and say what would have been accepted.
//!
//! [`validate`] returns every problem it finds rather than the first, so one
//! run of `codegraph plan lint` shows the whole list. [`load`] parses,
//! validates, and refuses to hand back a document with any violation, so no
//! caller downstream has to remember to check.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::plan::model::{PlaneFile, SUPPORTED_VERSION};
use crate::plan::{is_valid_id, ID_PATTERN_DESCRIPTION};

/// Which rule a [`Violation`] broke. Stable, machine-readable, and printed
/// in brackets ahead of every message so a caller can filter on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViolationCode {
    /// `version:` is not [`SUPPORTED_VERSION`].
    UnsupportedVersion,
    /// A plane id or work item id is not a usable identifier.
    InvalidId,
    /// Two planes share an id.
    DuplicatePlaneId,
    /// Two work items share an id, anywhere in the document.
    DuplicateItemId,
    /// A work item has an empty `touches` list.
    EmptyTouches,
    /// A `depends_on` entry names a work item that does not exist.
    DanglingDependency,
    /// A work item lists itself in `depends_on`.
    SelfDependency,
    /// A cycle exists in the `depends_on` graph.
    DependencyCycle,
}

impl ViolationCode {
    /// The bracketed tag printed with the message.
    pub fn as_str(self) -> &'static str {
        match self {
            ViolationCode::UnsupportedVersion => "unsupported-version",
            ViolationCode::InvalidId => "invalid-id",
            ViolationCode::DuplicatePlaneId => "duplicate-plane-id",
            ViolationCode::DuplicateItemId => "duplicate-item-id",
            ViolationCode::EmptyTouches => "empty-touches",
            ViolationCode::DanglingDependency => "dangling-dependency",
            ViolationCode::SelfDependency => "self-dependency",
            ViolationCode::DependencyCycle => "dependency-cycle",
        }
    }
}

impl fmt::Display for ViolationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One validation failure, with the rule it broke and a message that names
/// the offending value and where it sits in the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// The rule that was broken.
    pub code: ViolationCode,
    /// Human and agent readable description. Always names the value at
    /// fault and its location in the file.
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl Violation {
    fn new(code: ViolationCode, message: impl Into<String>) -> Self {
        Violation {
            code,
            message: message.into(),
        }
    }
}

/// Structural location of a plane in the document, for error messages.
fn plane_at(plane_index: usize, plane_id: &str) -> String {
    format!("planes[{plane_index}] (\"{plane_id}\")")
}

/// Structural location of a work item in the document, for error messages.
fn item_at(plane_index: usize, item_index: usize, plane_id: &str) -> String {
    format!("planes[{plane_index}].items[{item_index}] (plane \"{plane_id}\")")
}

/// Parse YAML text into a [`PlaneFile`] without validating it.
///
/// The `version` key is checked before anything else so that a document from
/// a future schema fails with a message about the version rather than with a
/// pile of confusing field errors from a shape this build does not know.
pub fn parse_str(yaml: &str) -> Result<PlaneFile> {
    let raw: serde_yaml::Value =
        serde_yaml::from_str(yaml).context("planes file is not valid YAML")?;

    match raw.get("version") {
        None => {
            return Err(anyhow!(
                "planes file has no \"version\" key. This build of codegraph reads version {SUPPORTED_VERSION}, so the file must start with \"version: {SUPPORTED_VERSION}\""
            ));
        }
        Some(value) => {
            let found = value
                .as_u64()
                .ok_or_else(|| {
                    anyhow!(
                        "planes file \"version\" must be a whole number, found {}. This build of codegraph reads version {SUPPORTED_VERSION}",
                        render_scalar(value)
                    )
                })?;
            if found != u64::from(SUPPORTED_VERSION) {
                return Err(anyhow!(
                    "planes file declares version {found}, which this build of codegraph does not read. The supported version is {SUPPORTED_VERSION}"
                ));
            }
        }
    }

    serde_yaml::from_value(raw).context("planes file does not match the version 1 schema")
}

/// Render a YAML scalar the way an error message should quote it back.
fn render_scalar(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => format!("\"{s}\""),
        other => serde_yaml::to_string(other)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "an unreadable value".to_string()),
    }
}

/// Every rule violation in the document, in a deterministic order: version
/// first, then structural problems plane by plane in file order, then the
/// cross-item dependency rules.
///
/// Returns an empty vector for a clean file. This never returns an error of
/// its own: a document that parsed is always inspectable.
pub fn validate(file: &PlaneFile) -> Vec<Violation> {
    let mut violations = Vec::new();

    if file.version != SUPPORTED_VERSION {
        violations.push(Violation::new(
            ViolationCode::UnsupportedVersion,
            format!(
                "planes file declares version {}, which this build of codegraph does not read. The supported version is {SUPPORTED_VERSION}",
                file.version
            ),
        ));
    }

    // Pass 1: per-plane and per-item structure, plus the two uniqueness
    // tables. First-seen location is remembered so a duplicate can name both
    // ends, which is the only form of the message that is actually
    // actionable.
    let mut plane_seen: HashMap<&str, String> = HashMap::new();
    let mut item_seen: HashMap<&str, String> = HashMap::new();
    // (item id, its location) in file order, for the dependency passes.
    let mut items: Vec<(&str, String)> = Vec::new();

    for (pi, plane) in file.planes.iter().enumerate() {
        let plane_loc = plane_at(pi, &plane.id);

        if !is_valid_id(&plane.id) {
            violations.push(Violation::new(
                ViolationCode::InvalidId,
                format!(
                    "plane id \"{}\" at {plane_loc} is not usable as an identifier. {ID_PATTERN_DESCRIPTION}",
                    plane.id
                ),
            ));
        }

        match plane_seen.get(plane.id.as_str()) {
            Some(first) => violations.push(Violation::new(
                ViolationCode::DuplicatePlaneId,
                format!(
                    "plane id \"{}\" is declared twice, at {first} and at {plane_loc}. Plane ids must be unique within the file",
                    plane.id
                ),
            )),
            None => {
                plane_seen.insert(plane.id.as_str(), plane_loc.clone());
            }
        }

        for (ii, item) in plane.items.iter().enumerate() {
            let item_loc = format!("{} id \"{}\"", item_at(pi, ii, &plane.id), item.id);

            if !is_valid_id(&item.id) {
                violations.push(Violation::new(
                    ViolationCode::InvalidId,
                    format!(
                        "work item id \"{}\" at {} is not usable as an identifier. {ID_PATTERN_DESCRIPTION}",
                        item.id,
                        item_at(pi, ii, &plane.id)
                    ),
                ));
            }

            if item.touches.is_empty() {
                violations.push(Violation::new(
                    ViolationCode::EmptyTouches,
                    format!(
                        "work item \"{}\" at {} has an empty \"touches\" list. A work item is a hyperedge over the code it touches, so an item that touches nothing cannot be queried, collided, or blast radiused. List at least one file, symbol, or glob",
                        item.id,
                        item_at(pi, ii, &plane.id)
                    ),
                ));
            }

            match item_seen.get(item.id.as_str()) {
                Some(first) => violations.push(Violation::new(
                    ViolationCode::DuplicateItemId,
                    format!(
                        "work item id \"{}\" is declared twice, at {first} and at {item_loc}. Work item ids must be unique across the whole file, not only within a plane, because \"codegraph plan show\" takes a bare item id",
                        item.id
                    ),
                )),
                None => {
                    item_seen.insert(item.id.as_str(), item_loc.clone());
                    items.push((item.id.as_str(), item_loc));
                }
            }
        }
    }

    // Pass 2: referential integrity of depends_on.
    for (pi, plane) in file.planes.iter().enumerate() {
        for (ii, item) in plane.items.iter().enumerate() {
            let loc = item_at(pi, ii, &plane.id);
            for dep in &item.depends_on {
                if dep == &item.id {
                    violations.push(Violation::new(
                        ViolationCode::SelfDependency,
                        format!(
                            "work item \"{}\" at {loc} lists itself in \"depends_on\". An item cannot block itself",
                            item.id
                        ),
                    ));
                } else if !item_seen.contains_key(dep.as_str()) {
                    violations.push(Violation::new(
                        ViolationCode::DanglingDependency,
                        format!(
                            "work item \"{}\" at {loc} depends_on \"{dep}\", which is not a work item in this file. Every \"depends_on\" entry must name an item id declared somewhere in the same planes file",
                            item.id
                        ),
                    ));
                }
            }
        }
    }

    // Pass 3: acyclicity. Only edges that actually exist are walked, so a
    // dangling reference reported above never also shows up here as a
    // phantom cycle.
    for cycle in find_cycles(file, &item_seen) {
        violations.push(Violation::new(
            ViolationCode::DependencyCycle,
            format!(
                "\"depends_on\" cycle: {}. Work item dependencies must form a directed acyclic graph, so no item in this chain can ever start",
                cycle.join(" -> ")
            ),
        ));
    }

    violations
}

/// Every distinct `depends_on` cycle, each rendered as a path that starts and
/// ends on the same id (`RA-1 -> RA-2 -> RA-1`).
///
/// Iterative depth-first search with an explicit stack, so a pathological
/// file cannot blow the real one. Each cycle is reported once: the search
/// rotates every path it finds to start at its smallest id and dedupes, so
/// the same loop entered from two different roots is not reported twice.
fn find_cycles(file: &PlaneFile, known: &HashMap<&str, String>) -> Vec<Vec<String>> {
    // Adjacency over item ids, restricted to edges whose target exists.
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for (_, item) in file.items() {
        if edges.contains_key(item.id.as_str()) {
            continue;
        }
        order.push(item.id.as_str());
        let targets = item
            .depends_on
            .iter()
            .map(String::as_str)
            .filter(|dep| *dep != item.id.as_str() && known.contains_key(dep))
            .collect();
        edges.insert(item.id.as_str(), targets);
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Closed,
    }

    let mut mark: HashMap<&str, Mark> = HashMap::new();
    let mut found: Vec<Vec<String>> = Vec::new();
    let mut seen_cycles: Vec<Vec<String>> = Vec::new();

    for root in &order {
        if mark.contains_key(root) {
            continue;
        }
        // (node, index of the next child to visit) plus the current path.
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        let mut path: Vec<&str> = vec![root];
        mark.insert(root, Mark::Open);

        while let Some((node, child_index)) = stack.pop() {
            let children = edges.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if child_index >= children.len() {
                mark.insert(node, Mark::Closed);
                path.pop();
                continue;
            }
            stack.push((node, child_index + 1));
            let child = children[child_index];
            match mark.get(child) {
                Some(Mark::Open) => {
                    // `child` is on the current path: everything from it
                    // onward is the cycle.
                    if let Some(start) = path.iter().position(|n| *n == child) {
                        let mut cycle: Vec<String> =
                            path[start..].iter().map(|s| s.to_string()).collect();
                        cycle.push(child.to_string());
                        let key = canonical_cycle(&cycle);
                        if !seen_cycles.contains(&key) {
                            seen_cycles.push(key);
                            found.push(cycle);
                        }
                    }
                }
                Some(Mark::Closed) => {}
                None => {
                    mark.insert(child, Mark::Open);
                    path.push(child);
                    stack.push((child, 0));
                }
            }
        }
    }

    found
}

/// Rotate a cycle so it starts at its smallest id, dropping the repeated
/// closing element. Two renderings of the same loop then compare equal.
fn canonical_cycle(cycle: &[String]) -> Vec<String> {
    let body = &cycle[..cycle.len().saturating_sub(1)];
    if body.is_empty() {
        return Vec::new();
    }
    let start = body
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(i, _)| i)
        .unwrap_or(0);
    body[start..].iter().chain(&body[..start]).cloned().collect()
}

/// Read, parse, and validate a planes file.
///
/// Fails if the file is missing, is not YAML, does not match the version 1
/// schema, or breaks any rule [`validate`] checks. The error lists every
/// violation at once rather than the first, so one run fixes the whole file.
pub fn load(path: &Path) -> Result<PlaneFile> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read the planes file at {}. Create one with \"codegraph init\", or pass the path with --planes-file",
            path.display()
        )
    })?;

    let file = parse_str(&text)
        .with_context(|| format!("cannot parse the planes file at {}", path.display()))?;

    let violations = validate(&file);
    if !violations.is_empty() {
        return Err(anyhow!("{}", render_violations(path, &violations)));
    }

    Ok(file)
}

/// Read and parse a planes file without enforcing the validation rules.
///
/// This is what `codegraph plan lint` wants: it needs the parsed document in
/// hand so it can report every violation, which [`load`] would have refused
/// to hand back. Anything that acts on a plan should call [`load`] instead.
pub fn load_unvalidated(path: &Path) -> Result<PlaneFile> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read the planes file at {}. Create one with \"codegraph init\", or pass the path with --planes-file",
            path.display()
        )
    })?;
    parse_str(&text)
        .with_context(|| format!("cannot parse the planes file at {}", path.display()))
}

/// Render a violation list the way [`load`] and `plan lint` both render it:
/// a counted header naming the file, then one numbered, tagged line each.
pub fn render_violations(path: &Path, violations: &[Violation]) -> String {
    let mut out = format!(
        "{} failed validation with {} problem{}:",
        path.display(),
        violations.len(),
        if violations.len() == 1 { "" } else { "s" }
    );
    for (i, v) in violations.iter().enumerate() {
        out.push_str(&format!("\n  {}. {v}", i + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> PlaneFile {
        parse_str(yaml).expect("fixture must parse")
    }

    const ONE_ITEM: &str = r#"
version: 1
project: demo
planes:
  - id: p1
    title: First
    status: active
    horizon: now
    items:
      - id: A-1
        title: Thing
        kind: feature
        status: planned
        touches:
          - file: src/cli.rs
"#;

    #[test]
    fn clean_file_has_no_violations() {
        assert_eq!(validate(&parse(ONE_ITEM)), Vec::new());
    }

    #[test]
    fn cycle_is_reported_once_with_its_path() {
        let yaml = ONE_ITEM.to_string()
            + r#"      - id: A-2
        title: Other
        kind: fix
        status: planned
        depends_on: [A-1]
        touches:
          - file: src/main.rs
"#;
        // Close the loop by making A-1 depend on A-2.
        let yaml = yaml.replace(
            "        touches:\n          - file: src/cli.rs\n",
            "        depends_on: [A-2]\n        touches:\n          - file: src/cli.rs\n",
        );
        let violations = validate(&parse(&yaml));
        let cycles: Vec<_> = violations
            .iter()
            .filter(|v| v.code == ViolationCode::DependencyCycle)
            .collect();
        assert_eq!(cycles.len(), 1, "expected exactly one cycle: {violations:?}");
        assert!(
            cycles[0].message.contains("A-1 -> A-2 -> A-1")
                || cycles[0].message.contains("A-2 -> A-1 -> A-2"),
            "cycle path not rendered: {}",
            cycles[0].message
        );
    }

    #[test]
    fn canonical_cycle_rotates_to_smallest() {
        let a = canonical_cycle(&["b".into(), "c".into(), "a".into(), "b".into()]);
        let b = canonical_cycle(&["a".into(), "b".into(), "c".into(), "a".into()]);
        assert_eq!(a, b);
    }
}
