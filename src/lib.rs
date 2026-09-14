//! codegraph — language-agnostic codebase graph indexer, backed by SurrealDB.
//!
//! Every module lives here, including the three that only the binary calls
//! (`cli`, `context`, `stats`). That is deliberate. `src/main.rs` used to
//! re-declare the module tree with its own `mod canon; mod db; mod graph;
//! ...`, which compiled all of it a second time inside the binary crate,
//! where `crate::` means the binary. A module written correctly against the
//! library (`crate::landscape`, `crate::plan`, `crate::facade`) then failed
//! to compile in that second copy, and the two copies produced
//! *incompatible types for the same struct*, which `src/main.rs` was already
//! working around by hand for `graph::dependencies::DependencyResult`.
//!
//! One crate owns the module tree. The binary is a thin `use codegraph::…`
//! over it, so `crate::` has exactly one meaning everywhere and there is
//! only ever one `DependencyResult`.

pub mod canon;
pub mod cli;
pub mod config;
pub mod context;
pub mod db;
pub mod doctor;
pub mod facade;
pub mod graph;
pub mod index;
pub mod init;
pub mod landscape;
pub mod mcp;
pub mod plan;
pub mod stats;
