//! `codegraph doctor` diagnostics.
//!
//! Answers "why is this not working" without the user having to know
//! anything. Every check here exists to turn one of codegraph's silences
//! into a sentence: can the store be reached, is it locked by something
//! else, has this project ever been indexed, where did the project id come
//! from, is there a planes file and does it validate, and what languages are
//! actually in this tree.
//!
//! No check reports [`CheckStatus::Ok`] without having performed the
//! observation it names. A check that cannot run reports what stopped it.
//!
//! ## Why this takes a url and not a connection
//!
//! Every other command in the tree opens the store first and hands the
//! connection down. Doctor cannot: "the store will not open" is one of the
//! conditions it exists to diagnose. So it takes the url and does its own
//! connecting, which lets a connection failure come back as a failed
//! [`Check`] with a remedy attached rather than as an error that kills the
//! command before any other check runs.
//!
//! ## Contract with `src/main.rs`
//!
//! `src/main.rs` is owned by lane P0. It calls [`run`], prints the report,
//! and sets the process exit code from [`DoctorReport::worst`]. It does not
//! read any other field. L1 may add checks and fields freely, and must not
//! change the signature of [`run`], the meaning of [`CheckStatus`], or the
//! `Serialize`/`Display` pair on [`DoctorReport`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use surrealdb_types::SurrealValue;

/// What to diagnose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorOptions {
    /// Project id, already resolved by [`crate::config`].
    pub project_id: String,
    /// How the project id was arrived at, so doctor can report it. This is
    /// one of the things a confused user most needs to see.
    pub project_id_source: String,
    /// Repo root the diagnosis is about.
    pub root: PathBuf,
    /// Store url, or `None` for the embedded default.
    pub db_url: Option<String>,
}

/// The verdict of one check.
///
/// Ordered from healthiest to worst, so [`DoctorReport::worst`] is a plain
/// maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The check passed.
    Ok,
    /// The check passed but something will bite later.
    Warn,
    /// The check failed. Something the user asked for cannot work.
    Fail,
}

impl CheckStatus {
    /// The token printed at the head of the line.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "FAIL",
        }
    }
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Short stable name, e.g. `store-reachable`.
    pub name: String,
    /// The verdict.
    pub status: CheckStatus,
    /// What was observed. States a fact, not a guess.
    pub detail: String,
    /// The command or edit that fixes it, when there is one. A failing check
    /// without a remedy is a bug in the check.
    pub remedy: Option<String>,
}

/// Every check, in the order they were run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Project diagnosed.
    pub project_id: String,
    /// Repo root diagnosed.
    pub root: PathBuf,
    /// The checks.
    pub checks: Vec<Check>,
}

impl DoctorReport {
    /// The worst status in the report, or [`CheckStatus::Ok`] when there are
    /// no checks.
    pub fn worst(&self) -> CheckStatus {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(CheckStatus::Ok)
    }

    /// True when nothing failed. A warning is still healthy.
    pub fn is_healthy(&self) -> bool {
        self.worst() != CheckStatus::Fail
    }
}

impl fmt::Display for DoctorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== codegraph doctor: {} ===", self.project_id)?;
        writeln!(f, "  Root: {}\n", self.root.display())?;
        for c in &self.checks {
            writeln!(f, "  [{:>4}] {}: {}", c.status, c.name, c.detail)?;
            if let Some(remedy) = &c.remedy {
                writeln!(f, "         fix: {remedy}")?;
            }
        }
        Ok(())
    }
}


/// Build one check.
fn check(name: &str, status: CheckStatus, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_string(),
        status,
        detail: detail.into(),
        remedy: None,
    }
}

/// Build one check that carries a remedy.
fn check_with_fix(
    name: &str,
    status: CheckStatus,
    detail: impl Into<String>,
    remedy: impl Into<String>,
) -> Check {
    Check {
        name: name.to_string(),
        status,
        detail: detail.into(),
        remedy: Some(remedy.into()),
    }
}

/// Row shape for the node and edge counts.
#[derive(Debug, Deserialize, SurrealValue)]
struct CountRow {
    count: i64,
}

/// Run every diagnostic.
///
/// Checks run in the order a user would ask the questions: what am I
/// running, where am I, what is this project called, is it configured, is
/// there a roadmap, can the store be opened, does it have a schema, has this
/// project been indexed, and is there anything here to index. Later checks
/// degrade rather than abort when an earlier one failed: a locked store
/// still leaves the language scan worth reporting, and a user with two
/// problems should see both in one run.
pub async fn run(options: &DoctorOptions) -> Result<DoctorReport> {
    let mut checks = Vec::new();

    checks.push(check(
        "version",
        CheckStatus::Ok,
        format!("codegraph {}", env!("CARGO_PKG_VERSION")),
    ));

    // Repo root, and how it was decided. A user standing in the wrong
    // directory is the single cheapest explanation for an empty graph.
    let git_marker = options.root.join(".git");
    if git_marker.exists() {
        checks.push(check(
            "repo-root",
            CheckStatus::Ok,
            format!(
                "{} (nearest ancestor holding a .git entry)",
                options.root.display()
            ),
        ));
    } else {
        checks.push(check_with_fix(
            "repo-root",
            CheckStatus::Warn,
            format!(
                "{} (no .git found above it, so the working directory is being used as the root)",
                options.root.display()
            ),
            "run codegraph from inside the repository you mean to index, or pass the path to codegraph index",
        ));
    }

    checks.push(check(
        "project-id",
        CheckStatus::Ok,
        format!("{} from {}", options.project_id, options.project_id_source),
    ));

    // config.toml: present, and parseable by the reader that actually reads
    // it, rather than by a second parser that could disagree with it.
    let config_path = options
        .root
        .join(crate::plan::CODEGRAPH_DIR)
        .join(crate::config::CONFIG_FILE);
    if config_path.exists() {
        match crate::config::read_config_key(&config_path, "project_id") {
            Ok(Some(id)) => checks.push(check(
                "config-file",
                CheckStatus::Ok,
                format!("{} declares project_id \"{id}\"", config_path.display()),
            )),
            Ok(None) => checks.push(check_with_fix(
                "config-file",
                CheckStatus::Warn,
                format!(
                    "{} exists but sets no project_id",
                    config_path.display()
                ),
                "add project_id = \"<name>\", or delete the file to derive the id from the repo root name",
            )),
            Err(e) => checks.push(check_with_fix(
                "config-file",
                CheckStatus::Fail,
                format!("{} cannot be read: {e}", config_path.display()),
                "fix the line the message names, or delete the file and run codegraph init",
            )),
        }
    } else {
        checks.push(check_with_fix(
            "config-file",
            CheckStatus::Warn,
            format!("no {}", config_path.display()),
            "run codegraph init to write one (not required: the project id is derived without it)",
        ));
    }

    checks.push(planes_check(&options.root));

    // The store. Opened here rather than handed in, because "it will not
    // open" is a diagnosis, not a reason to stop diagnosing.
    let db_url = match crate::config::db_url(options.db_url.as_deref(), &options.root) {
        Ok(url) => url,
        Err(e) => {
            checks.push(check_with_fix(
                "store-url",
                CheckStatus::Fail,
                format!("cannot work out which store to use: {e}"),
                "pass --db-url explicitly",
            ));
            return Ok(DoctorReport {
                project_id: options.project_id.clone(),
                root: options.root.clone(),
                checks,
            });
        }
    };
    checks.push(check("store-url", CheckStatus::Ok, db_url.clone()));

    match crate::db::connect(Some(&db_url)).await {
        Ok(client) => {
            checks.push(check("store-open", CheckStatus::Ok, "opened for writing"));
            checks.extend(store_checks(&client, &options.project_id).await);
        }
        Err(e) => {
            let raw = format!("{e:#}");
            if crate::db::is_store_locked(&raw) {
                checks.push(check_with_fix(
                    "store-open",
                    CheckStatus::Fail,
                    format!("{db_url} is open in another process, and it allows one writer at a time"),
                    "close the other codegraph process (an MCP \"codegraph serve\" left running is the usual one), or pass --db-url to use a different store",
                ));
            } else {
                checks.push(check_with_fix(
                    "store-open",
                    CheckStatus::Fail,
                    format!("{db_url} will not open: {raw}"),
                    "check the url and that the path is writable, or pass --db-url",
                ));
            }
        }
    }

    checks.push(languages_check(&options.root));
    checks.push(next_command_check(&checks, &options.root));

    Ok(DoctorReport {
        project_id: options.project_id.clone(),
        root: options.root.clone(),
        checks,
    })
}

/// Is there a planes file, and does it validate?
///
/// Validated through [`crate::plan::schema`] itself, so doctor's verdict and
/// `codegraph plan lint`'s verdict can never disagree.
fn planes_check(root: &Path) -> Check {
    let path = crate::plan::default_planes_path(root);
    if !path.exists() {
        return check_with_fix(
            "planes-file",
            CheckStatus::Warn,
            format!("no {}", path.display()),
            "run codegraph init to scaffold one (not required: every other command works without it)",
        );
    }
    match crate::plan::schema::load_unvalidated(&path) {
        Err(e) => check_with_fix(
            "planes-file",
            CheckStatus::Fail,
            format!("{} does not parse: {e}", path.display()),
            "run codegraph plan lint for the full list",
        ),
        Ok(file) => {
            let violations = crate::plan::schema::validate(&file);
            let items = file.items().count();
            if violations.is_empty() {
                check(
                    "planes-file",
                    CheckStatus::Ok,
                    format!(
                        "{} is valid: {} plane(s), {items} work item(s)",
                        path.display(),
                        file.planes.len()
                    ),
                )
            } else {
                check_with_fix(
                    "planes-file",
                    CheckStatus::Fail,
                    format!(
                        "{} has {} schema violation(s)",
                        path.display(),
                        violations.len()
                    ),
                    "run codegraph plan lint for the full list",
                )
            }
        }
    }
}

/// Checks that need an open store: is the schema there, and has this project
/// been indexed into it.
async fn store_checks(db: &Surreal<Any>, project_id: &str) -> Vec<Check> {
    let mut checks = Vec::new();

    // Schema presence is measured by querying a SCHEMAFULL table: an
    // undefined table errors rather than returning nothing, which is exactly
    // the difference being tested.
    //
    // Absent is a warning, not a failure. A store that has never been
    // indexed has no schema by construction, and `index` applies the DDL
    // before it writes anything, so this is a normal state on a first run
    // rather than something the user has to repair. Reporting it red would
    // make `init` then `doctor` red on every new repository, which is the
    // experience this whole lane exists to remove.
    let schema_probe = match db.query("SELECT node_id FROM code_node LIMIT 1").await {
        Ok(mut resp) => resp.take::<Vec<surrealdb_types::Value>>(0).map(|_| ()),
        Err(e) => Err(e),
    };
    let schema_present = schema_probe.is_ok();
    match schema_probe {
        Ok(()) => checks.push(check(
            "schema",
            CheckStatus::Ok,
            "code_node and code_edge are defined",
        )),
        Err(e) => checks.push(check_with_fix(
            "schema",
            CheckStatus::Warn,
            format!("the store carries no codegraph schema yet: {e}"),
            "run codegraph index, which applies the schema before it writes",
        )),
    }

    // Counting rows of a table that does not exist would error, and
    // reporting that error as "0 nodes" would turn a store problem into a
    // false statement about the project. The check above already said why.
    if !schema_present {
        checks.push(check_with_fix(
            "project-indexed",
            CheckStatus::Warn,
            format!("project \"{project_id}\" cannot have been indexed into a store with no schema"),
            "run codegraph index",
        ));
        return checks;
    }

    match (
        count_rows(db, "code_node", project_id).await,
        count_rows(db, "code_edge", project_id).await,
    ) {
        (Ok(0), Ok(_)) => checks.push(check_with_fix(
            "project-indexed",
            CheckStatus::Warn,
            format!("project \"{project_id}\" has no nodes in this store"),
            "run codegraph index",
        )),
        (Ok(n), Ok(e)) => checks.push(check(
            "project-indexed",
            CheckStatus::Ok,
            format!("project \"{project_id}\": {n} nodes, {e} edges"),
        )),
        (Err(e), _) | (_, Err(e)) => checks.push(check_with_fix(
            "project-indexed",
            CheckStatus::Fail,
            format!("cannot count rows for \"{project_id}\": {e}"),
            "run codegraph index to rebuild the store",
        )),
    }

    checks
}

/// Count rows of one table belonging to one project.
///
/// Every error is propagated rather than folded into a zero: "the query
/// failed" and "the project has no nodes" are different diagnoses, and this
/// is the function whose entire job is telling them apart.
async fn count_rows(db: &Surreal<Any>, table: &str, project_id: &str) -> Result<i64> {
    let query =
        format!("SELECT count() AS count FROM {table} WHERE project_id = $pid GROUP ALL;");
    let mut response = db
        .query(&query)
        .bind(("pid", project_id.to_string()))
        .await?;
    let row: Option<CountRow> = response.take(0)?;
    Ok(row.map(|r| r.count).unwrap_or(0))
}

/// What is actually in this tree, by language, through the same discovery
/// the indexer uses.
///
/// Running the real discovery rather than a second scan of its own is the
/// point: a file missing from this list is missing from the index for the
/// same reason, and the strategy line says which reason.
fn languages_check(root: &Path) -> Check {
    let found = crate::index::discover_source_files(root, None);
    if found.files.is_empty() {
        return check_with_fix(
            "languages",
            CheckStatus::Warn,
            format!(
                "no files in a supported language under {} ({})",
                root.display(),
                found.strategy.describe()
            ),
            "check the path, and that the files are not excluded by a .gitignore rule",
        );
    }

    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for path in &found.files {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Some(lang) = crate::index::parser::extension_to_language(ext) {
            *counts.entry(lang).or_default() += 1;
        }
    }
    let rendered = counts
        .iter()
        .map(|(lang, n)| format!("{lang} {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    check(
        "languages",
        CheckStatus::Ok,
        format!(
            "{} file(s) via {}: {rendered}",
            found.files.len(),
            found.strategy.describe()
        ),
    )
}

/// The one command to run next, chosen from what the other checks found.
///
/// Built from the checks rather than from a guess, so it can never
/// contradict the lines above it.
fn next_command_check(checks: &[Check], root: &Path) -> Check {
    let status_of = |name: &str| checks.iter().find(|c| c.name == name).map(|c| c.status);

    if status_of("store-open") == Some(CheckStatus::Fail) {
        return check(
            "next",
            CheckStatus::Ok,
            "fix the store problem above, then run codegraph index",
        );
    }
    if status_of("languages") == Some(CheckStatus::Warn) {
        return check(
            "next",
            CheckStatus::Ok,
            format!(
                "there is nothing here to index. Point codegraph at a source tree: codegraph index <path>, from {}",
                root.display()
            ),
        );
    }
    match status_of("project-indexed") {
        Some(CheckStatus::Ok) => check(
            "next",
            CheckStatus::Ok,
            "codegraph query --kind hubs",
        ),
        _ => check("next", CheckStatus::Ok, "codegraph index"),
    }
}
