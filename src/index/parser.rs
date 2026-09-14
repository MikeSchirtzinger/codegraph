//! Tree-sitter parsing dispatcher.
//!
//! Routes source files to language-specific extractors based on file extension.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use tree_sitter::Parser;

use super::extractors;
use super::node_id::generate_node_id;
use super::qualified_name;
use super::IndexingTier;

/// A code entity extracted from source.
#[derive(Debug, Clone, Serialize)]
pub struct CodeNode {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub language: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub content: Option<String>,
    pub metadata: HashMap<String, serde_json::Value>,
    /// Language-mechanical module path + containment chain + name, `::`
    /// separated (e.g. `graph::dependencies::resolve_deps`). Populated by
    /// `qualified_name::assign` after extraction — see `parse_file` below —
    /// so it's always empty on a `CodeNode` fresh out of `add_node`.
    pub qualified_name: String,
}

/// A relationship between two code nodes.
#[derive(Debug, Clone, Serialize)]
pub struct CodeEdge {
    pub from_id: String,
    /// Real node_id when known. When the target had to be looked up by name
    /// (e.g. a call to a function defined elsewhere), this is an empty-string
    /// placeholder and `to_name`/`to_type` carry the info needed to resolve
    /// it in the post-indexing resolution pass (see `index::mod::resolve_name_edges`).
    pub to_id: String,
    /// Set only for name-based (unresolved) edges. `None` once/if the edge's
    /// `to_id` is a real, directly-known node id.
    pub to_name: Option<String>,
    pub to_type: Option<String>,
    pub edge_type: String,
    /// EXTRACTED = directly from AST, INFERRED = resolved, AMBIGUOUS = heuristic
    pub confidence: String,
}

/// Result of parsing a single file.
#[derive(Debug)]
pub struct ParsedFile {
    pub language: String,
    pub nodes: Vec<CodeNode>,
    pub edges: Vec<CodeEdge>,
}

/// Map file extension to language name.
pub fn extension_to_language(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => Some("cpp"),
        _ => None,
    }
}

/// Parse a single source file and extract nodes + edges.
pub fn parse_file(
    abs_path: &Path,
    rel_path: &str,
    project_id: &str,
    tier: IndexingTier,
) -> Result<ParsedFile> {
    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let language =
        extension_to_language(ext).with_context(|| format!("unsupported extension: {ext}"))?;

    let source = std::fs::read_to_string(abs_path)
        .with_context(|| format!("cannot read: {}", abs_path.display()))?;

    let mut parser = Parser::new();

    // Select the tree-sitter grammar
    let ts_language = match language {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "typescript" => {
            if ext == "tsx" {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        // `.jsx` carries JSX elements, and the plain TypeScript grammar
        // parses `<div>` as a comparison against a type argument: the
        // component's body becomes an error node and every function inside
        // it disappears from the graph. The TSX grammar is the same
        // language with JSX enabled, which is what the `.tsx` arm above
        // already relies on.
        //
        // `.mjs` and `.cjs` stay on the plain grammar deliberately. They are
        // ordinary JavaScript modules, JavaScript is a subset of TypeScript,
        // and neither extension is conventionally JSX-bearing. Routing them
        // through TSX would only widen the grammar for no gain.
        "javascript" => {
            if ext == "jsx" {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        "python" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        _ => anyhow::bail!("no tree-sitter grammar for: {language}"),
    };

    parser
        .set_language(&ts_language)
        .with_context(|| format!("failed to set language: {language}"))?;

    let tree = parser
        .parse(&source, None)
        .with_context(|| format!("tree-sitter parse failed: {rel_path}"))?;

    let root = tree.root_node();

    // Extract using the language-specific extractor
    let mut ctx = ExtractionContext {
        project_id: project_id.to_string(),
        file_path: rel_path.to_string(),
        language: language.to_string(),
        source: source.clone(),
        tier,
        nodes: Vec::new(),
        edges: Vec::new(),
    };

    match language {
        "rust" => extractors::rust::extract(&mut ctx, root),
        "typescript" | "javascript" => extractors::typescript::extract(&mut ctx, root),
        "python" => extractors::python::extract(&mut ctx, root),
        "go" => extractors::go::extract(&mut ctx, root),
        "java" => extractors::java::extract(&mut ctx, root),
        "c" | "cpp" => extractors::c_cpp::extract(&mut ctx, root),
        _ => tracing::warn!("no extractor for {language}, skipping"),
    }

    // Language-agnostic pass: derive qualified_name for every node in this
    // file from its module path (mechanical, from rel_path) plus whatever
    // `contains` containment chain the extractor above emitted.
    let module_path = qualified_name::module_path(rel_path, language);
    qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

    Ok(ParsedFile {
        language: language.to_string(),
        nodes: ctx.nodes,
        edges: ctx.edges,
    })
}

/// Shared context passed to all extractors.
pub struct ExtractionContext {
    pub project_id: String,
    pub file_path: String,
    pub language: String,
    pub source: String,
    pub tier: IndexingTier,
    pub nodes: Vec<CodeNode>,
    pub edges: Vec<CodeEdge>,
}

impl ExtractionContext {
    /// Add a node and return its deterministic ID.
    pub fn add_node(
        &mut self,
        name: &str,
        node_type: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
        content: Option<String>,
        metadata: HashMap<String, serde_json::Value>,
    ) -> String {
        let id = generate_node_id(
            &self.project_id,
            &self.file_path,
            name,
            node_type,
            start_line,
        );

        self.nodes.push(CodeNode {
            id: id.clone(),
            name: name.to_string(),
            node_type: node_type.to_string(),
            language: self.language.clone(),
            start_line: start_line.map(|l| l as i64),
            end_line: end_line.map(|l| l as i64),
            content,
            metadata,
            // Filled in by `qualified_name::assign` once the whole file has
            // been extracted (see `parse_file`) — containment chains aren't
            // known until all of this file's `contains` edges exist.
            qualified_name: String::new(),
        });

        id
    }

    /// Add an edge between two nodes whose real IDs are both already known
    /// (e.g. structural containment within the same file).
    pub fn add_edge(&mut self, from_id: &str, to_id: &str, edge_type: &str, confidence: &str) {
        self.edges.push(CodeEdge {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            to_name: None,
            to_type: None,
            edge_type: edge_type.to_string(),
            confidence: confidence.to_string(),
        });
    }

    /// Add an edge whose target is only known by name (e.g. a call to
    /// `some_function()`, or an `impl X for Trait` — the target may live in
    /// a different file than we're currently parsing, so its real node_id
    /// can't be computed by hashing here; `generate_node_id` would silently
    /// produce a hash that never matches the real target's id).
    ///
    /// `to_id` is left as an empty-string placeholder. After all files in
    /// the project are indexed, `index::resolve_name_edges` runs a single
    /// pass that looks up `(project_id, name=to_name, node_type=to_type)`
    /// against the real `code_node` table and fills in `to_id`. If no match
    /// exists (external/std-lib symbol, or genuinely unresolved), the
    /// placeholder is left in place.
    ///
    /// **Multi-line captures are dropped here, at the one choke point every
    /// extractor goes through.** A name is a single-line token in all six
    /// languages; a `to_name` carrying a newline is never one — it is the
    /// raw source text of a wrapped receiver chain that the call-expression
    /// capture swallowed whole, e.g.
    /// `"nodes\n    .iter()\n    .find"` or a whole multi-line
    /// `db.query(\"…\")\n    .bind` builder. Measured self-indexing this
    /// repo: 759 such captures, every one of them `calls`/`UNRESOLVED` and
    /// unresolvable by construction (no symbol has that name), while their
    /// normalized bare tail (`find`, `bind`, `context`) collides freely with
    /// real project symbol names — which is exactly what
    /// `dependencies::find_stale_references` then has to reason about. Junk
    /// in, junk out: they are dropped rather than stored and filtered
    /// downstream, so no query has to know about them.
    ///
    /// Resolved-binding data is unaffected by construction: these captures
    /// never bind (`to_id` stays `""`, confidence `UNRESOLVED`), so no
    /// `RESOLVED` edge, `file_ref`, or fingerprint input disappears with
    /// them — only the unresolved noise floor drops.
    pub fn add_name_edge(
        &mut self,
        from_id: &str,
        to_name: &str,
        to_type: &str,
        edge_type: &str,
        confidence: &str,
    ) {
        if to_name.contains('\n') || to_name.contains('\r') {
            return;
        }
        self.edges.push(CodeEdge {
            from_id: from_id.to_string(),
            to_id: String::new(),
            to_name: Some(to_name.to_string()),
            to_type: Some(to_type.to_string()),
            edge_type: edge_type.to_string(),
            confidence: confidence.to_string(),
        });
    }

    /// Get source text for a tree-sitter node.
    pub fn node_text(&self, node: tree_sitter::Node) -> &str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }
}
