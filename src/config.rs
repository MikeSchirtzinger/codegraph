//! Where the project id comes from when nobody passes one.
//!
//! Requiring `--project-id` on every invocation was the single largest piece
//! of tribal knowledge in running codegraph on a repo for the first time:
//! the value is arbitrary, nothing validates it, and getting it wrong
//! silently produces an empty graph. This module removes the requirement
//! without removing the control.
//!
//! Resolution order, highest priority first:
//!
//! 1. an explicit `--project-id` on the command line
//! 2. the `CODEGRAPH_PROJECT_ID` environment variable
//! 3. `project_id` in `<repo root>/.codegraph/config.toml`
//! 4. the sanitized basename of the repo root
//!
//! Passing `--project-id` explicitly behaves exactly as it always has, so
//! every existing invocation keeps its old meaning. The lower rungs only
//! ever fill in a value that was previously mandatory.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::plan::CODEGRAPH_DIR;

/// Environment variable consulted when no `--project-id` is given.
pub const PROJECT_ID_ENV: &str = "CODEGRAPH_PROJECT_ID";

/// Per-repo configuration file, inside `.codegraph/`.
pub const CONFIG_FILE: &str = "config.toml";

/// Which rung of the resolution order supplied the project id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectIdSource {
    /// An explicit `--project-id` argument.
    Explicit,
    /// The `CODEGRAPH_PROJECT_ID` environment variable.
    Environment,
    /// The `project_id` key in this config file.
    ConfigFile(PathBuf),
    /// The basename of this directory, sanitized.
    RepoRoot(PathBuf),
}

impl ProjectIdSource {
    /// One short phrase naming where the value came from, for the log line
    /// and for `codegraph doctor`.
    pub fn describe(&self) -> String {
        match self {
            ProjectIdSource::Explicit => "the --project-id argument".to_string(),
            ProjectIdSource::Environment => format!("the {PROJECT_ID_ENV} environment variable"),
            ProjectIdSource::ConfigFile(p) => format!("project_id in {}", p.display()),
            ProjectIdSource::RepoRoot(p) => format!("the name of the repo root {}", p.display()),
        }
    }
}

/// A project id together with the rung that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProjectId {
    /// The project id to use.
    pub id: String,
    /// Where it came from.
    pub source: ProjectIdSource,
}

/// Resolve the project id for one invocation.
///
/// `explicit` is the `--project-id` argument if the user gave one.
/// `path_hint` is the path the command is about to act on (the `index` path
/// argument, say); when it is `None` the current working directory is used.
///
/// Errors only when every rung fails, which in practice means the working
/// directory has a name that sanitizes to nothing. The error says exactly
/// what to pass.
pub fn resolve_project_id(
    explicit: Option<&str>,
    path_hint: Option<&Path>,
) -> Result<ResolvedProjectId> {
    if let Some(id) = explicit {
        let id = id.trim();
        if id.is_empty() {
            return Err(anyhow!(
                "--project-id was given but is empty. Pass a non-empty id, or omit the flag to let codegraph derive one from the repo"
            ));
        }
        return Ok(ResolvedProjectId {
            id: id.to_string(),
            source: ProjectIdSource::Explicit,
        });
    }

    if let Ok(raw) = std::env::var(PROJECT_ID_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(ResolvedProjectId {
                id: trimmed.to_string(),
                source: ProjectIdSource::Environment,
            });
        }
    }

    let start = match path_hint {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("cannot read the current working directory")?,
    };
    let root = repo_root(&start);

    let config_path = root.join(CODEGRAPH_DIR).join(CONFIG_FILE);
    if let Some(id) = read_config_project_id(&config_path)? {
        return Ok(ResolvedProjectId {
            id,
            source: ProjectIdSource::ConfigFile(config_path),
        });
    }

    let base = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    match sanitize_project_id(base) {
        Some(id) => Ok(ResolvedProjectId {
            id,
            source: ProjectIdSource::RepoRoot(root),
        }),
        None => Err(anyhow!(
            "cannot derive a project id from {}: its name has no letters or digits to build one from. Pass --project-id, or set {PROJECT_ID_ENV}",
            root.display()
        )),
    }
}

/// Resolve the project id and log which rung supplied it.
///
/// The log line is at debug level for an explicit argument, since the caller
/// already knows what they typed, and at info level for every derived value,
/// since a derived id is exactly the kind of thing a user needs to see to
/// trust the run. Both go to stderr, so machine-readable stdout stays clean.
pub fn project_id(explicit: Option<&str>, path_hint: Option<&Path>) -> Result<String> {
    let resolved = resolve_project_id(explicit, path_hint)?;
    match resolved.source {
        ProjectIdSource::Explicit => {
            tracing::debug!("project id {} from {}", resolved.id, resolved.source.describe());
        }
        _ => {
            tracing::info!("project id {} from {}", resolved.id, resolved.source.describe());
        }
    }
    Ok(resolved.id)
}

/// Key in `.codegraph/config.toml` holding the default indexing tier.
pub const TIER_KEY: &str = "tier";

/// Key in `.codegraph/config.toml` holding the store url.
pub const DB_URL_KEY: &str = "db_url";

/// Store url relative to a repo root, as `init` writes it into
/// `config.toml`. Relative on purpose: an absolute path in a committed file
/// is wrong on every machine but the one that wrote it.
pub const DEFAULT_DB_PATH: &str = ".codegraph/graph.db";

/// Resolve the store url for one invocation, anchored at the repo root.
///
/// Order: an explicit `--db-url` (clap folds `SURREALDB_URL` into that
/// argument already), then `db_url` in `.codegraph/config.toml`, then an
/// embedded surrealkv file under the repo root.
///
/// Anchoring at the repo root rather than the working directory is the
/// point. The old relative default meant `cd src && codegraph query` created
/// a second, empty store at `src/.codegraph/graph.db` and reported an empty
/// graph, with nothing on screen to say why. One repository now has one
/// store no matter which directory the command is typed in. Invocations run
/// from the repo root, which is every invocation in the README, resolve to
/// the same path they always did.
///
/// An explicit url is passed through untouched, including a relative one:
/// someone who typed a path meant that path.
pub fn db_url(explicit: Option<&str>, root: &Path) -> Result<String> {
    if let Some(url) = explicit {
        let url = url.trim();
        if !url.is_empty() {
            return Ok(url.to_string());
        }
    }

    let config_path = root.join(CODEGRAPH_DIR).join(CONFIG_FILE);
    if let Some(raw) = read_config_key(&config_path, DB_URL_KEY)? {
        return Ok(anchor_db_url(&raw, root));
    }

    Ok(anchor_db_url(
        &format!("surrealkv://{DEFAULT_DB_PATH}"),
        root,
    ))
}

/// Rewrite a relative embedded-store url so it names a path under `root`.
///
/// Only `surrealkv://` is anchored. A remote url (`ws://`, `wss://`,
/// `http://`, `https://`) has no filesystem meaning, and `mem://` has no
/// path at all, so both pass through.
fn anchor_db_url(raw: &str, root: &Path) -> String {
    let Some(path) = raw.strip_prefix("surrealkv://") else {
        return raw.to_string();
    };
    if Path::new(path).is_absolute() {
        return raw.to_string();
    }
    format!("surrealkv://{}", root.join(path).display())
}

/// Resolve the indexing tier: an explicit `--tier`, then `tier` in
/// `.codegraph/config.toml`, then `full`.
///
/// `full` stays the default because the numbers in the README were measured
/// at it. The config key exists so a repository where `full` is too slow can
/// record that choice once instead of in every command line.
pub fn tier(explicit: Option<&str>, root: &Path) -> Result<String> {
    if let Some(t) = explicit {
        let t = t.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }

    let config_path = root.join(CODEGRAPH_DIR).join(CONFIG_FILE);
    if let Some(raw) = read_config_key(&config_path, TIER_KEY)? {
        return Ok(raw);
    }

    Ok(DEFAULT_TIER.to_string())
}

/// The tier used when nothing else says otherwise.
pub const DEFAULT_TIER: &str = "full";

/// The repo root for `start`: the nearest ancestor containing a `.git` entry,
/// or `start` itself when there is none.
///
/// `.git` is matched as an entry rather than a directory on purpose, because
/// a linked worktree has a `.git` *file*. Resolved through the filesystem
/// directly rather than by shelling out to git, so it costs one stat per
/// level and works with no git binary on the box.
pub fn repo_root(start: &Path) -> PathBuf {
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(start))
            .unwrap_or_else(|_| start.to_path_buf())
    };
    // `index` defaults its path to ".", so the join above yields
    // `/repo/.`, and every path derived from it reads `/repo/./.codegraph`.
    // Dropping the no-op components is cosmetic and it is the cosmetics of
    // every path this command prints. `..` is deliberately left alone: it
    // cannot be removed without resolving symlinks, which would change
    // which directory the path names.
    let absolute: PathBuf = absolute
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();

    // A file argument (rare, but `index` accepts a path) anchors at its
    // directory.
    let mut dir = if absolute.is_file() {
        absolute.parent().map(Path::to_path_buf).unwrap_or(absolute)
    } else {
        absolute
    };

    let anchor = dir.clone();
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return anchor,
        }
    }
}

/// Turn a directory name into a usable project id, or `None` when nothing
/// usable is left.
///
/// Letters, digits, underscores, and hyphens survive; every other character
/// becomes an underscore; leading and trailing underscores are trimmed. Case
/// is preserved, because a project id is an opaque key and silently
/// lowercasing it would make `--project-id MyRepo` and a derived id disagree.
pub fn sanitize_project_id(name: &str) -> Option<String> {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('_');
    if trimmed.chars().any(|c| c.is_ascii_alphanumeric()) {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Read `project_id` out of `.codegraph/config.toml`.
///
/// This reads exactly one key and is not a general TOML parser. It accepts
/// `project_id = "value"` (single or double quoted, or bare) at the top
/// level of the file, ignores blank lines and `#` comments, and stops at the
/// first `[section]` header, since anything under a section is not the
/// top-level key. A missing file is not an error; it is rung three of four
/// declining to answer.
///
/// Keeping this to one hand-read key is deliberate: the alternative is a
/// TOML dependency for a single string, and this file has exactly one key
/// defined for it.
fn read_config_project_id(path: &Path) -> Result<Option<String>> {
    read_config_key(path, "project_id")
}

/// Read one top-level string key out of `.codegraph/config.toml`.
///
/// See [`read_config_project_id`] for why this is a hand-read of three keys
/// rather than a TOML parser.
pub fn read_config_key(path: &Path, key: &str) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("cannot read {}", path.display()))
        }
    };

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            break;
        }
        let Some((found_key, value)) = line.split_once('=') else {
            continue;
        };
        if found_key.trim() != key {
            continue;
        }
        let value = value.trim();
        // Strip an inline comment only when the value is unquoted, so a `#`
        // inside a quoted id survives.
        let unquoted = if let Some(rest) = value.strip_prefix('"') {
            rest.split_once('"').map(|(v, _)| v.to_string())
        } else if let Some(rest) = value.strip_prefix('\'') {
            rest.split_once('\'').map(|(v, _)| v.to_string())
        } else {
            Some(
                value
                    .split('#')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            )
        };
        let Some(id) = unquoted else {
            return Err(anyhow!(
                "{}:{}: {key} has an unterminated quoted value. Write it as {key} = \"a-value\"",
                path.display(),
                lineno + 1
            ));
        };
        let id = id.trim();
        if id.is_empty() {
            return Err(anyhow!(
                "{}:{}: {key} is set but empty. Give it a value, or delete the line to let codegraph use its default",
                path.display(),
                lineno + 1
            ));
        }
        return Ok(Some(id.to_string()));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_wins_over_everything() {
        let r = resolve_project_id(Some("chosen"), None).expect("resolve");
        assert_eq!(r.id, "chosen");
        assert_eq!(r.source, ProjectIdSource::Explicit);
    }

    #[test]
    fn empty_explicit_is_an_error_that_says_what_to_do() {
        let err = resolve_project_id(Some("   "), None).expect_err("must reject");
        assert!(
            err.to_string().contains("--project-id was given but is empty"),
            "{err}"
        );
    }

    #[test]
    fn sanitize_maps_unusable_characters_and_trims() {
        assert_eq!(sanitize_project_id("my repo"), Some("my_repo".to_string()));
        assert_eq!(sanitize_project_id("codegraph"), Some("codegraph".to_string()));
        assert_eq!(sanitize_project_id("a.b"), Some("a_b".to_string()));
        assert_eq!(sanitize_project_id("__"), None);
        assert_eq!(sanitize_project_id(""), None);
    }

    #[test]
    fn config_file_key_is_read_and_quoting_is_handled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        std::fs::write(&path, "# a comment\nproject_id = \"quoted-id\"\n").unwrap();
        assert_eq!(read_config_project_id(&path).unwrap(), Some("quoted-id".into()));

        std::fs::write(&path, "project_id = bare-id  # trailing\n").unwrap();
        assert_eq!(read_config_project_id(&path).unwrap(), Some("bare-id".into()));

        std::fs::write(&path, "[other]\nproject_id = \"not-top-level\"\n").unwrap();
        assert_eq!(read_config_project_id(&path).unwrap(), None);

        assert_eq!(
            read_config_project_id(&dir.path().join("absent.toml")).unwrap(),
            None
        );
    }

    #[test]
    fn repo_root_walks_up_to_the_git_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let deep = root.join("src").join("index");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(repo_root(&deep), root);
    }

    #[test]
    fn repo_root_without_a_git_marker_is_the_path_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("no-git");
        std::fs::create_dir_all(&deep).unwrap();
        // /tmp and its ancestors carry no .git, so the walk falls back to
        // the anchor it started from.
        assert_eq!(repo_root(&deep), deep);
    }
}
