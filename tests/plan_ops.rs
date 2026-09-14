//! `codegraph plan ...` end to end, against a real index of a real fixture.
//!
//! Every assertion here is a fact about `tests/fixtures/rust` that its own
//! `expected.yaml` already records, so these tests and the resolver's tests
//! are pinned to one ground truth rather than two:
//!
//! - `helper` is defined in both `alpha.rs` and `beta.rs` (case c), which is
//!   what makes `- symbol: helper` AMBIGUOUS with exactly two candidates.
//! - `db::connection::connect` has exactly two RESOLVED callers, `run`
//!   (case b) and `delta::call_connect_via_module_import` (case bonus-r2),
//!   which is what the blast radius of `- symbol: connect` must return at
//!   depth 1.
//!
//! The last test is the feature's reason to exist: a plan that nobody
//! edited goes stale because the code moved underneath it, and codegraph
//! reports that in the same words it reports a stale code reference.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::index::{index_project, IndexConfig, IndexingTier};
use codegraph::plan::model::{Selector, TouchConfidence};
use codegraph::plan::ops::{self, ListFilter, MatchKind};
use codegraph::plan::resolve::UnresolvedReason;

const PROJECT: &str = "plan-ops";
const PLANES: &str = "tests/fixtures/planes/ops-rust.yaml";
const FIXTURE: &str = "tests/fixtures/rust";

/// A fresh store with `tests/fixtures/rust` indexed and `ops-rust.yaml`
/// synced into it.
async fn synced() -> (Arc<Surreal<Any>>, ops::SyncReport) {
    let db = common::fresh_db().await.expect("fresh in-memory store");
    let result = common::index_fixture(&db, PROJECT, FIXTURE)
        .await
        .expect("indexing the rust fixture");
    assert!(
        result.errors.is_empty(),
        "fixture must index cleanly: {:?}",
        result.errors
    );
    let report = ops::sync(&db, PROJECT, &common::repo_path(PLANES))
        .await
        .expect("plan sync");
    (db, report)
}

/// The one touch of `item` whose `raw` is `raw`.
fn touch_of<'a>(show: &'a ops::ShowReport, raw: &str) -> &'a codegraph::plan::model::ResolvedTouch {
    show.touches
        .iter()
        .map(|t| &t.touch)
        .find(|t| t.raw == raw)
        .unwrap_or_else(|| panic!("item {} has no touch {raw:?}", show.item.item_id))
}

#[tokio::test]
async fn sync_binds_every_confidence_and_names_every_reason() {
    let (db, report) = synced().await;

    // Selector level: the six entries an author wrote across OPS-1 and
    // OPS-2 plus the rest of the file. Counted as entries, not incidences,
    // because an author fixes entries.
    assert_eq!(report.planes, 2, "{report}");
    assert_eq!(report.items, 6, "{report}");
    assert_eq!(report.selectors, 11, "{report}");
    assert_eq!(report.selectors_resolved, 8, "{report}");
    assert_eq!(report.selectors_ambiguous, 1, "{report}");
    assert_eq!(report.selectors_unresolved, 2, "{report}");
    assert_eq!(
        report.unindexed, 1,
        "Cargo.toml binds to a real path the graph holds nothing for: {report}"
    );
    assert_eq!(
        report.selectors_resolved + report.selectors_ambiguous + report.selectors_unresolved,
        report.selectors,
        "the selector tallies must partition the selectors"
    );
    assert_eq!(
        report.resolved + report.ambiguous + report.unresolved,
        report.touches,
        "the row tallies must partition the rows"
    );
    // Globs expand, so there are strictly more incidences than entries.
    assert!(
        report.touches > report.selectors,
        "two globs must expand to more rows than entries: {report}"
    );

    // Only what is genuinely not there is UNRESOLVED. Cargo.toml is on
    // disk, so it bound, and it is not in this list.
    let reasons: Vec<(String, Option<UnresolvedReason>)> = report
        .unresolved_touches
        .iter()
        .map(|t| (t.touch.touch.raw.clone(), t.touch.reason))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (
                "no_such_function_anywhere".to_string(),
                Some(UnresolvedReason::NoSuchSymbol)
            ),
            (
                "src/does_not_exist.rs".to_string(),
                Some(UnresolvedReason::NoSuchFile)
            ),
        ],
        "{report}"
    );

    // The ambiguous entry names both definitions and picks neither.
    assert_eq!(report.ambiguous_touches.len(), 1, "{report}");
    let ambiguous = &report.ambiguous_touches[0];
    assert_eq!(ambiguous.item_id, "OPS-1");
    assert_eq!(ambiguous.touch.touch.raw, "helper");
    assert_eq!(ambiguous.touch.touch.to_id, None, "never a silent pick");

    let loaded = common::Loaded::fetch(&db, PROJECT).await.expect("load nodes");
    let mut expected: Vec<String> = ["alpha::helper", "beta::helper"]
        .iter()
        .map(|qn| {
            loaded
                .by_qualified_name(qn)
                .unwrap_or_else(|| panic!("fixture must define {qn}"))
                .node_id
                .clone()
        })
        .collect();
    expected.sort();
    assert_eq!(
        ambiguous.touch.touch.candidates, expected,
        "both definitions of `helper` must be listed as candidates"
    );

    // The index the verdicts were taken against is reported, so an empty
    // index can never be mistaken for an empty roadmap.
    assert!(report.indexed_files >= 9, "{report}");
    assert!(report.indexed_symbols >= 14, "{report}");
}

#[tokio::test]
async fn a_glob_becomes_one_incidence_per_matched_file() {
    let (db, _) = synced().await;
    let show = ops::show(&db, PROJECT, "OPS-2").await.expect("show OPS-2");

    let expanded: Vec<&ops::TouchView> = show
        .touches
        .iter()
        .filter(|t| t.touch.selector == Selector::Glob)
        .collect();
    assert_eq!(
        expanded.len(),
        1,
        "src/db/** matches exactly one indexed file in this fixture"
    );
    assert_eq!(expanded[0].touch.raw, "src/db/**", "the row keeps the raw glob");
    assert_eq!(
        expanded[0].touch.to_id.as_deref(),
        Some("src/db/connection.rs"),
        "and binds to the path it matched"
    );
    assert_eq!(expanded[0].indexed, Some(true));

    // The whole-tree glob on the abandoned item expands to every indexed
    // Rust file, which is what makes the incidence count exceed the
    // selector count.
    let sweep = ops::show(&db, PROJECT, "OPS-6").await.expect("show OPS-6");
    assert_eq!(sweep.item.selector_count, 1);
    assert!(
        sweep.item.touch_count >= 9,
        "src/**/*.rs must expand across the fixture: {}",
        sweep.item.touch_count
    );
    assert_eq!(sweep.item.touch_count, sweep.item.resolved);
}

#[tokio::test]
async fn sync_twice_is_idempotent() {
    let (db, first) = synced().await;
    let before = ops::list(&db, PROJECT, &ListFilter::default())
        .await
        .expect("list after first sync");

    let second = ops::sync(&db, PROJECT, &common::repo_path(PLANES))
        .await
        .expect("second plan sync");
    let after = ops::list(&db, PROJECT, &ListFilter::default())
        .await
        .expect("list after second sync");

    assert_eq!(first.planes, second.planes);
    assert_eq!(first.items, second.items);
    assert_eq!(first.touches, second.touches);
    assert_eq!(first.resolved, second.resolved);
    assert_eq!(first.ambiguous, second.ambiguous);
    assert_eq!(first.unresolved, second.unresolved);
    assert_eq!(
        second.removed_planes, 0,
        "an unchanged file removes nothing"
    );
    assert_eq!(second.removed_items, 0, "an unchanged file removes nothing");

    // Ids are derived purely from the identity tuple, so a re-sync
    // reproduces them byte for byte rather than minting new ones.
    assert_eq!(before, after, "a re-sync must be a no-op to every reader");

    let ids: Vec<&str> = after
        .planes
        .iter()
        .flat_map(|p| p.items.iter().map(|i| i.node_id.as_str()))
        .collect();
    assert!(ids.contains(&"work_item:plan-ops__OPS-1"), "{ids:?}");
}

#[tokio::test]
async fn touching_finds_the_item_by_file_and_by_symbol() {
    let (db, _) = synced().await;

    let by_file = ops::touching(&db, PROJECT, "src/main.rs")
        .await
        .expect("touching by path");
    let items: BTreeSet<&str> = by_file
        .hits
        .iter()
        .filter(|h| h.matched_by == MatchKind::Bound)
        .map(|h| h.item.item_id.as_str())
        .collect();
    assert_eq!(
        items,
        ["OPS-1", "OPS-2", "OPS-5", "OPS-6"].into_iter().collect(),
        "every item that binds to main.rs, including through the sweep glob: {by_file}"
    );
    assert_eq!(
        by_file.target_ids,
        vec!["src/main.rs".to_string()],
        "the target itself resolved, so an empty hit list would mean nothing is planned here"
    );

    let by_symbol = ops::touching(&db, PROJECT, "connect")
        .await
        .expect("touching by symbol");
    let items: Vec<&str> = by_symbol
        .hits
        .iter()
        .filter(|h| h.matched_by == MatchKind::Bound)
        .map(|h| h.item.item_id.as_str())
        .collect();
    assert_eq!(items, vec!["OPS-4"], "{by_symbol}");
    assert_eq!(
        by_symbol.hits[0].touch.touch.selector,
        Selector::Symbol,
        "and it says which touch matched"
    );

    let nothing = ops::touching(&db, PROJECT, "src/no/such/file.rs")
        .await
        .expect("touching an unknown target");
    assert!(nothing.hits.is_empty(), "{nothing}");
    assert!(
        nothing.target_ids.is_empty(),
        "an unresolvable target is reported as unresolvable, not as uncovered"
    );
}

#[tokio::test]
async fn collisions_finds_the_intended_pair_and_not_the_decoy() {
    let (db, _) = synced().await;
    let report = ops::collisions(&db, PROJECT).await.expect("collisions");

    assert_eq!(report.active_items, 3, "{report}");
    assert_eq!(report.collisions.len(), 1, "{report}");
    let c = &report.collisions[0];
    assert_eq!((c.a.item_id.as_str(), c.b.item_id.as_str()), ("OPS-1", "OPS-2"));
    assert_eq!(c.shared_nodes, vec!["src/main.rs".to_string()]);
    assert_eq!(c.shared_selectors.len(), 1, "{report}");
    assert!(
        c.shared_selectors[0].contains("src/main.rs"),
        "the message must name what is shared: {}",
        c.shared_selectors[0]
    );

    // OPS-3 is active and touches only beta.rs, so it collides with
    // nothing. OPS-5 touches main.rs but is `planned`, and OPS-6's sweep
    // glob covers every file but is `abandoned`; neither may appear.
    let named: String = format!("{report}");
    for quiet in ["OPS-3", "OPS-5", "OPS-6"] {
        assert!(
            !named.contains(quiet),
            "{quiet} must not be reported as colliding: {named}"
        );
    }
}

#[tokio::test]
async fn stale_names_the_item_its_touches_and_their_reasons() {
    let (db, _) = synced().await;
    let report = ops::stale(&db, PROJECT).await.expect("stale");

    assert_eq!(report.items.len(), 1, "{report}");
    let item = &report.items[0];
    assert_eq!(item.item.item_id, "OPS-1");
    assert_eq!(item.item.unresolved, 2);
    assert_eq!(
        item.item.unindexed, 1,
        "Cargo.toml is counted as bound but unindexed, not as stale"
    );
    assert_eq!(report.actionable, 2, "{report}");
    assert!(
        report.by_reason.iter().all(|r| r.actionable),
        "every reason this build produces is worth someone's time: {report}"
    );

    let named: Vec<(&str, Option<UnresolvedReason>)> = item
        .unresolved
        .iter()
        .map(|t| (t.touch.raw.as_str(), t.reason))
        .collect();
    // File order, not sorted order: the author reads the report next to the
    // file, so the report follows the file. Cargo.toml is absent because it
    // bound.
    assert_eq!(
        named,
        vec![
            ("no_such_function_anywhere", Some(UnresolvedReason::NoSuchSymbol)),
            ("src/does_not_exist.rs", Some(UnresolvedReason::NoSuchFile)),
        ],
        "{report}"
    );
    for t in &item.unresolved {
        let detail = t.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains(&t.touch.raw),
            "every reason must quote what was looked for: {detail:?}"
        );
    }
}

#[tokio::test]
async fn blast_returns_the_reverse_dependencies_the_manifest_documents() {
    let (db, _) = synced().await;
    let report = ops::blast(&db, PROJECT, "OPS-4", 1).await.expect("blast");

    assert_eq!(report.seeds.len(), 1, "one symbol touch, one seed: {report}");
    assert!(
        report.unbound_touches.is_empty(),
        "OPS-4's only touch binds: {report}"
    );

    let reached: BTreeSet<&str> = report.reached.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        reached,
        ["run", "call_connect_via_module_import"].into_iter().collect(),
        "expected.yaml's cases b and bonus-r2 are the only RESOLVED callers of \
         db::connection::connect: {report}"
    );
    assert!(report.reached.iter().all(|h| h.depth == 1), "{report}");
    assert_eq!(report.reached_files, 2, "{report}");

    // A file touch reaches through the same edges, because a file's reverse
    // dependencies are its symbols' reverse dependencies. OPS-2 touches
    // src/main.rs and, through its glob, src/db/connection.rs, so `connect`
    // is a seed by way of the file that defines it and `run` is a seed in
    // its own right. A seed is not part of its own blast radius, so the one
    // node left is the caller that lives outside the touched files: this is
    // the reach number an agent actually wants, the code it did not already
    // know it was editing.
    let via_file = ops::blast(&db, PROJECT, "OPS-2", 1).await.expect("blast OPS-2");
    let reached: Vec<&str> = via_file.reached.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(reached, vec!["call_connect_via_module_import"], "{via_file}");
    assert!(
        via_file.seeds.len() > 1,
        "the glob and the file touch both contribute seeds: {via_file}"
    );
}

#[tokio::test]
async fn list_and_show_read_back_what_sync_wrote() {
    let (db, _) = synced().await;

    let all = ops::list(&db, PROJECT, &ListFilter::default())
        .await
        .expect("list");
    assert_eq!(all.planes.len(), 2, "{all}");
    assert_eq!(all.planes[0].plane.plane_id, "core", "file order preserved");
    assert_eq!(all.planes[0].items.len(), 5);
    assert_eq!(all.planes[1].plane.plane_id, "parked");

    let active = ops::list(
        &db,
        PROJECT,
        &ListFilter {
            plane: Some("core".to_string()),
            status: Some(codegraph::plan::model::Status::Active),
            horizon: None,
        },
    )
    .await
    .expect("filtered list");
    let ids: Vec<&str> = active.planes[0]
        .items
        .iter()
        .map(|i| i.item_id.as_str())
        .collect();
    assert_eq!(ids, vec!["OPS-1", "OPS-2", "OPS-3"], "{active}");

    let show = ops::show(&db, PROJECT, "OPS-1").await.expect("show");
    assert_eq!(show.plane.plane_id, "core");
    assert_eq!(show.item.touch_count, 5);
    assert_eq!(show.item.resolved, 2);
    assert_eq!(show.item.ambiguous, 1);
    assert_eq!(show.item.unresolved, 2);
    assert_eq!(show.item.unindexed, 1);
    // The rendering marks it rather than leaving the reader to notice a
    // RESOLVED touch that reaches nothing.
    let rendered = format!("{show}");
    assert!(
        rendered.contains("file: Cargo.toml [RESOLVED] Cargo.toml (not indexed)"),
        "{rendered}"
    );
    assert_eq!(show.blocks, vec!["OPS-2".to_string()], "the inverse of depends_on");
    assert_eq!(show.spec.as_deref(), Some("specs/resolution-layer-v1.md"));
    assert_eq!(
        touch_of(&show, "src/main.rs").confidence,
        TouchConfidence::Resolved
    );
    assert_eq!(touch_of(&show, "helper").confidence, TouchConfidence::Ambiguous);
    assert_eq!(show.unbound.len(), 3, "one ambiguous plus two unresolved");
    let indexed_flags: Vec<(&str, Option<bool>)> = show
        .touches
        .iter()
        .map(|t| (t.touch.raw.as_str(), t.indexed))
        .collect();
    assert_eq!(
        indexed_flags,
        vec![
            ("src/main.rs", Some(true)),
            ("helper", None),
            ("no_such_function_anywhere", None),
            ("src/does_not_exist.rs", None),
            ("Cargo.toml", Some(false)),
        ],
        "a symbol touch has no indexed question to answer, and neither does one that did not bind"
    );

    let deps = ops::show(&db, PROJECT, "OPS-2").await.expect("show OPS-2");
    assert_eq!(deps.depends_on, vec!["OPS-1".to_string()]);

    let missing = ops::show(&db, PROJECT, "NOPE-1").await;
    let err = missing.expect_err("an unknown item id is an error, not an empty report");
    assert!(
        err.to_string().contains("OPS-1"),
        "the error must list ids that do exist: {err}"
    );
}

/// The feature's whole point, end to end: the plan does not change, the
/// code does, and the touch flips to UNRESOLVED.
#[tokio::test]
async fn renaming_a_touched_symbol_makes_the_plan_stale() {
    let project = "plan-rename";
    let planes = common::repo_path("tests/fixtures/planes/ops-rename.yaml");
    let scratch = tempfile::tempdir().expect("temp dir");
    let root = scratch.path().join("rust");
    copy_tree(&common::repo_path(FIXTURE), &root).expect("copy the fixture");

    let db = common::fresh_db().await.expect("fresh in-memory store");
    index_at(&db, project, &root).await.expect("first index");

    let before = ops::sync(&db, project, &planes).await.expect("first sync");
    assert_eq!(before.selectors_resolved, 2, "{before}");
    assert_eq!(before.selectors_unresolved, 0, "{before}");

    // Rename the function the plan names. Nothing else changes, and the
    // planes file is not touched at all.
    let main_rs = root.join("src/main.rs");
    let source = std::fs::read_to_string(&main_rs).expect("read main.rs");
    assert!(source.contains("fn log_startup()"), "fixture drifted");
    std::fs::write(
        &main_rs,
        source.replace("log_startup", "log_boot"),
    )
    .expect("write main.rs");

    index_at(&db, project, &root).await.expect("re-index");
    let after = ops::sync(&db, project, &planes).await.expect("second sync");

    assert_eq!(after.selectors_resolved, 1, "the file touch still binds: {after}");
    assert_eq!(after.selectors_unresolved, 1, "{after}");
    assert_eq!(after.unresolved_touches.len(), 1);
    let flipped = &after.unresolved_touches[0];
    assert_eq!(flipped.item_id, "RN-1");
    assert_eq!(flipped.touch.touch.raw, "log_startup");
    assert_eq!(flipped.touch.reason, Some(UnresolvedReason::NoSuchSymbol));

    let stale = ops::stale(&db, project).await.expect("stale after rename");
    assert_eq!(stale.items.len(), 1, "{stale}");
    assert_eq!(stale.items[0].item.item_id, "RN-1");
    assert_eq!(stale.items[0].unresolved[0].touch.raw, "log_startup");

    // And the new name is findable, so the reader can see where the symbol
    // went rather than only that it left.
    let moved = ops::touching(&db, project, "log_boot")
        .await
        .expect("touching the new name");
    assert!(
        moved.hits.is_empty() && !moved.target_ids.is_empty(),
        "log_boot exists in the index and nothing plans it yet: {moved}"
    );
}

/// A finished plan is not a stale plan.
///
/// `done` and `abandoned` items are excluded from `stale` for the reason
/// `collisions` excludes everything but `active`: the report is only useful
/// if every line in it is worth acting on. An item that landed, whose code
/// has since been renamed away, is a record of history. Asking the reader
/// to fix it is asking them to close something already closed.
#[tokio::test]
async fn stale_ignores_finished_and_abandoned_work() {
    let project = "plan-done";
    let db = common::fresh_db().await.expect("fresh in-memory store");
    common::index_fixture(&db, project, FIXTURE)
        .await
        .expect("indexing the rust fixture");

    let source = common::repo_path("tests/fixtures/planes/ops-done-item.yaml");
    ops::sync(&db, project, &source).await.expect("sync");

    // All three items carry an UNRESOLVED touch.
    let all = ops::list(&db, project, &ListFilter::default())
        .await
        .expect("list");
    let unresolved: Vec<(&str, usize)> = all.planes[0]
        .items
        .iter()
        .map(|i| (i.item_id.as_str(), i.unresolved))
        .collect();
    assert_eq!(
        unresolved,
        vec![("DONE-1", 1), ("GONE-1", 1), ("LIVE-1", 1)],
        "the fixture must give every item something that does not bind: {all}"
    );

    // Only the one still in play is reported.
    let report = ops::stale(&db, project).await.expect("stale");
    let named: Vec<&str> = report.items.iter().map(|i| i.item.item_id.as_str()).collect();
    assert_eq!(named, vec!["LIVE-1"], "{report}");
    assert_eq!(report.actionable, 1, "{report}");

    // The same item, in a state where the touch matters, is reported. This
    // is what proves the exclusion is about status and not about the touch.
    let scratch = tempfile::tempdir().expect("temp dir");
    let reopened = scratch.path().join("planes.yaml");
    let text = std::fs::read_to_string(&source).expect("read the fixture");
    std::fs::write(
        &reopened,
        text.replace("        status: done\n", "        status: planned\n"),
    )
    .expect("write the reopened roadmap");

    ops::sync(&db, project, &reopened).await.expect("re-sync");
    let report = ops::stale(&db, project).await.expect("stale after reopening");
    let named: Vec<&str> = report.items.iter().map(|i| i.item.item_id.as_str()).collect();
    assert_eq!(named, vec!["DONE-1", "LIVE-1"], "{report}");
    assert_eq!(report.actionable, 2, "{report}");
    assert_eq!(
        report.items[0].unresolved[0].touch.raw, "no_such_function_anywhere",
        "{report}"
    );
}

/// A file that exists binds. Whether the graph holds anything for it is a
/// separate bit, not a confidence level.
///
/// The distinction is the difference between a useful staleness report and
/// a useless one. On codegraph's own roadmap 23 of 61 touches name specs, a
/// README and a `.surql` schema: all present, all correct, none of them a
/// file type the indexer has a grammar for. Folding that into UNRESOLVED
/// made `plan stale` pure noise on the day it shipped.
#[tokio::test]
async fn a_real_but_unindexed_path_binds_and_is_never_stale() {
    let project = "plan-lag";
    let planes = common::repo_path("tests/fixtures/planes/ops-index-lag.yaml");
    let scratch = tempfile::tempdir().expect("temp dir");
    let root = scratch.path().join("rust");
    copy_tree(&common::repo_path(FIXTURE), &root).expect("copy the fixture");

    let db = common::fresh_db().await.expect("fresh in-memory store");
    index_at(&db, project, &root).await.expect("index");

    // Written after the index ran: on disk, indexable, absent from the
    // graph. The index is behind the tree, which is not the plan's fault.
    std::fs::write(root.join("src/late_arrival.rs"), "pub fn arrived() {}\n")
        .expect("write the late file");

    let report = ops::sync(&db, project, &planes).await.expect("sync");

    // Three of the four entries bind; only the one naming a file that is
    // not there does not.
    assert_eq!(report.selectors, 4, "{report}");
    assert_eq!(report.selectors_resolved, 3, "{report}");
    assert_eq!(report.selectors_unresolved, 1, "{report}");
    assert_eq!(report.unresolved_touches.len(), 1, "{report}");
    assert_eq!(report.unresolved_touches[0].touch.touch.raw, "src/gone.rs");
    assert_eq!(
        report.unresolved_touches[0].touch.reason,
        Some(UnresolvedReason::NoSuchFile)
    );
    assert!(
        report.working_tree_files > report.indexed_files,
        "the working tree holds more than the indexer has grammars for: {report}"
    );

    let show = ops::show(&db, project, "LAG-1").await.expect("show LAG-1");
    let bound: Vec<(&str, TouchConfidence, Option<bool>)> = show
        .touches
        .iter()
        .map(|t| (t.touch.raw.as_str(), t.touch.confidence, t.indexed))
        .collect();
    assert_eq!(
        bound,
        vec![
            // On disk, not yet indexed. Bound, and flagged.
            ("src/late_arrival.rs", TouchConfidence::Resolved, Some(false)),
            // Not there at all. The one genuinely stale touch.
            ("src/gone.rs", TouchConfidence::Unresolved, None),
            // On disk, no grammar for it, so never indexed. Still bound.
            ("Cargo.toml", TouchConfidence::Resolved, Some(false)),
            // The glob is matched against the working tree too, so it finds
            // a file the indexer would never have offered it.
            ("*.toml", TouchConfidence::Resolved, Some(false)),
        ],
        "{show}"
    );
    assert_eq!(show.item.unindexed, 3, "{show}");

    // Only the missing file is stale.
    let stale = ops::stale(&db, project).await.expect("stale");
    assert_eq!(stale.items.len(), 1, "{stale}");
    assert_eq!(stale.items[0].unresolved.len(), 1, "{stale}");
    assert_eq!(stale.items[0].unresolved[0].touch.raw, "src/gone.rs");
    assert_eq!(stale.actionable, 1, "{stale}");

    // An unindexed target is a real target for the inverse lookup, even
    // though the graph has no node for it.
    let covering = ops::touching(&db, project, "Cargo.toml")
        .await
        .expect("touching an unindexed path");
    assert_eq!(covering.target_ids, vec!["Cargo.toml".to_string()], "{covering}");
    assert!(
        covering.hits.iter().any(|h| h.item.item_id == "LAG-1"),
        "{covering}"
    );

    // And it is skipped as a blast seed, with the skip counted rather than
    // hidden, because a reach computed over fewer seeds is a lower bound.
    let blast = ops::blast(&db, project, "LAG-1", 2).await.expect("blast");
    assert_eq!(blast.skipped_unindexed, 3, "{blast}");
    assert_eq!(blast.seeds.len(), 0, "{blast}");
    assert!(
        format!("{blast}").contains("skipped as seeds"),
        "the rendering has to say so: {blast}"
    );

    // Re-indexing flips the flag without touching the confidence, which is
    // the proof that the plan was never the thing that was wrong.
    index_at(&db, project, &root).await.expect("re-index");
    let after = ops::sync(&db, project, &planes).await.expect("re-sync");
    assert_eq!(after.selectors_resolved, 3, "{after}");
    assert_eq!(after.selectors_unresolved, 1, "{after}");
    assert_eq!(after.unindexed, 2, "one fewer than before: {after}");
    let show = ops::show(&db, project, "LAG-1").await.expect("show after re-index");
    let late = show
        .touches
        .iter()
        .find(|t| t.touch.raw == "src/late_arrival.rs")
        .expect("the late file is still a touch");
    assert_eq!(late.touch.confidence, TouchConfidence::Resolved);
    assert_eq!(late.indexed, Some(true), "{show}");
}

/// A fresh full index of a project rooted anywhere on disk.
///
/// `common::index_fixture` is hard-wired to a path inside the repo; this
/// test needs a writable copy, because it renames a function mid-test.
async fn index_at(db: &Arc<Surreal<Any>>, project_id: &str, root: &Path) -> Result<()> {
    let result = index_project(
        db,
        &IndexConfig {
            project_id: project_id.to_string(),
            root_path: root.to_path_buf(),
            tier: IndexingTier::Balanced,
            languages: None,
            force: true,
        },
    )
    .await?;
    assert!(result.errors.is_empty(), "indexing errors: {:?}", result.errors);
    Ok(())
}

/// Recursive directory copy. `tempfile` is the only filesystem dev
/// dependency and the board forbids adding another crate for this.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target: PathBuf = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// A synthetic `module` node must never satisfy a `- symbol:` touch.
///
/// G2 (`specs/receipts/extractor-gaps-20260914.md`) gives every TypeScript
/// file whose top level runs code a node named after the file. That is a
/// container, not a definition an author wrote, and the plan resolver keys
/// its bare-name index on the name alone. Without the exclusion,
/// `- symbol: utils` binds silently to the module node for `utils.ts` and
/// an author who named a symbol that does not exist is told their plan is
/// fine. An author who means the file writes `- file: utils.ts`.
#[tokio::test]
async fn a_module_node_never_satisfies_a_symbol_touch() {
    let project = "plan-module-node";
    let planes = common::repo_path("tests/fixtures/planes/ops-module-node.yaml");
    let scratch = tempfile::tempdir().expect("temp dir");
    let root = scratch.path().join("ts");
    std::fs::create_dir_all(&root).expect("mkdir");
    // Top-level code, so `utils.ts` gets a module node named `utils`, and a
    // real definition in it with a different name so the file is not empty
    // of symbols either.
    std::fs::write(
        root.join("utils.ts"),
        "export function format(x: string): string {\n  return x.trim();\n}\n\nformat('a');\n",
    )
    .expect("write utils.ts");

    let db = common::fresh_db().await.expect("fresh in-memory store");
    index_at(&db, project, &root).await.expect("index");

    let report = ops::sync(&db, project, &planes).await.expect("sync");
    assert_eq!(
        report.selectors_unresolved, 1,
        "nothing defines `utils`, so the touch must not bind: {report}"
    );
    assert_eq!(report.selectors_resolved, 0, "{report}");
    assert_eq!(
        report.unresolved_touches[0].touch.reason,
        Some(UnresolvedReason::NoSuchSymbol),
        "and the reason must name the real problem: {report}"
    );
}

/// The converse, on the same planes file: a real definition of that name
/// still binds, and the module node does not make it AMBIGUOUS.
#[tokio::test]
async fn a_real_definition_still_binds_past_the_module_node() {
    let project = "plan-module-node";
    let planes = common::repo_path("tests/fixtures/planes/ops-module-node.yaml");
    let scratch = tempfile::tempdir().expect("temp dir");
    let root = scratch.path().join("ts");
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(
        root.join("utils.ts"),
        "export function format(x: string): string {\n  return x.trim();\n}\n\nformat('a');\n",
    )
    .expect("write utils.ts");
    // The name the plan means, defined for real, in another file.
    std::fs::write(
        root.join("helpers.ts"),
        "export function utils(): void {\n  return;\n}\n",
    )
    .expect("write helpers.ts");

    let db = common::fresh_db().await.expect("fresh in-memory store");
    index_at(&db, project, &root).await.expect("index");

    let report = ops::sync(&db, project, &planes).await.expect("sync");
    assert_eq!(
        report.selectors_resolved, 1,
        "one definition named `utils`, so exactly one candidate: {report}"
    );
    assert_eq!(report.selectors_unresolved, 0, "{report}");

    let show = ops::show(&db, project, "MN-1").await.expect("show");
    let touch = touch_of(&show, "utils");
    assert_eq!(touch.confidence, TouchConfidence::Resolved);
    let bound = touch.to_id.as_deref().expect("a bound node id");

    // It bound to the function, not to the file that shares the name.
    let nodes = common::load_all_nodes(&db, project)
        .await
        .expect("load nodes");
    let target = nodes
        .iter()
        .find(|n| n.node_id == bound)
        .expect("the bound node is in the store");
    assert_eq!(target.node_type, "function");
    assert_eq!(target.file_path, "helpers.ts");

    // And the module node really is there, so the test proves an exclusion
    // rather than an absence.
    assert!(
        nodes
            .iter()
            .any(|n| n.name == "utils" && n.node_type == "module"),
        "utils.ts must still carry its module node: {:?}",
        nodes
            .iter()
            .map(|n| (&n.name, &n.node_type))
            .collect::<Vec<_>>()
    );
}
