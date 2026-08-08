//! codegraph — language-agnostic codebase graph indexer, backed by SurrealDB.
//!
//! Re-exports the modules needed by integration tests and by the MCP server.

pub mod canon;
pub mod db;
pub mod facade;
pub mod graph;
pub mod index;
pub mod mcp;
