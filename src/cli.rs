use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "codegraph")]
#[command(about = "Language-agnostic codebase graph indexer, backed by SurrealDB")]
pub struct Cli {
    #[arg(short, long)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Index any codebase via tree-sitter (multi-language, project-isolated)
    Index {
        /// Path to the codebase root
        path: PathBuf,

        /// Project ID for multi-tenant isolation (required)
        #[arg(long)]
        project_id: String,

        /// Indexing tier: fast (functions/classes only), balanced (+calls), full (+references)
        #[arg(long, default_value = "full")]
        tier: String,

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
    /// `codegraph index` already runs this at the end of every run — this
    /// is only for re-resolving already-indexed data (e.g. after a schema
    /// upgrade) without a full re-index.
    Resolve {
        /// Project ID to resolve (required)
        #[arg(long)]
        project_id: String,

        /// SurrealDB URL (default: embedded .codegraph/graph.db)
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },

    /// Generate a session bootstrap context file summarizing the code graph
    Context {
        /// Output path (default: .codegraph/context.md)
        #[arg(long, default_value = ".codegraph/context.md")]
        output: PathBuf,

        /// Project ID to summarize (required)
        #[arg(long)]
        project_id: String,

        /// SurrealDB URL (default: embedded .codegraph/graph.db)
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },

    /// Print summary statistics from the codegraph
    Stats {
        /// Project ID to summarize (required)
        #[arg(long)]
        project_id: String,

        /// SurrealDB URL (default: embedded .codegraph/graph.db)
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },

    /// Start MCP server for agent-queryable code graph (stdio transport)
    Serve {
        /// Project ID to serve
        #[arg(long)]
        project_id: String,

        /// SurrealDB URL (default: embedded .codegraph/graph.db)
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },

    /// Detect structural clones: symbols whose 1-hop neighborhood
    /// fingerprints (see `codegraph index`'s fingerprint pass) are
    /// identical — same structure, edge types, and node kinds, names
    /// ignored entirely
    Clones {
        /// Project ID to scan (required)
        #[arg(long)]
        project_id: String,

        /// Minimum neighborhood size (total incident structural edges) for
        /// a symbol to participate. Below this, "identical structure" is
        /// trivially true of unrelated code and carries no clone signal.
        #[arg(long, default_value = "3")]
        min_edges: usize,

        /// Emit machine-readable JSON (a `Vec<graph::clones::CloneGroup>`)
        /// to stdout instead of the human-format text
        #[arg(long)]
        json: bool,

        /// SurrealDB URL (default: embedded .codegraph/graph.db)
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },

    /// Query the code graph for a project
    Query {
        /// Project ID to query
        #[arg(long)]
        project_id: String,

        /// Query type: summary, hubs, coupling, search, calls, deps, rdeps, circular
        #[arg(long, default_value = "summary")]
        kind: String,

        /// Query string (for search, calls, deps, rdeps — the function/type name)
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

        /// Emit machine-readable JSON to stdout instead of the human-format
        /// text (logs still go to stderr — see `main.rs`). `rdeps` emits a
        /// `codegraph::facade::StructuralVerdict`; every other kind emits
        /// its existing typed result struct as JSON.
        #[arg(long)]
        json: bool,

        /// SurrealDB URL (default: embedded .codegraph/graph.db)
        #[arg(long, env = "SURREALDB_URL")]
        db_url: Option<String>,
    },
}
