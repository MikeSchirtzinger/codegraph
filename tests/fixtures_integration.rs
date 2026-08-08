//! R3b, item 1: per-fixture integration tests. Indexes each fixture project
//! through the real library pipeline (`codegraph::index::index_project`,
//! which runs the R1 resolver as its own last step) and asserts every case
//! in its `expected.yaml` manifest — nodes' computed `qualified_name` (R0),
//! edges' resolver bindings (R1), and derived cross-file cycles (R2/D4).
//!
//! Generic and manifest-driven throughout (`common::check_fixture`): a case
//! added to a manifest later (e.g. the go fixture's planned `member_of`
//! receiver-method case) is picked up automatically, with no change needed
//! here.

mod common;

use common::check_fixture;

#[tokio::test]
async fn rust_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/rust/expected.yaml", "fixture-rust").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

#[tokio::test]
async fn typescript_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/typescript/expected.yaml", "fixture-typescript").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

#[tokio::test]
async fn python_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/python/expected.yaml", "fixture-python").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

#[tokio::test]
async fn go_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/go/expected.yaml", "fixture-go").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

#[tokio::test]
async fn java_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/java/expected.yaml", "fixture-java").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

#[tokio::test]
async fn c_cpp_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/c-cpp/expected.yaml", "fixture-c-cpp").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

#[tokio::test]
async fn polyglot_fixture_matches_manifest() {
    let c = check_fixture("tests/fixtures/polyglot/expected.yaml", "fixture-polyglot").await;
    assert!(c.nodes > 0 && c.edges > 0 && c.cycles > 0);
}

/// `rename-refactor/before` follows the ordinary per-project manifest
/// schema in its own right (see `tests/fixtures/README.md`) — this checks
/// its own nodes/edges independent of the before/after delta, which
/// `tests/kill_test.rs` covers separately.
#[tokio::test]
async fn rename_refactor_before_matches_manifest() {
    let c = check_fixture(
        "tests/fixtures/rename-refactor/before/expected.yaml",
        "fixture-rename-before",
    )
    .await;
    assert!(c.nodes > 0 && c.edges > 0);
}

#[tokio::test]
async fn rename_refactor_after_matches_manifest() {
    let c = check_fixture(
        "tests/fixtures/rename-refactor/after/expected.yaml",
        "fixture-rename-after",
    )
    .await;
    assert!(c.nodes > 0 && c.edges > 0);
}
