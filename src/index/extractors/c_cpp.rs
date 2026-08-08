//! C / C++ AST extractor.
//!
//! Extracts: functions, structs, enums, classes (C++), includes, typedefs.

use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

use super::super::parser::ExtractionContext;
use super::super::IndexingTier;

pub fn extract(ctx: &mut ExtractionContext, root: Node) {
    walk(ctx, root, None);
}

fn walk(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    match node.kind() {
        "function_definition" => {
            extract_function(ctx, node, enclosing_id);
        }
        "function_declarator" => {}
        "struct_specifier" => extract_struct(ctx, node),
        "enum_specifier" => extract_enum(ctx, node),
        "class_specifier" => extract_class(ctx, node),
        "preproc_include" => extract_include(ctx, node),
        "type_definition" => extract_typedef(ctx, node),
        _ => {}
    }

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

fn extract_function(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    // The function name is inside the declarator
    let name = find_function_name(ctx, node).unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    // Extract return type
    if let Some(ret) = node.child_by_field_name("type") {
        meta.insert("return_type".into(), json!(ctx.node_text(ret)));
    }

    let source = ctx.node_text(node);
    let content = truncate(source, 2000);
    let fn_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &fn_id, "contains", "EXTRACTED");
    }

    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls(ctx, body, &fn_id);
        }
    }
}

fn extract_struct(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "type_identifier")
        .or_else(|| child_text(ctx, node, "identifier"))
        .unwrap_or_default();

    // Anonymous structs — skip
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    // Collect fields
    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "field_declaration" {
                    if let Some(fname) = child_text(ctx, child, "field_identifier") {
                        fields.push(fname);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    if !fields.is_empty() {
        meta.insert("fields".into(), json!(fields));
    }

    let source = ctx.node_text(node);
    let content = truncate(source, 2000);
    ctx.add_node(&name, "struct", Some(start), Some(end), content, meta);
}

fn extract_enum(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "type_identifier")
        .or_else(|| child_text(ctx, node, "identifier"))
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    let mut values = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "enumerator" {
                    if let Some(ename) = child_text(ctx, child, "identifier") {
                        values.push(ename);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    if !values.is_empty() {
        meta.insert("values".into(), json!(values));
    }

    ctx.add_node(&name, "enum", Some(start), Some(end), None, meta);
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
    let meta = HashMap::new();

    let class_id = ctx.add_node(&name, "class", Some(start), Some(end), None, meta);

    // Extract methods from body
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "function_definition" {
                    extract_function(ctx, child, Some(&class_id));
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

fn extract_include(ctx: &mut ExtractionContext, node: Node) {
    let source = ctx.node_text(node).to_string();
    let start = node.start_position().row as u32 + 1;

    let mut meta = HashMap::new();
    meta.insert("raw".into(), json!(source));

    ctx.add_node(&source, "import", Some(start), None, None, meta);
}

fn extract_typedef(ctx: &mut ExtractionContext, node: Node) {
    let source = ctx.node_text(node).to_string();
    let start = node.start_position().row as u32 + 1;

    // Try to extract the new type name (last identifier before semicolon)
    let name = source
        .trim_end_matches(';')
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim_start_matches('*')
        .to_string();

    if name.is_empty() {
        return;
    }

    let content = truncate(&source, 500);
    ctx.add_node(
        &name,
        "type_alias",
        Some(start),
        None,
        content,
        HashMap::new(),
    );
}

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

/// Navigate into function declarators to find the function name.
fn find_function_name(ctx: &ExtractionContext, node: Node) -> Option<String> {
    // Look for declarator field, then find identifier within it
    if let Some(declarator) = node.child_by_field_name("declarator") {
        // Could be function_declarator or pointer_declarator wrapping one
        return find_identifier_in(ctx, declarator);
    }
    None
}

fn find_identifier_in(ctx: &ExtractionContext, node: Node) -> Option<String> {
    if node.kind() == "identifier" || node.kind() == "field_identifier" {
        return Some(ctx.node_text(node).to_string());
    }

    // Look in declarator field first (for nested declarators)
    if let Some(inner) = node.child_by_field_name("declarator") {
        return find_identifier_in(ctx, inner);
    }

    // Fall back to first child scan
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if let Some(name) = find_identifier_in(ctx, cursor.node()) {
                return Some(name);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    None
}

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

    fn extract_source(
        file_path: &str,
        language: &str,
        ts_language: tree_sitter::Language,
        source: &str,
    ) -> Vec<super::super::super::parser::CodeNode> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_language).expect("load grammar");
        let tree = parser.parse(source, None).expect("parse source");

        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: file_path.to_string(),
            language: language.to_string(),
            source: source.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };

        extract(&mut ctx, tree.root_node());

        let module_path = qualified_name::module_path(file_path, language);
        qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

        ctx.nodes
    }

    #[test]
    fn c_top_level_function_qualified_name() {
        let src = "int add(int a, int b) { return a + b; }\n";
        let nodes = extract_source("lib/util.c", "c", tree_sitter_c::LANGUAGE.into(), src);
        let f = nodes.iter().find(|n| n.name == "add").expect("add node");
        assert_eq!(f.qualified_name, "lib::util::add");
    }

    #[test]
    fn cpp_class_method_qualified_name() {
        let src = "class Handler {\n    void serve() {}\n};\n";
        let nodes = extract_source(
            "lib/handler.cpp",
            "cpp",
            tree_sitter_cpp::LANGUAGE.into(),
            src,
        );
        let class = nodes
            .iter()
            .find(|n| n.name == "Handler")
            .expect("Handler node");
        let method = nodes
            .iter()
            .find(|n| n.name == "serve")
            .expect("serve node");
        assert_eq!(class.qualified_name, "lib::handler::Handler");
        assert_eq!(method.qualified_name, "lib::handler::Handler::serve");
    }
}
