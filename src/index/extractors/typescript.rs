//! TypeScript / JavaScript AST extractor.
//!
//! Extracts: functions, classes, interfaces, type aliases, imports, exports,
//! method definitions, arrow functions (named only).

use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

use super::super::parser::ExtractionContext;
use super::super::IndexingTier;

pub fn extract(ctx: &mut ExtractionContext, root: Node) {
    walk(ctx, root);
}

fn walk(ctx: &mut ExtractionContext, node: Node) {
    match node.kind() {
        "function_declaration" => extract_function(ctx, node, None),
        "class_declaration" => extract_class(ctx, node),
        "interface_declaration" => extract_interface(ctx, node),
        "type_alias_declaration" => extract_type_alias(ctx, node),
        "import_statement" => extract_import(ctx, node),
        "export_statement" => {
            extract_export(ctx, node);
            return;
        }
        "lexical_declaration" | "variable_declaration" => {
            // Check for `const foo = () => {}` arrow function patterns
            extract_named_arrows(ctx, node);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk(ctx, cursor.node());
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn extract_function(ctx: &mut ExtractionContext, node: Node, class_id: Option<&str>) {
    let name = child_text(ctx, node, "identifier")
        .or_else(|| child_text(ctx, node, "property_identifier"))
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    let source = ctx.node_text(node);
    if source.contains("async ") {
        meta.insert("async".into(), json!(true));
    }
    if source.starts_with("export ") {
        meta.insert("exported".into(), json!(true));
    }

    let content = truncate(source, 2000);
    let fn_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    if let Some(parent_id) = class_id {
        ctx.add_edge(parent_id, &fn_id, "contains", "EXTRACTED");
    }

    // Extract calls from body
    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls_from(ctx, body, &fn_id);
        }
    }
}

fn extract_class(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "type_identifier")
        .or_else(|| child_text(ctx, node, "identifier"))
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    // Check for exports
    let source = ctx.node_text(node);
    if source.starts_with("export ") {
        meta.insert("exported".into(), json!(true));
    }

    let class_id = ctx.add_node(&name, "class", Some(start), Some(end), None, meta);

    // Extract methods
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "method_definition" || child.kind() == "public_field_definition"
                {
                    extract_method(ctx, child, &class_id);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

fn extract_method(ctx: &mut ExtractionContext, node: Node, class_id: &str) -> String {
    let name = child_text(ctx, node, "property_identifier")
        .or_else(|| child_text(ctx, node, "identifier"))
        .unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let meta = HashMap::new();

    let source = ctx.node_text(node);
    let content = truncate(source, 2000);

    let method_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);
    ctx.add_edge(class_id, &method_id, "contains", "EXTRACTED");
    method_id
}

fn extract_interface(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "type_identifier")
        .or_else(|| child_text(ctx, node, "identifier"))
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();

    // Collect property signatures
    let mut properties = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "property_signature" || child.kind() == "method_signature" {
                    if let Some(pname) = child_text(ctx, child, "property_identifier") {
                        properties.push(pname);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    if !properties.is_empty() {
        meta.insert("properties".into(), json!(properties));
    }

    ctx.add_node(&name, "interface", Some(start), Some(end), None, meta);
}

fn extract_type_alias(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "type_identifier")
        .or_else(|| child_text(ctx, node, "identifier"))
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let source = ctx.node_text(node);
    let content = truncate(source, 500);

    ctx.add_node(
        &name,
        "type_alias",
        Some(start),
        None,
        content,
        HashMap::new(),
    );
}

fn extract_import(ctx: &mut ExtractionContext, node: Node) {
    let source = ctx.node_text(node).to_string();
    let start = node.start_position().row as u32 + 1;

    let mut meta = HashMap::new();
    meta.insert("raw".into(), json!(source));

    ctx.add_node(&source, "import", Some(start), None, None, meta);
}

fn extract_export(ctx: &mut ExtractionContext, node: Node) {
    // For re-exports like `export { foo } from './bar'`
    let source_text = ctx.node_text(node).to_string();
    if source_text.contains(" from ") {
        let start = node.start_position().row as u32 + 1;
        let mut meta = HashMap::new();
        meta.insert("raw".into(), json!(&source_text));
        ctx.add_node(&source_text, "import", Some(start), None, None, meta);
    }

    // The export_statement owns extraction of its inner declaration so it
    // isn't walked (and extracted) a second time by the generic recursion.
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "class_declaration" => {
                    extract_class(ctx, child);
                }
                "function_declaration" => {
                    extract_function(ctx, child, None);
                }
                "interface_declaration" => {
                    extract_interface(ctx, child);
                }
                "type_alias_declaration" => {
                    extract_type_alias(ctx, child);
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn extract_named_arrows(ctx: &mut ExtractionContext, node: Node) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "variable_declarator" {
                let name = child_text(ctx, child, "identifier").unwrap_or_default();
                if name.is_empty() {
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                }

                // Check if the value is an arrow function
                if let Some(value) = child.child_by_field_name("value") {
                    if value.kind() == "arrow_function" {
                        let start = node.start_position().row as u32 + 1;
                        let end = value.end_position().row as u32 + 1;
                        let source = ctx.node_text(node);
                        let content = truncate(source, 2000);
                        ctx.add_node(
                            &name,
                            "function",
                            Some(start),
                            Some(end),
                            content,
                            HashMap::new(),
                        );
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn extract_calls_from(ctx: &mut ExtractionContext, node: Node, caller_id: &str) {
    let mut cursor = node.walk();
    walk_calls(ctx, &mut cursor, caller_id);
}

fn walk_calls(ctx: &mut ExtractionContext, cursor: &mut tree_sitter::TreeCursor, caller_id: &str) {
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "call_expression" {
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
    child_text_from_source(node, ctx.source.as_bytes(), kind)
}

/// Find the first child of `node` with `kind` and return its text, using
/// raw source bytes.  Separated from `child_text` so it can be called without
/// an `&ExtractionContext` borrow.
fn child_text_from_source(node: Node, source: &[u8], kind: &str) -> Option<String> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return Some(cursor.node().utf8_text(source).unwrap_or("").to_string());
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
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .expect("load typescript grammar");
        let tree = parser.parse(source, None).expect("parse typescript source");

        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: file_path.to_string(),
            language: "typescript".to_string(),
            source: source.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };

        extract(&mut ctx, tree.root_node());

        let module_path = qualified_name::module_path(file_path, "typescript");
        qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

        ctx.nodes
    }

    #[test]
    fn top_level_function_qualified_name() {
        let src = "function run() {}\n";
        let nodes = extract_source("src/app.ts", src);
        let f = nodes.iter().find(|n| n.name == "run").expect("run node");
        assert_eq!(f.qualified_name, "src::app::run");
    }

    #[test]
    fn class_method_qualified_name() {
        let src = "class Handler {\n    serve() {}\n}\n";
        let nodes = extract_source("src/server/handler.ts", src);
        let class = nodes
            .iter()
            .find(|n| n.name == "Handler")
            .expect("Handler node");
        let method = nodes
            .iter()
            .find(|n| n.name == "serve")
            .expect("serve node");
        // Unlike Go/Java, TS keeps the filename segment (`handler`) since it
        // allows genuine top-level, container-less functions too.
        assert_eq!(class.qualified_name, "src::server::handler::Handler");
        assert_eq!(method.qualified_name, "src::server::handler::Handler::serve");
    }
}
