//! Python AST extractor.
//!
//! Extracts: functions, classes, methods, imports, decorated definitions.

use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

use super::super::parser::ExtractionContext;
use super::super::IndexingTier;

// ============================================================================
// Public extract entry point
// ============================================================================

pub fn extract(ctx: &mut ExtractionContext, root: Node) {
    walk(ctx, root, None);
}

// ============================================================================
// AST walk
// ============================================================================

fn walk(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    match node.kind() {
        "decorated_definition" => extract_decorated_definition(ctx, node, enclosing_id),
        "function_definition" => {
            extract_function(ctx, node, enclosing_id);
        }
        "class_definition" => {
            extract_class(ctx, node, enclosing_id);
        }
        "import_statement" | "import_from_statement" => extract_import(ctx, node),
        _ => recurse(ctx, node, enclosing_id),
    }
}

fn recurse(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk(ctx, cursor.node(), enclosing_id);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// ============================================================================
// decorated_definition handler
// ============================================================================

/// Handle a `decorated_definition` node: one or more `decorator` nodes
/// followed by the decorated `function_definition` or `class_definition`.
/// Decorators themselves are not modeled as nodes/edges — only the
/// underlying function/class declaration is extracted.
fn extract_decorated_definition(
    ctx: &mut ExtractionContext,
    node: Node,
    enclosing_id: Option<&str>,
) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "function_definition" => {
                    extract_function(ctx, child, enclosing_id);
                }
                "class_definition" => {
                    extract_class(ctx, child, enclosing_id);
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// ============================================================================
// Function and class extraction
// ============================================================================

/// Extract a `function_definition` node. Returns the generated node_id.
fn extract_function(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) -> String {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() || (name.starts_with('_') && name != "__init__") {
        return String::new();
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    let source = ctx.node_text(node);

    // Check for async
    if source.starts_with("async ") {
        meta.insert("async".into(), json!(true));
    }

    // Extract parameters
    if let Some(params) = node.child_by_field_name("parameters") {
        meta.insert("parameters".into(), json!(ctx.node_text(params)));
    }

    // Extract return type annotation
    if let Some(ret) = node.child_by_field_name("return_type") {
        meta.insert("return_type".into(), json!(ctx.node_text(ret)));
    }

    let content = truncate(source, 2000);
    let fn_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &fn_id, "contains", "EXTRACTED");
    }

    // Extract calls from body
    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls(ctx, body, &fn_id);
        }
    }

    fn_id
}

/// Extract a `class_definition` node. Returns the generated node_id.
fn extract_class(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) -> String {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    // Check for base classes
    if let Some(superclasses) = node.child_by_field_name("superclasses") {
        let bases = ctx.node_text(superclasses);
        meta.insert("bases".into(), json!(bases));
    }

    let class_id = ctx.add_node(&name, "class", Some(start), Some(end), None, meta);

    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &class_id, "contains", "EXTRACTED");
    }

    // Extract methods from body
    if let Some(body) = node.child_by_field_name("body") {
        walk(ctx, body, Some(&class_id));
    }

    class_id
}

fn extract_import(ctx: &mut ExtractionContext, node: Node) {
    let source = ctx.node_text(node).to_string();
    let start = node.start_position().row as u32 + 1;

    let mut meta = HashMap::new();
    meta.insert("raw".into(), json!(source));

    ctx.add_node(&source, "import", Some(start), None, None, meta);
}

// ============================================================================
// Call extraction
// ============================================================================

fn extract_calls(ctx: &mut ExtractionContext, node: Node, caller_id: &str) {
    let mut cursor = node.walk();
    walk_calls(ctx, &mut cursor, caller_id);
}

fn walk_calls(ctx: &mut ExtractionContext, cursor: &mut tree_sitter::TreeCursor, caller_id: &str) {
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "call" {
            if let Some(func) = node.child_by_field_name("function") {
                let callee = ctx.node_text(func).to_string();
                if !callee.is_empty() && callee.len() < 100 {
                    ctx.add_name_edge(caller_id, &callee, "function", "calls", "INFERRED");
                }
            }
        }

        walk_calls(ctx, cursor, caller_id);

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    cursor.goto_parent();
}

// ============================================================================
// Shared utilities
// ============================================================================

fn child_text(ctx: &ExtractionContext, node: Node, kind: &str) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return Some(ctx.node_text(cursor.node()).to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> Option<String> {
    if s.len() <= max {
        return Some(s.to_string());
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Some(s[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::qualified_name;

    fn extract_source(file_path: &str, source: &str) -> Vec<super::super::super::parser::CodeNode> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("load python grammar");
        let tree = parser.parse(source, None).expect("parse python source");

        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: file_path.to_string(),
            language: "python".to_string(),
            source: source.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };

        extract(&mut ctx, tree.root_node());

        let module_path = qualified_name::module_path(file_path, "python");
        qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

        ctx.nodes
    }

    #[test]
    fn top_level_function_qualified_name() {
        let src = "def helper():\n    pass\n";
        let nodes = extract_source("mypkg/utils/helpers.py", src);
        let f = nodes
            .iter()
            .find(|n| n.name == "helper")
            .expect("helper node");
        assert_eq!(f.qualified_name, "mypkg::utils::helpers::helper");
    }

    #[test]
    fn class_method_qualified_name() {
        let src = "class Handler:\n    def serve(self):\n        pass\n";
        let nodes = extract_source("mypkg/server.py", src);
        let class = nodes
            .iter()
            .find(|n| n.name == "Handler")
            .expect("Handler node");
        let method = nodes
            .iter()
            .find(|n| n.name == "serve")
            .expect("serve node");
        assert_eq!(class.qualified_name, "mypkg::server::Handler");
        assert_eq!(method.qualified_name, "mypkg::server::Handler::serve");
    }

    #[test]
    fn init_py_names_its_own_package() {
        let src = "def helper():\n    pass\n";
        let nodes = extract_source("mypkg/__init__.py", src);
        let f = nodes
            .iter()
            .find(|n| n.name == "helper")
            .expect("helper node");
        assert_eq!(f.qualified_name, "mypkg::helper");
    }
}
