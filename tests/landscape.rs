//! Integration tests for `codegraph landscape`, `codegraph plan brief`, and
//! the `codegraph context` roadmap integration (lane L3).
//!
//! Everything here runs against a real index of `tests/fixtures/go`, chosen
//! because it is the only fixture whose files sit in more than one directory
//! *and* call across directory boundaries, which is the only way to test an
//! inter-subsystem dependency at all.
//!
//! The partition expectation below is hand-derived from the fixture's file
//! list and the documented rule, not read back from the implementation. The
//! inter-subsystem weights are checked against an oracle computed in this
//! file from the raw `code_node` / `code_edge` rows, so the assertion never
//! reduces to "the code agrees with itself".

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::landscape::{
    self, Landscape, LandscapeFormat, LandscapeOptions, Partition, PartitionRule, RoadmapSource,
    UnplacedCause, ROOT_SUBSYSTEM,
};
use codegraph::plan::export;

const PROJECT: &str = "landscape-go";
const FIXTURE: &str = "tests/fixtures/go";

/// Every `.go` file in `tests/fixtures/go`, which is exactly what the
/// indexer picks up there (`go.mod` has no grammar and is never indexed).
const FIXTURE_FILES: [&str; 9] = [
    "alpha/alpha.go",
    "beta/beta.go",
    "db/db.go",
    "evenodd/even.go",
    "evenodd/odd.go",
    "gamma.go",
    "handler/serve.go",
    "handler/types.go",
    "main.go",
];

/// The partition the documented rule produces for [`FIXTURE_FILES`], worked
/// out by hand:
///
/// 9 files, so the 25% share threshold is 2.25 files. Cutting at the top
/// level gives `.`=2 (gamma.go, main.go), `alpha`=1, `beta`=1, `db`=1,
/// `evenodd`=2, `handler`=2. Nothing exceeds 2.25, so nothing splits. The
/// three single-file subsystems sit at depth 1 and have no parent on the
/// map, so they stay rather than folding: claiming `alpha/alpha.go` lives at
/// the repo root would be false.
fn expected_partition() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::from([
        (ROOT_SUBSYSTEM, vec!["gamma.go", "main.go"]),
        ("alpha", vec!["alpha/alpha.go"]),
        ("beta", vec!["beta/beta.go"]),
        ("db", vec!["db/db.go"]),
        ("evenodd", vec!["evenodd/even.go", "evenodd/odd.go"]),
        ("handler", vec!["handler/serve.go", "handler/types.go"]),
    ])
}

fn planes_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/planes")
        .join(name)
}

/// A path that certainly does not exist, for the "no roadmap" cases.
fn absent_planes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/planes/does-not-exist.yaml")
}

async fn indexed() -> Arc<Surreal<Any>> {
    let db = common::fresh_db().await.expect("fresh in-memory store");
    let result = common::index_fixture(&db, PROJECT, FIXTURE)
        .await
        .expect("indexing the go fixture");
    assert!(
        result.errors.is_empty(),
        "indexing reported errors: {:?}",
        result.errors
    );
    db
}

async fn build(db: &Arc<Surreal<Any>>, planes: &Path) -> Landscape {
    landscape::build_with_planes(db, PROJECT, planes, PartitionRule::default())
        .await
        .expect("building the landscape")
}

// ============================================================================
// The partition
// ============================================================================

#[tokio::test]
async fn the_partition_matches_the_hand_written_expectation() {
    let db = indexed().await;
    let nodes = common::load_all_nodes(&db, PROJECT)
        .await
        .expect("loading nodes");
    let files: BTreeSet<String> = nodes.iter().map(|n| n.file_path.clone()).collect();
    assert_eq!(
        files.iter().map(String::as_str).collect::<Vec<_>>(),
        FIXTURE_FILES.to_vec(),
        "the fixture's indexed file list changed; the hand-derived partition below is stale"
    );

    let list: Vec<String> = files.into_iter().collect();
    let partition = Partition::build(&list, PartitionRule::default());

    let expected = expected_partition();
    assert_eq!(
        partition.names().map(String::as_str).collect::<Vec<_>>(),
        expected.keys().copied().collect::<Vec<_>>()
    );
    for (name, members) in &expected {
        assert_eq!(
            partition.members(name),
            members
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .as_slice(),
            "members of {name} differ"
        );
    }
}

#[tokio::test]
async fn every_fixture_file_lands_in_exactly_one_subsystem() {
    let db = indexed().await;
    let landscape = build(&db, &absent_planes()).await;

    let counted: usize = landscape.subsystems.iter().map(|s| s.file_count).sum();
    assert_eq!(counted, landscape.file_count);
    assert_eq!(landscape.file_count, FIXTURE_FILES.len());

    let list: Vec<String> = FIXTURE_FILES.iter().map(|f| f.to_string()).collect();
    let partition = Partition::build(&list, PartitionRule::default());
    for file in FIXTURE_FILES {
        let owners: Vec<&String> = partition
            .names()
            .filter(|name| partition.members(name).iter().any(|m| m == file))
            .collect();
        assert_eq!(owners.len(), 1, "{file} landed in {owners:?}");
        assert_eq!(partition.subsystem_of(file), Some(owners[0].as_str()));
    }
}

#[tokio::test]
async fn two_builds_of_one_index_are_byte_identical() {
    let db = indexed().await;
    let planes = planes_fixture("landscape-overlay.yaml");
    let first = build(&db, &planes).await;
    let second = build(&db, &planes).await;
    assert_eq!(first, second, "the landscape model is not deterministic");
    assert_eq!(export::markdown(&first), export::markdown(&second));
    assert_eq!(export::mermaid(&first), export::mermaid(&second));
    assert_eq!(export::dot(&first), export::dot(&second));
    assert_eq!(export::brief(&first), export::brief(&second));
}

// ============================================================================
// Inter-subsystem weights
// ============================================================================

/// Recompute the subsystem channel set straight from the stored rows, using
/// the hand-written partition above rather than [`Partition`]. This is the
/// oracle: if `landscape::build` drops an edge, double counts a file pair,
/// or maps a node to the wrong box, the two sets disagree.
async fn oracle_channels(db: &Arc<Surreal<Any>>) -> BTreeMap<(String, String), usize> {
    let nodes = common::load_all_nodes(db, PROJECT)
        .await
        .expect("loading nodes");
    let edges = common::load_all_edges(db, PROJECT)
        .await
        .expect("loading edges");

    let mut subsystem_of: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, members) in expected_partition() {
        for m in members {
            subsystem_of.insert(m, name);
        }
    }
    let file_of: BTreeMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.node_id.as_str(), n.file_path.as_str()))
        .collect();

    let name_edge_types = ["calls", "member_of", "implements"];
    let mut pairs: BTreeMap<(String, String), BTreeSet<(String, String)>> = BTreeMap::new();

    for e in &edges {
        let crosses = if e.edge_type == "file_ref" {
            None
        } else if name_edge_types.contains(&e.edge_type.as_str())
            && e.confidence == "RESOLVED"
            && !e.to_id.is_empty()
        {
            match (file_of.get(e.from_id.as_str()), file_of.get(e.to_id.as_str())) {
                (Some(from), Some(to)) if from != to => Some(((*from).to_string(), (*to).to_string())),
                _ => None,
            }
        } else {
            None
        };
        let Some((from_file, to_file)) = crosses else {
            continue;
        };
        let (Some(from_sub), Some(to_sub)) = (
            subsystem_of.get(from_file.as_str()),
            subsystem_of.get(to_file.as_str()),
        ) else {
            continue;
        };
        pairs
            .entry(((*from_sub).to_string(), (*to_sub).to_string()))
            .or_default()
            .insert((from_file, to_file));
    }

    // `file_ref` rows are the deduplicated file-pair view of the same
    // bindings, so they are unioned into the channel set rather than added
    // to it. Read straight from the store here, because the shared test
    // harness projects `from_id`/`to_id` and these rows are keyed on
    // `from_file`/`to_file`.
    let mut resp = db
        .query(
            "SELECT from_file, to_file FROM code_edge \
             WHERE project_id = $pid AND edge_type = 'file_ref'",
        )
        .bind(("pid", PROJECT.to_string()))
        .await
        .expect("file_ref query");
    let rows: Vec<surrealdb_types::Value> = resp.take(0).expect("file_ref rows");
    for row in &rows {
        let surrealdb_types::Value::Object(obj) = row else {
            continue;
        };
        let get = |key: &str| match obj.get(key) {
            Some(surrealdb_types::Value::String(s)) => Some(s.to_string()),
            _ => None,
        };
        let (Some(from_file), Some(to_file)) = (get("from_file"), get("to_file")) else {
            continue;
        };
        if from_file == to_file {
            continue;
        }
        let (Some(from_sub), Some(to_sub)) = (
            subsystem_of.get(from_file.as_str()),
            subsystem_of.get(to_file.as_str()),
        ) else {
            continue;
        };
        pairs
            .entry(((*from_sub).to_string(), (*to_sub).to_string()))
            .or_default()
            .insert((from_file, to_file));
    }

    pairs
        .into_iter()
        .map(|(key, set)| (key, set.len()))
        .collect()
}

#[tokio::test]
async fn inter_subsystem_channels_match_an_independent_count() {
    let db = indexed().await;
    let landscape = build(&db, &absent_planes()).await;

    let mut actual: BTreeMap<(String, String), usize> = BTreeMap::new();
    for e in &landscape.edges {
        actual.insert((e.from.clone(), e.to.clone()), e.channels);
    }
    for s in &landscape.subsystems {
        if s.internal_channels > 0 {
            actual.insert((s.name.clone(), s.name.clone()), s.internal_channels);
        }
    }

    assert_eq!(
        actual,
        oracle_channels(&db).await,
        "landscape channels disagree with a count taken straight from the rows"
    );
}

#[tokio::test]
async fn the_fixtures_documented_cross_package_call_is_on_the_map() {
    // `expected.yaml` case b: main.go's Run calls db.Connect, RESOLVED by
    // r1. main.go is in the root subsystem, db/db.go is in `db`, so exactly
    // one channel runs between them and `calls` is one of its sources.
    let db = indexed().await;
    let landscape = build(&db, &absent_planes()).await;

    let edge = landscape
        .edges
        .iter()
        .find(|e| e.from == ROOT_SUBSYSTEM && e.to == "db")
        .unwrap_or_else(|| panic!("no root to db edge in {:?}", landscape.edges));
    assert_eq!(edge.channels, 1, "only main.go depends on db/db.go");
    assert!(edge.references >= 1, "{edge:?}");
    assert!(edge.via.contains(&"calls".to_string()), "{edge:?}");
    assert!(edge.via.contains(&"file_ref".to_string()), "{edge:?}");
}

#[tokio::test]
async fn a_cycle_inside_one_subsystem_is_not_a_subsystem_cycle() {
    // `expected.yaml` declares one file cycle, evenodd/even.go against
    // evenodd/odd.go. Both files are in `evenodd`, so it is internal
    // cohesion, not a cycle between subsystems.
    let db = indexed().await;
    let landscape = build(&db, &absent_planes()).await;

    let evenodd = landscape
        .subsystems
        .iter()
        .find(|s| s.name == "evenodd")
        .expect("evenodd subsystem");
    assert!(
        evenodd.internal_channels >= 2,
        "the mutual recursion should show as internal channels: {evenodd:?}"
    );
    assert!(
        !landscape
            .cycles
            .iter()
            .any(|c| c.a == "evenodd" || c.b == "evenodd"),
        "an internal cycle was reported as a subsystem cycle: {:?}",
        landscape.cycles
    );
}

#[tokio::test]
async fn a_cross_file_member_edge_counts_as_an_internal_channel() {
    // `expected.yaml` case f: handler/serve.go's Serve is a member_of
    // handler/types.go's Handler, cross-file inside one package.
    let db = indexed().await;
    let landscape = build(&db, &absent_planes()).await;
    let handler = landscape
        .subsystems
        .iter()
        .find(|s| s.name == "handler")
        .expect("handler subsystem");
    assert!(handler.internal_channels >= 1, "{handler:?}");
}

// ============================================================================
// Formats
// ============================================================================

#[tokio::test]
async fn mermaid_declares_every_subsystem_and_never_repeats_an_edge() {
    let db = indexed().await;
    let landscape = build(&db, &planes_fixture("landscape-overlay.yaml")).await;
    let text = export::mermaid(&landscape);

    assert!(text.contains("flowchart LR"), "{text}");
    for s in &landscape.subsystems {
        assert!(
            text.contains(&format!("{} files, {} nodes", s.file_count, s.node_count)),
            "subsystem {} missing from the mermaid output:\n{text}",
            s.name
        );
    }

    let edge_lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("-->"))
        .collect();
    assert_eq!(
        edge_lines.len(),
        landscape.edges.len().min(edge_lines.len()),
        "more edge lines than edges"
    );
    let mut unique = edge_lines.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        edge_lines.len(),
        unique.len(),
        "duplicate mermaid edges: {edge_lines:?}"
    );

    // The root to db channel from case b has to be drawn.
    let root_index = landscape
        .subsystems
        .iter()
        .position(|s| s.name == ROOT_SUBSYSTEM)
        .expect("root subsystem");
    let db_index = landscape
        .subsystems
        .iter()
        .position(|s| s.name == "db")
        .expect("db subsystem");
    assert!(
        text.contains(&format!("s{root_index} -->|1| s{db_index}")),
        "{text}"
    );
}

#[tokio::test]
async fn json_round_trips_through_the_typed_model() {
    let db = indexed().await;
    let landscape = build(&db, &planes_fixture("landscape-overlay.yaml")).await;
    let text = export::json(&landscape).expect("serializing");
    let back: Landscape = serde_json::from_str(&text).expect("deserializing");
    assert_eq!(back, landscape);
}

#[tokio::test]
async fn dot_is_a_closed_digraph_with_the_same_edges() {
    let db = indexed().await;
    let landscape = build(&db, &absent_planes()).await;
    let text = export::dot(&landscape);
    assert!(text.contains("digraph landscape {"), "{text}");
    assert!(text.trim_end().ends_with('}'), "{text}");
    let arrows = text.lines().filter(|l| l.contains(" -> ")).count();
    assert_eq!(arrows, landscape.edges.len().min(arrows));
    assert!(arrows > 0, "the go fixture has cross-package calls: {text}");
}

#[tokio::test]
async fn run_writes_the_file_it_says_it_wrote() {
    let db = indexed().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let out = dir.path().join("nested/landscape.md");
    let options = LandscapeOptions {
        project_id: PROJECT.to_string(),
        format: LandscapeFormat::Text,
        output: Some(out.clone()),
    };
    let result = landscape::run(&db, &options).await.expect("landscape run");
    assert_eq!(result.written_to.as_deref(), Some(out.as_path()));
    let on_disk = std::fs::read_to_string(&out).expect("reading the written file");
    assert_eq!(on_disk, result.rendered);
    assert!(on_disk.contains("# Landscape: landscape-go"), "{on_disk}");
    assert!(on_disk.contains("## How the boxes were drawn"), "{on_disk}");
}

#[tokio::test]
async fn every_format_renders_and_carries_no_em_dash() {
    let db = indexed().await;
    for format in LandscapeFormat::ALL {
        let options = LandscapeOptions {
            project_id: PROJECT.to_string(),
            format,
            output: None,
        };
        let out = landscape::run(&db, &options)
            .await
            .unwrap_or_else(|e| panic!("{format} rendering failed: {e:?}"));
        assert!(!out.rendered.is_empty(), "{format} rendered nothing");
        assert!(out.written_to.is_none());
        assert!(
            !out.rendered.contains('\u{2014}'),
            "{format} output contains an em dash"
        );
    }
}

// ============================================================================
// The roadmap overlay
// ============================================================================

#[tokio::test]
async fn the_overlay_places_file_symbol_and_glob_touches() {
    let db = indexed().await;
    let landscape = build(&db, &planes_fixture("landscape-overlay.yaml")).await;
    assert_eq!(landscape.roadmap.source, RoadmapSource::Loaded);

    let (_, ls1) = landscape
        .roadmap
        .items()
        .find(|(_, i)| i.id == "LS-1")
        .expect("LS-1");
    assert_eq!(ls1.subsystems, vec!["db".to_string()]);
    let file_touch = ls1
        .touches
        .iter()
        .find(|t| t.raw == "db/db.go")
        .expect("file touch");
    assert_eq!(file_touch.paths, vec!["db/db.go".to_string()]);
    let symbol_touch = ls1
        .touches
        .iter()
        .find(|t| t.raw == "Connect")
        .expect("symbol touch");
    assert_eq!(
        symbol_touch.paths,
        vec!["db/db.go".to_string()],
        "the symbol lookup should find only db::Connect"
    );

    let (_, ls2) = landscape
        .roadmap
        .items()
        .find(|(_, i)| i.id == "LS-2")
        .expect("LS-2");
    let glob = ls2.touches.first().expect("glob touch");
    assert_eq!(
        glob.paths,
        vec!["handler/serve.go".to_string(), "handler/types.go".to_string()],
        "the glob should expand to every indexed file under handler"
    );
    assert_eq!(ls2.subsystems, vec!["handler".to_string()]);

    let handler = landscape
        .subsystems
        .iter()
        .find(|s| s.name == "handler")
        .expect("handler subsystem");
    assert_eq!(handler.items, vec!["LS-2".to_string()]);
}

#[tokio::test]
async fn a_touch_that_names_nothing_is_reported_not_dropped() {
    let db = indexed().await;
    let landscape = build(&db, &planes_fixture("landscape-overlay.yaml")).await;

    let raws: Vec<&str> = landscape
        .roadmap
        .unplaced
        .iter()
        .map(|u| u.raw.as_str())
        .collect();
    assert!(
        raws.contains(&"docs/nowhere/absent.md"),
        "unplaced list was {raws:?}"
    );
    assert!(
        raws.contains(&"NoSuchSymbolExistsAnywhere"),
        "unplaced list was {raws:?}"
    );
    for u in &landscape.roadmap.unplaced {
        assert_eq!(u.item, "LS-5");
        // Both of LS-5's touches are stale rather than merely unindexed:
        // the path is not on disk and the symbol has no definition.
        assert!(u.cause.is_stale(), "{u:?}");
    }
    let causes: Vec<UnplacedCause> = landscape
        .roadmap
        .unplaced
        .iter()
        .map(|u| u.cause)
        .collect();
    assert!(causes.contains(&UnplacedCause::NotOnDisk), "{causes:?}");
    assert!(causes.contains(&UnplacedCause::NoDefinition), "{causes:?}");
}

#[tokio::test]
async fn the_brief_lists_live_items_and_omits_finished_and_dropped_ones() {
    // `landscape::brief` reads `.codegraph/planes.yaml` under the process
    // cwd, which every test in this binary shares. Going through the
    // builder with an explicit fixture path tests the same rendering
    // without a global-state race; `run_writes_the_file_it_says_it_wrote`
    // covers the entry point itself.
    let db = indexed().await;
    let built = build(&db, &planes_fixture("landscape-overlay.yaml")).await;
    let brief = export::brief(&built);

    assert!(brief.contains("LS-1 [active]"), "{brief}");
    assert!(brief.contains("LS-2 [planned]"), "{brief}");
    assert!(!brief.contains("LS-3"), "a done item leaked in:\n{brief}");
    assert!(
        !brief.contains("LS-4"),
        "an abandoned item leaked in:\n{brief}"
    );
    assert!(
        brief.contains("touches: db/db.go"),
        "the touches must be listed as literal paths:\n{brief}"
    );
    assert!(
        brief.contains("db/db.go: LS-1"),
        "the in-flight index must be keyed by path:\n{brief}"
    );
    assert!(
        brief.lines().count() < 120,
        "the brief is {} lines, which is too long to paste:\n{brief}",
        brief.lines().count()
    );
    for line in brief.lines() {
        assert!(
            !line.starts_with("#### "),
            "the brief must stop at level three headings: {line}"
        );
    }
}

#[tokio::test]
async fn a_roadmap_that_does_not_parse_never_fails_the_landscape() {
    let db = indexed().await;
    let landscape = build(&db, &planes_fixture("dependency-cycle.yaml")).await;
    match &landscape.roadmap.source {
        RoadmapSource::Invalid { reason } => {
            assert!(reason.contains("dependency-cycle"), "{reason}");
        }
        other => panic!("expected an invalid roadmap, got {other:?}"),
    }
    assert!(
        !landscape.subsystems.is_empty(),
        "the code half must still render"
    );
    assert!(export::markdown(&landscape).contains("did not load"));
}

// ============================================================================
// The context integration
// ============================================================================

#[tokio::test]
async fn context_carries_the_landscape_and_the_roadmap() {
    let db = indexed().await;
    let md = landscape::context_markdown(&db, PROJECT, &planes_fixture("landscape-overlay.yaml"))
        .await
        .expect("rendering the context file");

    assert!(md.contains("\n## Landscape\n"), "{md}");
    assert!(md.contains("\n## Roadmap\n"), "{md}");
    assert!(md.contains("\n## Hub Nodes (top 20)\n"), "{md}");
    assert!(
        md.contains("### Landscape: landscape-go"),
        "the landscape's own title should be demoted under the Landscape heading:\n{md}"
    );
    assert!(
        md.contains("### codegraph landscape: landscape-go"),
        "the brief should sit under the Roadmap heading unchanged:\n{md}"
    );
    for line in md.lines() {
        assert!(
            !line.starts_with("###### "),
            "the embedded documents pushed a heading past level five: {line}"
        );
    }
    assert!(md.contains("LS-1 [active]"), "{md}");
    assert!(!md.contains('\u{2014}'), "em dash in the context file");
}

/// The writer itself, not just the document it writes.
///
/// `src/context.rs` became a library module when `src/main.rs` stopped
/// re-declaring the module tree, so this path is reachable from a test for
/// the first time. It covers what `context_markdown` alone cannot: that the
/// bytes reach the named file, that a missing parent directory is created
/// rather than erroring, and that the file on disk is the whole document.
#[tokio::test]
async fn generate_context_writes_the_whole_document_to_disk() {
    let db = indexed().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let out = dir.path().join("nested/deeper/context.md");
    let planes = planes_fixture("landscape-overlay.yaml");

    codegraph::context::generate_context_with_planes(&db, PROJECT, &out, &planes)
        .await
        .expect("writing the context file");

    let on_disk = std::fs::read_to_string(&out).expect("reading it back");
    let expected = landscape::context_markdown(&db, PROJECT, &planes)
        .await
        .expect("rendering the same document");
    assert_eq!(
        on_disk, expected,
        "the writer must put the whole document on disk, unmodified"
    );
    assert!(on_disk.contains("\n## Landscape\n"), "{on_disk}");
    assert!(on_disk.contains("\n## Roadmap\n"), "{on_disk}");
    assert!(on_disk.contains("LS-1 [active]"), "{on_disk}");
}

#[tokio::test]
async fn context_without_a_planes_file_still_succeeds() {
    let db = indexed().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let out = dir.path().join("context.md");

    codegraph::context::generate_context_with_planes(&db, PROJECT, &out, &absent_planes())
        .await
        .expect("codegraph context must not fail when there is no roadmap");

    let md = std::fs::read_to_string(&out).expect("reading the context file");
    assert!(md.contains("\n## Roadmap\n"), "{md}");
    assert!(md.contains(export::NO_PLANES_NOTICE), "{md}");
    assert!(md.contains("\n## Landscape\n"), "{md}");
    assert_eq!(
        md.matches(export::NO_PLANES_NOTICE).count(),
        2,
        "the notice belongs in both the landscape overlay and the roadmap section:\n{md}"
    );
}
