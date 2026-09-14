//! Lane L1: what the first run on an unfamiliar repository does.
//!
//! Every test here is about the experience of pointing codegraph at a
//! codebase for the first time, so most of them drive the real binary
//! (`env!("CARGO_BIN_EXE_codegraph")`) rather than the library. The gaps
//! being closed were all gaps in what the *command* does: an exit code, a
//! message, a file that should not have been indexed, eight minutes of
//! silence. A library-level test cannot fail on any of those.
//!
//! Deliberately self-contained: it does not pull in `tests/common/mod.rs`,
//! because nothing here wants an in-memory store or the fixture harness.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary under test, built by `cargo test`.
const BIN: &str = env!("CARGO_BIN_EXE_codegraph");

/// A path inside the crate, independent of the cwd `cargo test` chose.
fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Run the binary from inside `cwd`, with progress forced to the
/// line-per-update style so stderr is deterministic and free of carriage
/// returns whether or not the test harness has a terminal attached.
fn run_in(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(cwd)
        .env("CODEGRAPH_PROGRESS", "plain")
        // The environment must not leak into a test about defaults.
        .env_remove("CODEGRAPH_PROJECT_ID")
        .env_remove("SURREALDB_URL")
        .args(args)
        .output()
        .expect("the codegraph binary must run")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A throwaway git repository containing `files`, each `(relative path,
/// contents)`.
///
/// A real `git init` rather than a committed fixture: a nested `.git`
/// directory cannot live inside this repository's own working tree, and the
/// behavior under test is git's, so a simulated one would prove nothing.
fn temp_git_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write_files(dir.path(), files);
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .expect("git must run");
        assert!(out.status.success(), "git {args:?} failed: {}", stderr(&out));
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "test"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "fixture"]);
    dir
}

/// A throwaway directory containing `files` and no repository at all.
fn temp_plain_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write_files(dir.path(), files);
    dir
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, contents) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
}

/// A store url pointing inside `dir`, so tests never share a store and
/// never touch the repository's own.
fn store_in(dir: &Path) -> String {
    format!("surrealkv://{}/graph.db", dir.display())
}

/// The two-file repository most tests here index: one file that must be
/// indexed, one under a gitignored directory that must not be.
fn ignored_generated_repo() -> tempfile::TempDir {
    temp_git_repo(&[
        (".gitignore", "generated/\n"),
        (
            "src/keep.rs",
            "pub fn kept_function() -> usize { 1 }\n",
        ),
        (
            "generated/bindings.rs",
            "pub fn generated_function() -> usize { 2 }\n",
        ),
    ])
}

// ============================================================================
// D2: ignore rules
// ============================================================================

#[test]
fn a_gitignored_directory_is_not_indexed() {
    let repo = ignored_generated_repo();
    let out = run_in(
        repo.path(),
        &["index", "--project-id", "d2", "--db-url", &store_in(repo.path())],
    );
    assert!(out.status.success(), "index failed: {}", stderr(&out));

    let text = stdout(&out);
    assert!(
        text.contains("1 scanned"),
        "only src/keep.rs should have been scanned, got:\n{text}"
    );
    assert!(
        text.contains("git ls-files"),
        "the run should say it used git discovery, got:\n{text}"
    );

    // The claim that matters is not the count, it is that the symbol
    // defined under `generated/` is absent from the graph.
    let found = run_in(
        repo.path(),
        &[
            "query",
            "--kind",
            "search",
            "--name",
            "generated_function",
            "--project-id",
            "d2",
            "--db-url",
            &store_in(repo.path()),
        ],
    );
    assert!(
        stdout(&found).contains("(0 results)"),
        "a symbol under a gitignored directory must not be in the graph, got:\n{}",
        stdout(&found)
    );

    // And the file that is not ignored is there, so the test cannot pass by
    // indexing nothing at all.
    let kept = run_in(
        repo.path(),
        &[
            "query",
            "--kind",
            "search",
            "--name",
            "kept_function",
            "--project-id",
            "d2",
            "--db-url",
            &store_in(repo.path()),
        ],
    );
    assert!(
        stdout(&kept).contains("kept_function"),
        "the tracked file must still be indexed, got:\n{}",
        stdout(&kept)
    );
}

#[test]
fn a_directory_with_no_repository_still_indexes_through_the_walk() {
    let dir = temp_plain_dir(&[
        ("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n"),
        ("target/junk.rs", "pub fn build_artifact() -> usize { 2 }\n"),
    ]);
    let out = run_in(
        dir.path(),
        &["index", "--project-id", "d2b", "--db-url", &store_in(dir.path())],
    );
    assert!(out.status.success(), "index failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("directory walk"),
        "a plain directory must fall back to the walk, got:\n{text}"
    );
    // The built-in skip list still holds on the fallback path.
    assert!(
        text.contains("1 scanned"),
        "target/ must stay skipped, got:\n{text}"
    );
}

#[test]
fn an_untracked_file_is_indexed_without_being_committed_first() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    // Written after the commit, so git calls it "other", not "cached".
    std::fs::write(
        repo.path().join("src/fresh.rs"),
        "pub fn fresh_function() -> usize { 3 }\n",
    )
    .expect("write");

    let out = run_in(
        repo.path(),
        &["index", "--project-id", "d2c", "--db-url", &store_in(repo.path())],
    );
    assert!(out.status.success(), "index failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("2 scanned"),
        "an uncommitted file still belongs to the project, got:\n{}",
        stdout(&out)
    );
}

// ============================================================================
// D1 and D3: progress and the tier notice
// ============================================================================

#[test]
fn indexing_reports_its_phases_the_tier_and_a_total() {
    let repo = ignored_generated_repo();
    let out = run_in(
        repo.path(),
        &["index", "--project-id", "d1", "--db-url", &store_in(repo.path())],
    );
    assert!(out.status.success(), "index failed: {}", stderr(&out));
    let err = stderr(&out);

    // D3: the tier, and that lighter ones exist.
    assert!(
        err.contains("[codegraph] tier full"),
        "the run must name its tier, got:\n{err}"
    );
    assert!(
        err.contains("--tier fast") && err.contains("--tier balanced"),
        "the run must say lighter tiers exist, got:\n{err}"
    );

    // D1: a line per phase, and a total with the split.
    for phase in [
        "discovery",
        "change detection",
        "parse+store",
        "resolve",
        "fingerprint",
        "registry",
    ] {
        assert!(
            err.contains(&format!("[codegraph] {phase} starting at ")),
            "no phase line for {phase}, got:\n{err}"
        );
    }
    assert!(
        err.contains("[codegraph] done in "),
        "no total line, got:\n{err}"
    );
    assert!(
        err.contains("parse+store ") && err.contains("resolve "),
        "the total must carry the per-phase split, got:\n{err}"
    );

    // Not a carriage-return repaint: the non-terminal style appends lines.
    assert!(
        !err.contains('\r'),
        "the plain style must not rewrite lines in place, got:\n{err:?}"
    );
}

#[test]
fn the_progress_cadence_is_lower_when_stderr_is_not_a_terminal() {
    use codegraph::index::{progress_tick_due, ProgressStyle};
    use std::time::Duration;

    let instant = Duration::from_millis(1);
    let two_seconds = Duration::from_secs(2);
    let ten_seconds = Duration::from_secs(10);

    // In place: every 25 files, but never more than ten times a second.
    assert!(!progress_tick_due(ProgressStyle::InPlace, 25, instant));
    assert!(progress_tick_due(
        ProgressStyle::InPlace,
        25,
        Duration::from_millis(120)
    ));
    // ... or every two seconds, however few files went by.
    assert!(progress_tick_due(ProgressStyle::InPlace, 0, two_seconds));

    // Plain: the same rule, an order of magnitude slower.
    assert!(!progress_tick_due(ProgressStyle::Plain, 25, two_seconds));
    assert!(progress_tick_due(ProgressStyle::Plain, 500, instant));
    assert!(progress_tick_due(ProgressStyle::Plain, 0, ten_seconds));

    // Off writes nothing, whatever happened.
    assert!(!progress_tick_due(ProgressStyle::Off, 100_000, ten_seconds));
}

// ============================================================================
// D4: init and doctor
// ============================================================================

#[test]
fn init_scaffolds_a_repository_and_says_what_to_run_next() {
    let repo = temp_git_repo(&[
        (".gitignore", "/target\n.codegraph/\n"),
        ("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n"),
    ]);

    let out = run_in(repo.path(), &["init"]);
    assert!(out.status.success(), "init failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("config.toml"), "got:\n{text}");
    assert!(text.contains("planes.yaml"), "got:\n{text}");
    assert!(text.contains("Next: codegraph index"), "got:\n{text}");

    assert!(repo.path().join(".codegraph/config.toml").exists());
    assert!(repo.path().join(".codegraph/planes.yaml").exists());

    // The whole-directory ignore is what stops the two files being
    // committable, so init has to have replaced it.
    let ignore = std::fs::read_to_string(repo.path().join(".gitignore")).expect("read");
    assert_eq!(
        ignore, "/target\n.codegraph/*\n!.codegraph/planes.yaml\n!.codegraph/config.toml\n",
        "the whole-directory rule must be rewritten so the negations can match"
    );

    // Proven against git itself, not against the text of the file: a
    // whole-directory ignore silently defeats the negations, and only git
    // can say whether this one does.
    let check = Command::new("git")
        .current_dir(repo.path())
        .args(["check-ignore", "-q", ".codegraph/planes.yaml"])
        .status()
        .expect("git must run");
    assert!(
        !check.success(),
        "git still ignores .codegraph/planes.yaml after init"
    );

    // Idempotent.
    let again = run_in(repo.path(), &["init"]);
    assert!(again.status.success(), "second init failed: {}", stderr(&again));
    assert!(
        stdout(&again).contains("already set up"),
        "a second init must say it did nothing, got:\n{}",
        stdout(&again)
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join(".gitignore")).expect("read"),
        ignore,
        "a second init must not touch .gitignore again"
    );

    // And the file it wrote validates. Checked through `doctor`, which runs
    // `plan::schema::validate` itself, rather than through `plan lint`,
    // which belongs to a lane that has not landed.
    let doctor = run_in(repo.path(), &["doctor", "--db-url", &store_in(repo.path())]);
    assert!(
        stdout(&doctor).contains("planes-file: ") && stdout(&doctor).contains("is valid:"),
        "the scaffolded planes file must validate:\n{}",
        stdout(&doctor)
    );
}

#[test]
fn the_scaffolded_planes_file_resolves_on_the_repo_it_describes() {
    // Sources under lib/, not src/. A scaffold that hardcoded "src/**"
    // produced an UNRESOLVED touch on the user's very first plan sync, which
    // reads as a broken tool rather than as a placeholder to replace.
    let repo = temp_git_repo(&[
        ("README.md", "# fixture\n"),
        ("lib/keep.rs", "pub fn kept_function() -> usize { 1 }\n"),
        ("lib/deep/more.rs", "pub fn more_function() -> usize { 2 }\n"),
    ]);

    let init = run_in(repo.path(), &["init"]);
    assert!(init.status.success(), "init failed: {}", stderr(&init));

    let scaffold =
        std::fs::read_to_string(repo.path().join(".codegraph/planes.yaml")).expect("read");
    assert!(scaffold.contains("- file: README.md"), "got:\n{scaffold}");
    assert!(scaffold.contains("- glob: \"lib/**\""), "got:\n{scaffold}");

    // An in-memory store: `plan sync` resolves file and glob touches against
    // the working tree, so it needs no index and no store that outlives the
    // process.
    let out = run_in(
        repo.path(),
        &["plan", "sync", "--db-url", "mem://", "--json"],
    );
    assert!(out.status.success(), "plan sync failed: {}", stderr(&out));

    let report: serde_json::Value =
        serde_json::from_str(stdout(&out).trim()).expect("plan sync --json must emit JSON");
    assert_eq!(
        report["unresolved"], 0,
        "the scaffolded file must resolve clean on its own repo, got:\n{report}"
    );
    assert_eq!(report["ambiguous"], 0, "got:\n{report}");
    assert!(
        report["resolved"].as_u64().expect("resolved") >= 3,
        "the README plus both files under lib/ must bind, got:\n{report}"
    );
}

#[test]
fn doctor_reports_every_check_and_is_honest_about_an_unindexed_project() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    let db = store_in(repo.path());

    let before = run_in(repo.path(), &["doctor", "--db-url", &db]);
    let text = stdout(&before);
    for name in [
        "version",
        "repo-root",
        "project-id",
        "config-file",
        "planes-file",
        "store-url",
        "store-open",
        "schema",
        "project-indexed",
        "languages",
        "next",
    ] {
        assert!(text.contains(name), "doctor omitted the {name} check:\n{text}");
    }
    // A store that has never been indexed has no schema, so doctor says
    // that rather than claiming a node count it never managed to read.
    assert!(
        text.contains("carries no codegraph schema yet"),
        "doctor must say the store has no schema yet:\n{text}"
    );
    assert!(
        text.contains("cannot have been indexed into a store with no schema"),
        "doctor must say the project is not indexed yet:\n{text}"
    );
    assert!(
        text.contains("rust 1"),
        "doctor must report the languages it found:\n{text}"
    );
    // Warnings are healthy: an unindexed repo is a normal state, not a fault.
    assert!(before.status.success(), "doctor exited non-zero:\n{text}");

    let indexed = run_in(
        repo.path(),
        &["index", "--db-url", &db],
    );
    assert!(indexed.status.success(), "index failed: {}", stderr(&indexed));

    let after = run_in(repo.path(), &["doctor", "--db-url", &db, "--json"]);
    let report: serde_json::Value =
        serde_json::from_str(stdout(&after).trim()).expect("doctor --json must emit JSON");
    let checks = report["checks"].as_array().expect("checks array");
    let indexed_check = checks
        .iter()
        .find(|c| c["name"] == "project-indexed")
        .expect("project-indexed check");
    assert_eq!(indexed_check["status"], "ok", "got {indexed_check}");
    assert!(
        indexed_check["detail"]
            .as_str()
            .expect("detail")
            .contains("nodes"),
        "got {indexed_check}"
    );
}

#[test]
fn doctor_exits_non_zero_when_a_check_fails() {
    // A planes file that parses but breaks a schema rule: an item with an
    // empty touch set is a hyperedge with no incidence.
    let repo = temp_git_repo(&[
        ("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n"),
        (
            ".codegraph/planes.yaml",
            "version: 1\nplanes:\n  - id: p\n    title: P\n    status: active\n    horizon: now\n    items:\n      - id: I-1\n        title: No touches\n        kind: feature\n        status: planned\n        touches: []\n",
        ),
    ]);
    let out = run_in(
        repo.path(),
        &["doctor", "--db-url", &store_in(repo.path())],
    );
    assert!(
        !out.status.success(),
        "a failing check must set a non-zero exit code:\n{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("schema violation"),
        "doctor must name the problem:\n{}",
        stdout(&out)
    );
}

// ============================================================================
// D5: --name is required where it means something
// ============================================================================

#[test]
fn a_name_taking_query_without_a_name_is_an_error_not_an_empty_answer() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    let db = store_in(repo.path());

    for kind in ["calls", "deps", "rdeps", "search"] {
        let out = run_in(
            repo.path(),
            &["query", "--kind", kind, "--project-id", "d5", "--db-url", &db],
        );
        assert!(
            !out.status.success(),
            "--kind {kind} with no --name must not exit 0, got:\n{}",
            stdout(&out)
        );
        let err = stderr(&out);
        assert!(
            err.contains(&format!("--kind {kind} needs a symbol"))
                && err.contains(&format!("--kind {kind} --name <symbol>")),
            "--kind {kind} must name the flag to pass, got:\n{err}"
        );
        assert!(
            !stdout(&out).contains("No function named ''"),
            "--kind {kind} must not answer a question about the codebase, got:\n{}",
            stdout(&out)
        );
    }
}

#[test]
fn query_kinds_that_take_no_name_are_unchanged() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    let db = store_in(repo.path());
    assert!(run_in(repo.path(), &["index", "--db-url", &db]).status.success());

    for kind in ["summary", "hubs", "coupling", "circular"] {
        let out = run_in(
            repo.path(),
            &["query", "--kind", kind, "--db-url", &db],
        );
        assert!(
            out.status.success(),
            "--kind {kind} must still work without --name:\n{}",
            stderr(&out)
        );
    }
}

// ============================================================================
// D6: the store lock message
// ============================================================================

#[tokio::test]
async fn a_store_held_by_another_process_says_so_in_product_terms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = store_in(dir.path());

    let _held = codegraph::db::connect(Some(&url))
        .await
        .expect("the first open must succeed");

    let second = codegraph::db::connect(Some(&url)).await;
    let err = match second {
        Ok(_) => panic!("a second writer must not be able to open the same embedded store"),
        Err(e) => format!("{e:#}"),
    };

    assert!(
        err.contains("open in another process"),
        "the message must say what happened, got:\n{err}"
    );
    assert!(
        err.contains(&url),
        "the message must name the store, got:\n{err}"
    );
    assert!(
        err.contains("codegraph serve") && err.contains("--db-url"),
        "the message must give both ways out, got:\n{err}"
    );
    // And the driver's own sentence is still detected, so the diagnosis
    // cannot silently stop applying.
    assert!(codegraph::db::is_store_locked(&err), "got:\n{err}");
}

// ============================================================================
// Opening an embedded store is quiet
// ============================================================================

#[test]
fn opening_an_embedded_store_logs_no_warning() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    let out = run_in(
        repo.path(),
        &["index", "--project-id", "quiet", "--db-url", &store_in(repo.path())],
    );
    assert!(out.status.success(), "index failed: {}", stderr(&out));

    // An embedded engine has no auth layer, so there is nothing to sign in
    // to and nothing to warn about. A warning on every command against the
    // default store teaches a user that codegraph's warnings are noise.
    let err = stderr(&out);
    assert!(
        !err.contains("signin failed"),
        "an embedded store must not report a signin failure, got:\n{err}"
    );
    assert!(
        !err.contains("WARN"),
        "opening an embedded store must log nothing at WARN, got:\n{err}"
    );

    // The skip is decided by the url scheme, and only for the two engines
    // that are known to have no auth. Anything else still gets its attempt
    // and its warning.
    assert!(codegraph::db::is_embedded("surrealkv://.codegraph/graph.db"));
    assert!(codegraph::db::is_embedded("mem://"));
    assert!(!codegraph::db::is_embedded("ws://localhost:8000"));
    assert!(!codegraph::db::is_embedded("wss://db.example.invalid"));
    assert!(!codegraph::db::is_embedded("http://localhost:8000"));
}

// ============================================================================
// D7: a file is not a directory
// ============================================================================

#[test]
fn indexing_a_file_says_to_pass_its_directory() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    let out = run_in(
        repo.path(),
        &[
            "index",
            "src/keep.rs",
            "--project-id",
            "d7",
            "--db-url",
            &store_in(repo.path()),
        ],
    );
    assert!(
        !out.status.success(),
        "indexing a file must not succeed as an empty index:\n{}",
        stdout(&out)
    );
    let err = stderr(&out);
    assert!(err.contains("keep.rs"), "the message must name the path:\n{err}");
    assert!(err.contains("is a file"), "the message must say why:\n{err}");
    assert!(
        err.contains("codegraph index "),
        "the message must show the command to run instead:\n{err}"
    );
}

#[test]
fn index_with_no_path_indexes_the_current_directory() {
    let repo = temp_git_repo(&[("src/keep.rs", "pub fn kept_function() -> usize { 1 }\n")]);
    let out = run_in(
        repo.path(),
        &["index", "--project-id", "d7b", "--db-url", &store_in(repo.path())],
    );
    assert!(
        out.status.success(),
        "index with no path must default to the working directory:\n{}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("1 scanned"), "got:\n{}", stdout(&out));
}

// ============================================================================
// D8: .jsx parses as JSX
// ============================================================================

#[tokio::test]
async fn a_jsx_component_reaches_the_graph() {
    use codegraph::index::{index_project, IndexConfig, IndexingTier};

    let db = std::sync::Arc::new(
        surrealdb::engine::any::connect("mem://")
            .await
            .expect("mem store"),
    );
    db.use_ns("codegraph").use_db("codegraph").await.expect("ns/db");
    db.query(include_str!("../src/schema.surql"))
        .await
        .expect("schema")
        .check()
        .expect("schema check");

    let result = index_project(
        &db,
        &IndexConfig {
            project_id: "d8".to_string(),
            root_path: repo_path("tests/fixtures/run_anywhere/jsx"),
            tier: IndexingTier::Full,
            languages: None,
            force: true,
        },
    )
    .await
    .expect("index");
    assert_eq!(result.files_skipped, 0, "errors: {:?}", result.errors);

    let names = codegraph::graph::search::search_nodes(&db, "d8", "", None, 200)
        .await
        .expect("search")
        .into_iter()
        .map(|n| n.name)
        .collect::<Vec<_>>();

    // The function whose body is a JSX element. Under the plain TypeScript
    // grammar the body is an error node and this name never appears.
    assert!(
        names.iter().any(|n| n == "renderPanel"),
        "the JSX component must be extracted, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "panelTitle"),
        "the function after the JSX one must survive too, got: {names:?}"
    );
    // And .mjs, which stays on the plain grammar, keeps working.
    assert!(
        names.iter().any(|n| n == "loadRows"),
        "the .mjs module must still parse, got: {names:?}"
    );
}

// ============================================================================
// D9: --explain
// ============================================================================

#[test]
fn explain_attaches_evidence_chains_and_is_absent_without_the_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = store_in(dir.path());
    let fixture = repo_path("tests/fixtures/rename-refactor/after");
    let fixture = fixture.to_string_lossy().into_owned();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let indexed = run_in(
        root,
        &["index", &fixture, "--project-id", "d9", "--db-url", &db, "--force"],
    );
    assert!(indexed.status.success(), "index failed: {}", stderr(&indexed));

    let plain = run_in(
        root,
        &[
            "query", "--kind", "rdeps", "--name", "helper", "--project-id", "d9", "--db-url",
            &db, "--json",
        ],
    );
    assert!(plain.status.success(), "{}", stderr(&plain));
    let plain_json: serde_json::Value =
        serde_json::from_str(stdout(&plain).trim()).expect("json");
    assert!(
        plain_json.get("explanations").is_none(),
        "the field must stay absent when explain was not asked for: {plain_json}"
    );

    let explained = run_in(
        root,
        &[
            "query", "--kind", "rdeps", "--name", "helper", "--project-id", "d9", "--db-url",
            &db, "--json", "--explain",
        ],
    );
    assert!(explained.status.success(), "{}", stderr(&explained));
    let explained_json: serde_json::Value =
        serde_json::from_str(stdout(&explained).trim()).expect("json");
    let chains = explained_json["explanations"]
        .as_array()
        .expect("explanations must be present with --explain");
    assert!(
        !chains.is_empty(),
        "the stale-rename fixture must produce at least one chain"
    );

    // Explaining a verdict must not change it.
    let mut stripped = explained_json.clone();
    stripped
        .as_object_mut()
        .expect("object")
        .remove("explanations");
    assert_eq!(
        stripped, plain_json,
        "--explain changed the verdict it was supposed to explain"
    );

    // And the text path renders the same chains rather than ignoring the flag.
    let text = run_in(
        root,
        &[
            "query", "--kind", "rdeps", "--name", "helper", "--project-id", "d9", "--db-url",
            &db, "--explain",
        ],
    );
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(
        stdout(&text).contains("=== Evidence ("),
        "the text path must render the chains, got:\n{}",
        stdout(&text)
    );
}
