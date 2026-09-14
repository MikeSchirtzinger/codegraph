//! Schema tests for `.codegraph/planes.yaml` (lane P0).
//!
//! Every validation rule in `codegraph::plan::schema` gets a fixture under
//! `tests/fixtures/planes/` and a test that asserts on the error *message*,
//! not merely that an error happened. The messages are the deliverable: they
//! are read by a human fixing a file and by an agent deciding whether it may
//! proceed, so a message that says "invalid" and stops is a failure of the
//! feature even when the `Result` is correct.

use std::path::{Path, PathBuf};

use codegraph::plan::model::{Horizon, Kind, Selector, Status};
use codegraph::plan::schema::{load, load_unvalidated, validate, ViolationCode};
use codegraph::plan::{item_record_id, plane_record_id};

/// Absolute path to one fixture, independent of the cwd `cargo test` runs
/// from.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/planes")
        .join(name)
}

/// Load a fixture that is expected to fail, and return the full error chain
/// rendered the way a user sees it on the terminal.
fn load_error(name: &str) -> String {
    let err = load(&fixture(name)).expect_err("fixture must be rejected");
    format!("{err:#}")
}

/// Assert that `haystack` contains `needle`, printing the whole message on
/// failure so a drift in wording is immediately readable.
fn assert_says(haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "message did not contain {needle:?}.\nFull message:\n{haystack}"
    );
}

// ============================================================================
// The happy path
// ============================================================================

#[test]
fn valid_multi_plane_file_loads_with_every_field_intact() {
    let file = load(&fixture("valid-multi-plane.yaml")).expect("valid fixture must load");

    assert_eq!(file.version, 1);
    assert_eq!(file.project.as_deref(), Some("fixture-project"));
    assert_eq!(file.planes.len(), 2);

    let run_anywhere = &file.planes[0];
    assert_eq!(run_anywhere.id, "run-anywhere");
    assert_eq!(run_anywhere.status, Status::Active);
    assert_eq!(run_anywhere.horizon, Horizon::Now);
    assert_eq!(
        run_anywhere.summary.as_deref(),
        Some("One command, sensible defaults, honest errors.")
    );
    assert_eq!(run_anywhere.items.len(), 2);

    // The hyperedge: one item, three incidences, one per selector kind.
    let ra1 = &run_anywhere.items[1];
    assert_eq!(ra1.id, "RA-1");
    assert_eq!(ra1.kind, Kind::Feature);
    assert_eq!(ra1.status, Status::Planned);
    assert_eq!(ra1.depends_on, vec!["RA-0".to_string()]);
    assert_eq!(ra1.spec.as_deref(), Some("specs/resolution-layer-v1.md"));
    assert!(ra1.notes.is_some());

    let selectors: Vec<Selector> = ra1.touches.iter().map(|t| t.selector).collect();
    assert_eq!(
        selectors,
        vec![Selector::File, Selector::Symbol, Selector::Glob]
    );
    let raws: Vec<&str> = ra1.touches.iter().map(|t| t.raw.as_str()).collect();
    assert_eq!(raws, vec!["src/cli.rs", "Commands::Index", "src/index/**"]);

    // A plane with no summary and an abandoned item still parses.
    let planes_core = &file.planes[1];
    assert_eq!(planes_core.horizon, Horizon::Next);
    assert!(planes_core.summary.is_none());
    assert_eq!(planes_core.items[1].status, Status::Abandoned);
    assert_eq!(planes_core.items[1].kind, Kind::Docs);

    // Four items across two planes, reachable by a flat walk and by id.
    assert_eq!(file.items().count(), 4);
    let (plane, item) = file.find_item("PC-1").expect("PC-1 must be found");
    assert_eq!(plane.id, "planes-core");
    assert_eq!(item.title, "Ingest the planes file and resolve every touch");
}

#[test]
fn a_valid_file_produces_no_violations() {
    let file = load_unvalidated(&fixture("valid-multi-plane.yaml")).expect("parse");
    assert_eq!(validate(&file), Vec::new());
}

#[test]
fn record_ids_are_derived_from_the_file_ids() {
    let file = load(&fixture("valid-multi-plane.yaml")).expect("load");
    let project = file.project.as_deref().expect("fixture declares a project");

    assert_eq!(
        plane_record_id(project, &file.planes[0].id),
        "work_plane:fixture-project__run-anywhere"
    );
    assert_eq!(
        item_record_id(project, &file.planes[0].items[1].id),
        "work_item:fixture-project__RA-1"
    );
}

// ============================================================================
// Version
// ============================================================================

#[test]
fn missing_version_names_the_supported_version() {
    let msg = load_error("missing-version.yaml");
    assert_says(&msg, "has no \"version\" key");
    assert_says(&msg, "reads version 1");
}

#[test]
fn unsupported_version_names_both_the_found_and_the_supported_version() {
    let msg = load_error("unsupported-version.yaml");
    assert_says(&msg, "declares version 7");
    assert_says(&msg, "The supported version is 1");
}

#[test]
fn non_numeric_version_is_rejected_by_type() {
    let msg = load_error("non-numeric-version.yaml");
    assert_says(&msg, "\"version\" must be a whole number");
    assert_says(&msg, "found \"one\"");
}

// ============================================================================
// Uniqueness
// ============================================================================

#[test]
fn duplicate_plane_id_names_both_locations() {
    let msg = load_error("duplicate-plane-id.yaml");
    assert_says(&msg, "[duplicate-plane-id]");
    assert_says(&msg, "plane id \"same-plane\" is declared twice");
    // Both ends, by index, so the second copy is findable in a long file.
    assert_says(&msg, "planes[0]");
    assert_says(&msg, "planes[2]");
}

#[test]
fn duplicate_item_id_names_both_locations() {
    let msg = load_error("duplicate-item-id.yaml");
    assert_says(&msg, "[duplicate-item-id]");
    assert_says(&msg, "work item id \"DUP-1\" is declared twice");
    assert_says(&msg, "planes[0].items[0]");
    assert_says(&msg, "planes[1].items[0]");
}

// ============================================================================
// Dependencies
// ============================================================================

#[test]
fn dangling_dependency_names_the_dangling_ref() {
    let msg = load_error("dangling-dependency.yaml");
    assert_says(&msg, "[dangling-dependency]");
    assert_says(&msg, "work item \"D-1\"");
    assert_says(&msg, "depends_on \"D-404\", which is not a work item in this file");
}

#[test]
fn dependency_cycle_names_the_cycle_path() {
    let msg = load_error("dependency-cycle.yaml");
    assert_says(&msg, "[dependency-cycle]");
    assert_says(&msg, "\"depends_on\" cycle:");
    // Every member of the loop appears, and the path closes on itself.
    for id in ["C-1", "C-2", "C-3"] {
        assert_says(&msg, id);
    }
    assert_says(&msg, " -> ");

    // Exactly one cycle, not one per entry point into the same loop.
    let file = load_unvalidated(&fixture("dependency-cycle.yaml")).expect("parse");
    let cycles = validate(&file)
        .into_iter()
        .filter(|v| v.code == ViolationCode::DependencyCycle)
        .count();
    assert_eq!(cycles, 1, "the same loop must be reported once");
}

#[test]
fn self_dependency_is_rejected_and_is_not_reported_as_a_cycle() {
    let msg = load_error("self-dependency.yaml");
    assert_says(&msg, "[self-dependency]");
    assert_says(&msg, "work item \"S-1\"");
    assert_says(&msg, "lists itself in \"depends_on\"");
    assert!(
        !msg.contains("[dependency-cycle]"),
        "a self edge is one problem, not two:\n{msg}"
    );
}

// ============================================================================
// Structure
// ============================================================================

#[test]
fn empty_touches_is_rejected_because_a_hyperedge_needs_an_incidence() {
    let msg = load_error("empty-touches.yaml");
    assert_says(&msg, "[empty-touches]");
    assert_says(&msg, "work item \"E-1\"");
    assert_says(&msg, "has an empty \"touches\" list");
    assert_says(&msg, "List at least one file, symbol, or glob");
}

#[test]
fn invalid_id_is_rejected_and_the_accepted_shape_is_stated() {
    let msg = load_error("invalid-id.yaml");
    assert_says(&msg, "[invalid-id]");
    assert_says(&msg, "work item id \"has space\"");
    assert_says(&msg, "must start with a letter or digit");
}

// ============================================================================
// Enumerations. Every unknown value lists what would have been accepted.
// ============================================================================

#[test]
fn unknown_status_lists_accepted_values() {
    let msg = load_error("unknown-status.yaml");
    assert_says(&msg, "unknown status value \"wip\"");
    assert_says(&msg, "accepted values are planned, active, done, abandoned");
}

#[test]
fn unknown_horizon_lists_accepted_values() {
    let msg = load_error("unknown-horizon.yaml");
    assert_says(&msg, "unknown horizon value \"someday\"");
    assert_says(&msg, "accepted values are now, next, later");
}

#[test]
fn unknown_kind_lists_accepted_values() {
    let msg = load_error("unknown-kind.yaml");
    assert_says(&msg, "unknown kind value \"chore\"");
    assert_says(
        &msg,
        "accepted values are feature, fix, perf, refactor, docs, research",
    );
}

#[test]
fn unknown_touch_selector_lists_accepted_selectors() {
    let msg = load_error("unknown-selector.yaml");
    assert_says(&msg, "unknown touch selector \"module\"");
    assert_says(&msg, "accepted selectors are file, symbol, glob");
}

#[test]
fn a_touch_with_an_empty_value_is_rejected() {
    let msg = load_error("empty-touch-value.yaml");
    assert_says(&msg, "touch entry \"file\" has an empty value");
}

// ============================================================================
// Cross-cutting
// ============================================================================

#[test]
fn a_missing_planes_file_says_how_to_create_one() {
    let msg = load(&fixture("does-not-exist.yaml"))
        .expect_err("a missing file must fail")
        .to_string();
    assert_says(&msg, "cannot read the planes file at");
    assert_says(&msg, "codegraph init");
}

/// House rule: no em dash anywhere in text a user reads. These messages are
/// user-facing, so the rule applies to every one of them.
#[test]
fn no_error_message_uses_an_em_dash() {
    let fixtures = [
        "missing-version.yaml",
        "unsupported-version.yaml",
        "non-numeric-version.yaml",
        "duplicate-plane-id.yaml",
        "duplicate-item-id.yaml",
        "dangling-dependency.yaml",
        "dependency-cycle.yaml",
        "self-dependency.yaml",
        "empty-touches.yaml",
        "invalid-id.yaml",
        "unknown-status.yaml",
        "unknown-horizon.yaml",
        "unknown-kind.yaml",
        "unknown-selector.yaml",
        "empty-touch-value.yaml",
    ];
    for name in fixtures {
        let msg = load_error(name);
        for bad in ['\u{2014}', '\u{2013}'] {
            assert!(
                !msg.contains(bad),
                "{name} produced a message containing {bad:?}:\n{msg}"
            );
        }
    }
}

/// `load` refuses a document with any violation, and the header counts them,
/// so one run of the tool fixes the whole file instead of one problem per
/// round trip.
#[test]
fn validation_failures_are_reported_together_with_a_count() {
    let msg = load_error("duplicate-plane-id.yaml");
    assert_says(&msg, "failed validation with 1 problem:");
    assert_says(&msg, "  1. [");
}
