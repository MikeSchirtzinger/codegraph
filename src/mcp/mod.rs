//! MCP server for agent-queryable code graphs.
//!
//! Exposes the graph query tools via the Model Context Protocol (stdio transport),
//! allowing AI agents to query code structure, dependencies, and metrics on demand.

pub mod server;
