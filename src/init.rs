//! `codegraph init` scaffolding.
//!
//! Creates `.codegraph/` in a repository and puts enough in it that the next
//! command needs no flags: a `config.toml` carrying the project id (rung
//! three of [`crate::config`]'s resolution order) together with the tier and
//! store url defaults, and a starter `planes.yaml` that parses cleanly under
//! [`crate::plan::schema::load`]. Everything written here is text a human is
//! happy to commit, because `.codegraph/` is tracked.
//!
//! It also repairs the `.gitignore` rule for `.codegraph/`. A
//! whole-directory ignore cannot be negated (git does not descend into an
//! excluded directory, so `!.codegraph/planes.yaml` under a `.codegraph/`
//! rule never matches), which means a repo with the obvious rule silently
//! cannot commit the two files init just told it to commit. See
//! [`gitignore_patch`].
//!
//! Existing files are never silently replaced. Without `force` an existing
//! file is reported in [`InitReport::skipped`] and left exactly as it was;
//! with `force` it is replaced and reported in [`InitReport::overwritten`].
//! `planes.yaml` is the exception and is never overwritten by either path:
//! it is a hand-maintained roadmap document, replacing one with a three-line
//! template destroys work no other copy holds, and nothing about init needs
//! to. The store at `.codegraph/graph.db` is not an init concern and is not
//! touched by either path.
//!
//! ## Contract with `src/main.rs`
//!
//! `src/main.rs` is owned by lane P0 and calls [`run`] directly. It does not
//! read a field of [`InitReport`]: it serializes the value or prints its
//! [`Display`] rendering. L1 may add fields and change the rendering, but
//! must not change the signature of [`run`] or drop the `Serialize`/
//! `Display` pair.
//!
//! [`Display`]: std::fmt::Display

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::CONFIG_FILE;
use crate::plan::CODEGRAPH_DIR;

/// What to scaffold and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitOptions {
    /// Repo root to scaffold into. `.codegraph/` is created directly under
    /// it.
    pub root: PathBuf,
    /// Project id to record in `config.toml`, already resolved by
    /// [`crate::config`] so init never has to guess.
    pub project_id: String,
    /// Replace files that already exist instead of leaving them alone.
    pub force: bool,
}

/// What one `init` did, file by file, so a user can see exactly what
/// appeared in their repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitReport {
    /// Repo root that was scaffolded.
    pub root: PathBuf,
    /// Project id written into `config.toml`.
    pub project_id: String,
    /// Paths created by this run.
    pub created: Vec<PathBuf>,
    /// Paths that already existed and were left untouched.
    pub skipped: Vec<PathBuf>,
    /// Paths that already existed and were replaced because `force` was set.
    pub overwritten: Vec<PathBuf>,
    /// What init did to `.gitignore`, in one sentence, or `None` when there
    /// was no `.gitignore` to edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitignore: Option<String>,
    /// The command to run next. The whole point of init is that there is
    /// one and the user does not have to know it.
    pub next_command: String,
}

impl fmt::Display for InitReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== codegraph init ===")?;
        writeln!(f, "  Root:        {}", self.root.display())?;
        writeln!(f, "  Project:     {}", self.project_id)?;
        for p in &self.created {
            writeln!(f, "  created      {}", p.display())?;
        }
        for p in &self.overwritten {
            writeln!(f, "  replaced     {}", p.display())?;
        }
        for p in &self.skipped {
            writeln!(f, "  kept         {} (already present)", p.display())?;
        }
        if let Some(note) = &self.gitignore {
            writeln!(f, "  .gitignore   {note}")?;
        }
        if self.created.is_empty() && self.overwritten.is_empty() {
            writeln!(f, "\n  Nothing to do. This repository is already set up.")?;
        }
        if !self.skipped.is_empty() {
            writeln!(
                f,
                "\n  Pass --force to replace the files that were kept, except planes.yaml, which init never replaces."
            )?;
        }
        writeln!(f, "\n  Next: {}", self.next_command)?;
        Ok(())
    }
}


/// Scaffold `.codegraph/` in a repo.
///
/// Synchronous and free of any database connection on purpose: init is what
/// a user runs before anything works, so it must not be able to fail on a
/// store problem.
pub fn run(options: &InitOptions) -> Result<InitReport> {
    let dir = options.root.join(CODEGRAPH_DIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;

    let mut report = InitReport {
        root: options.root.clone(),
        project_id: options.project_id.clone(),
        created: Vec::new(),
        skipped: Vec::new(),
        overwritten: Vec::new(),
        gitignore: None,
        next_command: "codegraph index".to_string(),
    };

    let config_path = dir.join(CONFIG_FILE);
    write_file(
        &config_path,
        &config_toml(&options.project_id),
        options.force,
        &mut report,
    )?;

    // `planes.yaml` is never replaced, hence `force: false` regardless of
    // what the caller asked for. See the module docs.
    let planes_path = crate::plan::default_planes_path(&options.root);
    write_file(
        &planes_path,
        &starter_planes(&options.project_id, &options.root),
        false,
        &mut report,
    )?;

    report.gitignore = patch_gitignore(&options.root)?;

    Ok(report)
}

/// Write one scaffolded file, honoring the "never silently replace" rule and
/// recording which of the three things happened.
fn write_file(path: &Path, contents: &str, force: bool, report: &mut InitReport) -> Result<()> {
    let existed = path.exists();
    if existed && !force {
        report.skipped.push(path.to_path_buf());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("cannot write {}", path.display()))?;
    if existed {
        report.overwritten.push(path.to_path_buf());
    } else {
        report.created.push(path.to_path_buf());
    }
    Ok(())
}

/// The `.codegraph/config.toml` init writes.
///
/// Every key here is read back by [`crate::config`]. A config file whose
/// keys do nothing is worse than no config file, because editing it looks
/// like it worked.
fn config_toml(project_id: &str) -> String {
    format!(
        "# codegraph configuration for this repository.\n\
         # Written by \"codegraph init\". Safe to commit: it holds no secrets.\n\
         \n\
         # Project id for every codegraph command run here. An explicit\n\
         # --project-id and the CODEGRAPH_PROJECT_ID environment variable both\n\
         # take priority over this value.\n\
         project_id = \"{project_id}\"\n\
         \n\
         # Default indexing tier for \"codegraph index\", overridden by --tier.\n\
         #   fast     definitions only, no call edges\n\
         #   balanced adds call edges\n\
         #   full     adds references and type annotations\n\
         tier = \"{tier}\"\n\
         \n\
         # Where the graph lives, overridden by --db-url and SURREALDB_URL.\n\
         # A relative surrealkv path is resolved against this repository root,\n\
         # so the same store is used whichever directory you run from. Point\n\
         # it at ws://host:8000 to use a SurrealDB server instead.\n\
         db_url = \"surrealkv://{db_path}\"\n",
        project_id = project_id,
        tier = crate::config::DEFAULT_TIER,
        db_path = crate::config::DEFAULT_DB_PATH,
    )
}

/// A README at the repository root, if there is one.
///
/// Checked in the order a reader would expect to find one. The returned name
/// is the on-disk spelling, because the touch has to name the real path.
fn root_readme(root: &Path) -> Option<&'static str> {
    ["README.md", "README.rst", "README"]
        .into_iter()
        .find(|name| root.join(name).is_file())
}

/// The top-level directory holding the most files codegraph has a grammar
/// for, or `None` when every source file sits at the root (or there are
/// none).
///
/// Discovery is [`crate::index::discover_source_files`] rather than a walk of
/// its own, which matters twice: "a file codegraph can parse" means exactly
/// what it means to the indexer, and the ignore rules are the same ones, so
/// the scaffolded glob can never name a path the index will refuse to hold.
///
/// Ties break alphabetically, so the same tree always produces the same file.
fn busiest_source_dir(root: &Path) -> Option<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in crate::index::discover_source_files(root, None).files {
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let mut components = rel.components();
        let Some(first) = components.next() else {
            continue;
        };
        // A file directly at the root has no directory to name.
        if components.next().is_none() {
            continue;
        }
        *counts
            .entry(first.as_os_str().to_string_lossy().into_owned())
            .or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(name, n)| (*n, std::cmp::Reverse(name.clone())))
        .map(|(name, _)| name)
}

/// The starter `.codegraph/planes.yaml`.
///
/// Two requirements pull against each other. It has to pass
/// [`crate::plan::schema::validate`] with zero violations, and its example
/// touches have to actually resolve on the repository it is written into,
/// because the first thing a new user runs after `init` is `plan sync` and an
/// UNRESOLVED touch there reads as a broken tool rather than as a placeholder
/// they were supposed to replace.
///
/// A fixed `glob: "src/**"` failed the second requirement on six of the ten
/// corpus repositories (`specs/receipts/run-anywhere-corpus-20260914.md` §11),
/// among them a Java project whose sources live at
/// `retrofit/src/main/java/`. So the touches are derived from the tree
/// instead: the README if there is one, otherwise the `config.toml` init has
/// just written, which exists by construction, plus a glob over whichever
/// top-level directory actually holds this project's code.
///
/// Pinned by `starter_planes_file_is_valid` and, end to end through a real
/// `plan sync`, by `the_scaffolded_planes_file_resolves_on_the_repo_it_describes`.
fn starter_planes(project_id: &str, root: &Path) -> String {
    // `file:` binds on a path that exists, indexed or not, so a README is a
    // touch that resolves on a repository that has never been indexed. The
    // fallback names a file init itself wrote a moment ago.
    let anchor = root_readme(root).unwrap_or(concat!(".codegraph/", "config.toml"));
    // Ten spaces, matching the `- file:` line above it. The template's
    // indentation is written with `\` line continuations, which strip the
    // leading whitespace of each source line, so a substituted value has to
    // carry its own.
    let glob = match busiest_source_dir(root) {
        Some(dir) => format!("          - glob: \"{dir}/**\"\n"),
        None => String::new(),
    };

    format!(
        "# The roadmap as hyperedges over the code graph.\n\
         #\n\
         # Each work item names the set of files and symbols it touches. Once\n\
         # \"codegraph plan sync\" has run, that set is queryable: which planned\n\
         # work covers this file, which two active items collide, which plan has\n\
         # gone stale because the code it names no longer exists.\n\
         #\n\
         # Replace the example below with real work. Delete the file if you do\n\
         # not want a roadmap; nothing else depends on it.\n\
         #\n\
         # The example touches were picked to exist in this repository, so the\n\
         # first \"codegraph plan sync\" resolves all of them. Point them at the\n\
         # code your work actually changes.\n\
         version: 1\n\
         project: {project_id}\n\
         \n\
         planes:\n\
         \x20 - id: now\n\
         \x20   title: Work in flight\n\
         \x20   status: active\n\
         \x20   horizon: now\n\
         \x20   summary: Replace this plane with the work actually underway.\n\
         \x20   items:\n\
         \x20     - id: W-1\n\
         \x20       title: Describe one unit of work here\n\
         \x20       kind: feature\n\
         \x20       status: planned\n\
         \x20       touches:\n\
         \x20         - file: {anchor}\n\
         {glob}"
    )
}

/// The `.gitignore` edit init makes, as a pure function of the current file.
///
/// Returns the new contents and a one-sentence description, or `None` when
/// the file already says the right thing.
///
/// Two cases, and the first is the one that matters. A whole-directory rule
/// (`.codegraph/`, or `/.codegraph`, or `.codegraph`) tells git not to
/// descend into the directory at all, and a negation cannot re-include a
/// file whose parent directory was excluded. So a repo carrying the obvious
/// rule cannot commit the planes file or the config file no matter what
/// negations follow, and git reports nothing: the files are simply absent
/// from `git status`. Rewriting the rule as `.codegraph/*` keeps every
/// generated artifact ignored, including the store, while leaving the
/// directory itself traversable so the two negations below it can match.
///
/// The second case is a repo with no `.codegraph` rule at all, where the
/// three lines are appended.
pub fn gitignore_patch(current: &str) -> Option<(String, String)> {
    let has_negations = current
        .lines()
        .any(|l| l.trim() == "!.codegraph/planes.yaml");

    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in current.lines() {
        let trimmed = line.trim();
        let is_whole_dir = matches!(
            trimmed,
            ".codegraph/" | ".codegraph" | "/.codegraph/" | "/.codegraph"
        );
        if is_whole_dir && !replaced {
            out.push(".codegraph/*".to_string());
            if !has_negations {
                out.push("!.codegraph/planes.yaml".to_string());
                out.push("!.codegraph/config.toml".to_string());
            }
            replaced = true;
            continue;
        }
        if is_whole_dir {
            // A second whole-directory rule would re-exclude the directory
            // and undo the first rewrite. Drop it.
            continue;
        }
        out.push(line.to_string());
    }

    if replaced {
        let mut text = out.join("\n");
        text.push('\n');
        return Some((
            text,
            "rewrote the whole-directory .codegraph/ rule as .codegraph/* so planes.yaml and config.toml can be committed".to_string(),
        ));
    }

    // No whole-directory rule. If the two files are already committable,
    // there is nothing to do.
    if current.lines().any(|l| l.trim() == ".codegraph/*") || has_negations {
        return None;
    }

    let mut text = current.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(
        "\n# codegraph: ignore the generated store, keep the two files that are meant to be committed\n\
         .codegraph/*\n\
         !.codegraph/planes.yaml\n\
         !.codegraph/config.toml\n",
    );
    Some((
        text,
        "added .codegraph/* with negations for planes.yaml and config.toml".to_string(),
    ))
}

/// Apply [`gitignore_patch`] to the repo's `.gitignore`, if there is one.
///
/// A repository with no `.gitignore` gets none written: creating one is a
/// bigger decision than init is entitled to make, and the two files are
/// already committable without it.
fn patch_gitignore(root: &Path) -> Result<Option<String>> {
    let path = root.join(".gitignore");
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("cannot read {}", path.display()))
        }
    };

    match gitignore_patch(&current) {
        Some((text, note)) => {
            std::fs::write(&path, text)
                .with_context(|| format!("cannot write {}", path.display()))?;
            Ok(Some(note))
        }
        None => Ok(Some("already allows planes.yaml and config.toml".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_planes_file_is_valid() {
        // Two shapes, because the scaffold is now derived from the tree: a
        // repository with a README and a source directory, and the barest
        // possible one, where the only touch is the config file init wrote.
        let rich = tempfile::tempdir().expect("tempdir");
        std::fs::write(rich.path().join("README.md"), "# demo\n").unwrap();
        std::fs::create_dir_all(rich.path().join("lib")).unwrap();
        std::fs::write(rich.path().join("lib/a.rs"), "pub fn a() {}\n").unwrap();

        let bare = tempfile::tempdir().expect("tempdir");

        for root in [rich.path(), bare.path()] {
            let text = starter_planes("demo", root);
            let file = crate::plan::schema::parse_str(&text)
                .unwrap_or_else(|e| panic!("must parse: {e}\n{text}"));
            let violations = crate::plan::schema::validate(&file);
            assert!(
                violations.is_empty(),
                "the scaffolded planes file must validate clean, got: {violations:?}\n{text}"
            );
        }
    }

    #[test]
    fn the_scaffold_names_the_directory_the_code_is_actually_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("README.md"), "# demo\n").unwrap();
        // Two candidate directories, and the busier one must win. A fixed
        // "src/**" is exactly what this replaces, so a tree with no src/ at
        // all is the case that matters.
        std::fs::create_dir_all(root.join("lib/deep")).unwrap();
        std::fs::create_dir_all(root.join("tools")).unwrap();
        for n in 0..3 {
            std::fs::write(root.join(format!("lib/deep/f{n}.rs")), "pub fn f() {}\n").unwrap();
        }
        std::fs::write(root.join("tools/one.rs"), "pub fn t() {}\n").unwrap();
        // A source file at the root names no directory and must not vote.
        std::fs::write(root.join("build.rs"), "fn main() {}\n").unwrap();

        assert_eq!(busiest_source_dir(root), Some("lib".to_string()));
        assert_eq!(root_readme(root), Some("README.md"));

        let text = starter_planes("demo", root);
        assert!(text.contains("- file: README.md"), "{text}");
        assert!(text.contains("- glob: \"lib/**\""), "{text}");
        assert!(!text.contains("src/**"), "{text}");
    }

    #[test]
    fn a_tree_with_no_source_directory_still_scaffolds_a_resolvable_touch() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(busiest_source_dir(dir.path()), None);
        let text = starter_planes("demo", dir.path());
        assert!(text.contains("- file: .codegraph/config.toml"), "{text}");
        assert!(!text.contains("glob:"), "a glob that matches nothing must be omitted:\n{text}");
    }

    #[test]
    fn whole_directory_rule_is_rewritten_so_negations_can_match() {
        let (text, _) =
            gitignore_patch("/target\n.codegraph/\n.DS_Store\n").expect("must patch");
        assert_eq!(
            text,
            "/target\n.codegraph/*\n!.codegraph/planes.yaml\n!.codegraph/config.toml\n.DS_Store\n"
        );
        // And the result is a fixed point.
        assert!(gitignore_patch(&text).is_none(), "must be idempotent");
    }

    #[test]
    fn a_gitignore_with_no_codegraph_rule_gets_the_lines_appended() {
        let (text, _) = gitignore_patch("/target\n").expect("must patch");
        assert!(text.starts_with("/target\n"));
        assert!(text.contains("\n.codegraph/*\n"));
        assert!(text.contains("\n!.codegraph/planes.yaml\n"));
        assert!(text.contains("\n!.codegraph/config.toml\n"));
        assert!(gitignore_patch(&text).is_none(), "must be idempotent");
    }

    #[test]
    fn init_is_idempotent_and_never_replaces_the_planes_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join(".gitignore"), "/target\n.codegraph/\n").unwrap();

        let options = InitOptions {
            root: root.clone(),
            project_id: "demo".to_string(),
            force: false,
        };

        let first = run(&options).expect("first init");
        assert_eq!(first.created.len(), 2, "config.toml and planes.yaml");
        assert!(first.skipped.is_empty());

        // A hand-edited roadmap, which the second run must not touch.
        let planes = crate::plan::default_planes_path(&root);
        let edited = format!("{}\n# hand edited\n", std::fs::read_to_string(&planes).unwrap());
        std::fs::write(&planes, &edited).unwrap();

        let second = run(&options).expect("second init");
        assert!(second.created.is_empty(), "nothing new the second time");
        assert_eq!(second.skipped.len(), 2);
        assert_eq!(std::fs::read_to_string(&planes).unwrap(), edited);

        // Even with --force.
        let forced = run(&InitOptions {
            force: true,
            ..options
        })
        .expect("forced init");
        assert_eq!(forced.overwritten.len(), 1, "config.toml only");
        assert_eq!(std::fs::read_to_string(&planes).unwrap(), edited);

        // And the gitignore rewrite survived both later runs unchanged.
        let ignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(
            ignore,
            "/target\n.codegraph/*\n!.codegraph/planes.yaml\n!.codegraph/config.toml\n"
        );
    }
}
