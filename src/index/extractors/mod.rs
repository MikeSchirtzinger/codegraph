//! Per-language AST extractors.
//!
//! Each extractor walks a tree-sitter parse tree and emits CodeNode + CodeEdge
//! records via the shared ExtractionContext.

pub mod c_cpp;
pub mod go;
pub mod java;
pub mod python;
pub mod rust;
pub mod typescript;
