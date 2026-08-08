//! R3b, item 3: the gate kill-test, operationalized
//! (`specs/resolution-layer-v1.md` §R3: "the exact scenario a skeptical
//! engineer will try in the first two hours"). Indexes the
//! `rename-refactor/{before,after}` fixture pair as two independent fresh
//! indexes (per `tests/fixtures/README.md`'s "For R3b" note — the *shape* of
//! this assertion doesn't need real incremental re-resolution, R4's
//! territory) and asserts the rename's breakage is surfaced, never silently
//! dropped: `stale_caller` must show `RESOLVED -> UNRESOLVED` across the
//! rename, and a reverse-dependency query on the new name must show only
//! the correctly-migrated caller — no false positives from the stale one.

mod common;

use codegraph::graph::dependencies::get_reverse_dependencies;
use common::{fresh_db, index_fixture, load_rename_manifest, Loaded};

#[tokio::test]
async fn kill_test_stale_caller_surfaces_and_rdeps_has_no_false_positives() {
    let parent = load_rename_manifest("tests/fixtures/rename-refactor/expected.yaml");
    assert!(
        !parent.kill_test.callers.is_empty(),
        "kill-test manifest has no callers to check — fixture regression"
    );

    let before_db = fresh_db().await.expect("fresh before/ store");
    index_fixture(&before_db, "rename-before", "tests/fixtures/rename-refactor/before")
        .await
        .expect("index before/");
    let before = Loaded::fetch(&before_db, "rename-before").await.expect("load before/");

    let after_db = fresh_db().await.expect("fresh after/ store");
    index_fixture(&after_db, "rename-after", "tests/fixtures/rename-refactor/after")
        .await
        .expect("index after/");
    let after = Loaded::fetch(&after_db, "rename-after").await.expect("load after/");

    for caller in &parent.kill_test.callers {
        let before_from = before
            .by_qualified_name(&caller.from)
            .unwrap_or_else(|| panic!("before/: caller {:?} not indexed", caller.from));
        let before_edge = before
            .edges
            .iter()
            .find(|e| e.edge_type == "calls" && e.from_id == before_from.node_id)
            .unwrap_or_else(|| panic!("before/: no calls edge from {:?}", caller.from));
        assert_eq!(before_edge.confidence, caller.before.confidence, "before/{}: confidence", caller.from);
        assert_eq!(before_edge.resolved_by, caller.before.resolved_by, "before/{}: resolved_by", caller.from);
        let before_target = before.by_node_id(&before_edge.to_id).map(|n| n.qualified_name.clone());
        assert_eq!(
            before_target.as_deref(), caller.before.target.as_deref(),
            "before/{}: target", caller.from
        );

        let after_from = after
            .by_qualified_name(&caller.from)
            .unwrap_or_else(|| panic!("after/: caller {:?} not indexed", caller.from));
        let after_edge = after
            .edges
            .iter()
            .find(|e| e.edge_type == "calls" && e.from_id == after_from.node_id)
            .unwrap_or_else(|| panic!("after/: no calls edge from {:?}", caller.from));
        assert_eq!(after_edge.confidence, caller.after.confidence, "after/{}: confidence", caller.from);
        assert_eq!(after_edge.resolved_by, caller.after.resolved_by, "after/{}: resolved_by", caller.from);
        let after_target = after.by_node_id(&after_edge.to_id).map(|n| n.qualified_name.clone());
        assert_eq!(
            after_target.as_deref(), caller.after.target.as_deref(),
            "after/{}: target", caller.from
        );

        if caller.must_flag {
            assert_ne!(
                before_edge.confidence, after_edge.confidence,
                "{}: must_flag=true but before/after confidence didn't transition — kill-test regression",
                caller.from
            );
        } else {
            assert_eq!(
                after_edge.confidence, "RESOLVED",
                "{}: must_flag=false (correctly migrated) but after/ isn't RESOLVED",
                caller.from
            );
        }
    }

    // THE visible-not-dropped assertion: rdeps on the OLD (renamed-away)
    // name against the after/ store must surface the stale caller via the
    // project-wide `stale_references` bare-tail scan — the only way an
    // UNRESOLVED edge (zero candidates, empty to_id by definition) is ever
    // attributable to any node at all. Depth 1: this check is about direct
    // call-site resolution, not transitive reachability.
    let old_name_bare = parent.kill_test.qualified_name_before.rsplit("::").next().unwrap();
    let rdeps_old = get_reverse_dependencies(&after_db, "rename-after", old_name_bare, &["calls"], 1, false)
        .await
        .expect("rdeps on old name");
    assert!(
        rdeps_old.groups.is_empty(),
        "kill-test: no live symbol named {old_name_bare:?} should exist in after/, got {:?}",
        rdeps_old.groups
    );
    assert!(
        rdeps_old.stale_references.iter().any(|r| r.from_name == "use_stale"),
        "kill-test regression: stale caller must surface via stale_references, got {:?}",
        rdeps_old.stale_references
    );

    // No false positives: rdeps on the NEW name must show only the properly
    // migrated DIRECT caller, never the stale one (depth 1 — `main` also
    // transitively reaches helper_v2 through updated_caller at depth 2,
    // which is a true positive, not the false-positive shape this check is
    // for; see rename-refactor/after/src/main.rs).
    let new_name_bare = parent.kill_test.qualified_name_after.rsplit("::").next().unwrap();
    let rdeps_new = get_reverse_dependencies(&after_db, "rename-after", new_name_bare, &["calls"], 1, false)
        .await
        .expect("rdeps on new name");
    assert_eq!(
        rdeps_new.groups.len(), 1,
        "kill-test: expected exactly one live symbol named {new_name_bare:?}, got {:?}",
        rdeps_new.groups
    );
    let caller_names: Vec<&str> = rdeps_new.groups[0].items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        caller_names, vec!["use_updated"],
        "kill-test regression: rdeps on the new name must not show the stale caller as a false positive"
    );
    assert!(
        rdeps_new.stale_references.is_empty(),
        "kill-test: no stale reference should attach to a query on the NEW name, got {:?}",
        rdeps_new.stale_references
    );
}
