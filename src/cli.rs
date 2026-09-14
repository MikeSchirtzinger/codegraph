//! Command line surface.
//!
//! `--project-id` is optional everywhere. When it is omitted the id is
//! resolved by [`codegraph::config::resolve_project_id`]: the
//! `CODEGRAPH_PROJECT_ID` environment variable, then `project_id` in
//! `.codegraph/config.toml`, then the sanitized name of the repo root.
//! Passing the flag explicitly behaves exactly as it always has.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand};

use crate::landscape::LandscapeFormat;
use crate::plan::{Horizon, Status};

#[derive(Parser)]
#[command(name = "codegraph")]
#[command(version)]
#[command(about = "Language-agnostic codebase graph indexer, backed by SurrealDB")]
pub struct Cli {
    #[arg(short, long)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

/// Shared wording for the optional `--project-id` flag, so all nine
/// subcommands describe the same resolution order in the same words.
const PROJECT_ID_HELP: &str = "Project ID for multi-tenant isolation. Defaults to CODEGRAPH_PROJECT_ID, then project_id in .codegraph/config.toml, then the name of the repo root";

/// Shared wording for the optional `--db-url` flag.
const DB_URL_HELP: &str =
    "SurrealDB URL (default: embedded .codegraph/graph.db, no server required)";

#[derive(Subcommand)]
pub enum Commands {
    /// Scaffold .codegraph/ in a repository so later commands need no flags
    Init {
        /// Repository root to scaffold (default: the current directory)
        path: Option<PathBuf>,

        /// Project ID to record in .codegraph/config.toml
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        /// Replace files that already exist instead of keeping them
        #[arg(long)]
        force: bool,

        /// Emit machine-readable JSON to stdout instead of human-format text
        #[arg(long)]
        json: bool,
    },

    /// Diagnose the environment and the store, and say what to fix
    Doctor {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,

        /// Emit machine-readable JSON to stdout instead of human-format text
        #[arg(long)]
        json: bool,
    },

    /// Index any codebase via tree-sitter (multi-language, project-isolated)
    Index {
        /// Path to the codebase root (default: the current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        /// Indexing tier: fast (functions/classes only), balanced (+calls),
        /// full (+references). Defaults to tier in .codegraph/config.toml,
        /// then full
        #[arg(long)]
        tier: Option<String>,

        /// Filter to specific languages (comma-separated: rust,python,typescript,go,java,c,cpp)
        #[arg(long, value_delimiter = ',')]
        languages: Option<Vec<String>>,

        /// Force full rebuild, ignoring incremental cache
        #[arg(long)]
        force: bool,

        /// SurrealDB URL. Defaults to an embedded surrealkv file at
        /// .codegraph/graph.db (no server required). Pass ws://... or wss://...
        /// to use a remote SurrealDB instance instead.
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },

    /// Re-run the resolver pass (R1) standalone, without re-indexing.
    /// `codegraph index` already runs this at the end of every run. This
    /// is only for re-resolving already-indexed data (e.g. after a schema
    /// upgrade) without a full re-index.
    Resolve {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },

    /// Generate a session bootstrap context file summarizing the code graph
    Context {
        /// Output path (default: .codegraph/context.md)
        #[arg(long, default_value = ".codegraph/context.md")]
        output: PathBuf,

        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },

    /// Print summary statistics from the codegraph
    Stats {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },

    /// Start MCP server for agent-queryable code graph (stdio transport)
    Serve {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },

    /// Detect structural clones: symbols whose 1-hop neighborhood
    /// fingerprints (see `codegraph index`'s fingerprint pass) are
    /// identical. Same structure, edge types, and node kinds, with names
    /// ignored entirely
    Clones {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        /// Minimum neighborhood size (total incident structural edges) for
        /// a symbol to participate. Below this, "identical structure" is
        /// trivially true of unrelated code and carries no clone signal.
        #[arg(long, default_value = "3")]
        min_edges: usize,

        /// Emit machine-readable JSON (a `Vec<graph::clones::CloneGroup>`)
        /// to stdout instead of the human-format text
        #[arg(long)]
        json: bool,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },

    /// Query the code graph for a project
    Query {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        /// Query type: summary, hubs, coupling, search, calls, deps, rdeps, circular
        #[arg(long, default_value = "summary")]
        kind: String,

        /// Query string (for search, calls, deps, rdeps: the function/type name)
        #[arg(long)]
        name: Option<String>,

        /// Max results to return
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Max depth for traversal queries
        #[arg(long, default_value = "3")]
        depth: usize,

        /// Also surface AMBIGUOUS edges (candidates shown, never blended
        /// into RESOLVED results) for calls/deps/rdeps. Ignored by other
        /// query kinds.
        #[arg(long)]
        include_ambiguous: bool,

        /// Attach a machine-checkable evidence chain to every finding.
        /// Honored by rdeps; ignored by other query kinds. With --json the
        /// chains appear in the "explanations" field
        #[arg(long)]
        explain: bool,

        /// Emit machine-readable JSON to stdout instead of the human-format
        /// text (logs still go to stderr, see `main.rs`). `rdeps` emits a
        /// `codegraph::facade::StructuralVerdict`; every other kind emits
        /// its existing typed result struct as JSON.
        #[arg(long)]
        json: bool,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },

    /// The roadmap as hyperedges over the code graph, read from
    /// .codegraph/planes.yaml
    Plan {
        #[command(subcommand)]
        cmd: PlanCmd,

        #[arg(long, global = true, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        #[arg(long, global = true, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,

        /// Planes file to read (default: .codegraph/planes.yaml under the
        /// repo root). Used by `sync` and `lint`.
        #[arg(long, global = true)]
        planes_file: Option<PathBuf>,

        /// Emit machine-readable JSON to stdout instead of human-format text
        #[arg(long, global = true)]
        json: bool,
    },

    /// Render the hypergraph of the current codebase with the planes
    /// overlaid on it
    Landscape {
        #[arg(long, help = PROJECT_ID_HELP)]
        project_id: Option<String>,

        /// Output format: text, json, mermaid, dot
        #[arg(long, default_value = "text", value_parser = parse_landscape_format)]
        format: LandscapeFormat,

        /// Write the rendering to this file instead of stdout
        #[arg(long)]
        output: Option<PathBuf>,

        #[arg(long, env = "SURREALDB_URL", help = DB_URL_HELP)]
        db_url: Option<String>,
    },
}

/// The `codegraph plan` subcommands. Each one takes the flags declared on
/// `Plan` itself, which are global and so may be written either before or
/// after the subcommand name.
#[derive(Subcommand)]
pub enum PlanCmd {
    /// Ingest the planes file, resolve every touch, and report confidence
    Sync,

    /// List planes and their work items
    List {
        /// Only this plane
        #[arg(long)]
        plane: Option<String>,

        /// Only items in this state: planned, active, done, abandoned
        #[arg(long, value_parser = parse_status)]
        status: Option<Status>,

        /// Only planes at this horizon: now, next, later
        #[arg(long, value_parser = parse_horizon)]
        horizon: Option<Horizon>,
    },

    /// Show one work item: its touches with confidence, its dependencies,
    /// and how far it reaches
    Show {
        /// Work item id, as written in the planes file
        id: String,
    },

    /// Inverse incidence: what planned work covers this file or symbol
    Touching {
        /// A repo-relative file path or a symbol name
        target: String,
    },

    /// Active work items whose touch sets intersect
    Collisions,

    /// Work items whose touches no longer bind to live code
    Stale,

    /// Reverse dependencies over the union of a work item's touch set
    Blast {
        /// Work item id, as written in the planes file
        id: String,

        /// Max traversal depth
        #[arg(long, default_value = "3")]
        depth: usize,
    },

    /// Check the planes file against the schema rules without touching the
    /// store
    Lint,

    /// Compact, agent-pasteable markdown summary of the landscape
    Brief,
}

/// clap value parser for `--status`, so an unknown value is rejected at
/// parse time with the accepted list attached.
fn parse_status(raw: &str) -> Result<Status, String> {
    Status::from_str(raw)
}

/// clap value parser for `--horizon`.
fn parse_horizon(raw: &str) -> Result<Horizon, String> {
    Horizon::from_str(raw)
}

/// clap value parser for `landscape --format`.
fn parse_landscape_format(raw: &str) -> Result<LandscapeFormat, String> {
    LandscapeFormat::from_str(raw)
}
