//! Go AST extractor.
//!
//! Extracts: functions, methods (receiver), structs, interfaces, imports, type declarations.

use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

use super::super::parser::ExtractionContext;
use super::super::IndexingTier;

// ============================================================================
// Public entry point
// ============================================================================

pub fn extract(ctx: &mut ExtractionContext, root: Node) {
    // Two passes. Unlike Rust's `impl` blocks or a TS/Python/Java/C++
    // class, a Go method isn't lexically nested inside its receiver type —
    // both are top-level declarations linked only by the receiver's type
    // name, in any order. Collecting every type declaration first (with
    // real node ids) lets same-file methods below get genuine `contains`
    // containment instead of always falling back to a name-only edge —
    // without this, Go emitted zero `contains` edges at all and every Go
    // qualified_name was flat (missing e.g. `Handler::` in
    // `pkg::server::Handler::serve`).
    let mut types: HashMap<String, String> = HashMap::new();
    collect_types(ctx, root, &mut types);
    walk(ctx, root, &types);
}

/// Pass 1: extract every type declaration (struct/interface/type alias) in
/// the file, recording `name -> node_id` so pass 2 can link same-file
/// methods to their receiver type by a real id, not just by name.
fn collect_types(ctx: &mut ExtractionContext, node: Node, types: &mut HashMap<String, String>) {
    if node.kind() == "type_declaration" {
        extract_type_decl(ctx, node, types);
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_types(ctx, cursor.node(), types);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Pass 2: functions, methods, imports. Type declarations were already
/// extracted in `collect_types` above, so `type_declaration` is skipped
/// here (its children are still walked, harmlessly — none match below).
fn walk(ctx: &mut ExtractionContext, node: Node, types: &HashMap<String, String>) {
    match node.kind() {
        "function_declaration" => extract_function(ctx, node),
        "method_declaration" => extract_method(ctx, node, types),
        "import_declaration" => extract_imports(ctx, node),
        _ => {}
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk(ctx, cursor.node(), types);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// ============================================================================
// Declaration extractors
// ============================================================================

fn extract_function(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    // In Go, exported = capitalized first letter
    if name
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        meta.insert("exported".into(), json!(true));
    }

    if let Some(params) = node.child_by_field_name("parameters") {
        meta.insert("parameters".into(), json!(ctx.node_text(params)));
    }

    if let Some(result) = node.child_by_field_name("result") {
        meta.insert("return_type".into(), json!(ctx.node_text(result)));
    }

    let source = ctx.node_text(node);
    let content = truncate(source, 2000);
    let fn_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls(ctx, body, &fn_id);
        }
    }
}

fn extract_method(ctx: &mut ExtractionContext, node: Node, types: &HashMap<String, String>) {
    let name = child_text(ctx, node, "field_identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();
    let mut receiver_type: Option<String> = None;

    // Extract receiver type
    if let Some(receiver) = node.child_by_field_name("receiver") {
        let recv_text = ctx.node_text(receiver);
        meta.insert("receiver".into(), json!(recv_text));

        // Try to extract the type name from receiver
        // Pattern: (r *TypeName) or (r TypeName)
        let recv_clean = recv_text.trim_matches(|c| c == '(' || c == ')').trim();
        let type_name = recv_clean
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim_start_matches('*');

        if !type_name.is_empty() {
            meta.insert("method_of".into(), json!(type_name));
            receiver_type = Some(type_name.to_string());
        }
    }

    if name
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        meta.insert("exported".into(), json!(true));
    }

    let source = ctx.node_text(node);
    let content = truncate(source, 2000);

    let method_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    if let Some(type_name) = receiver_type {
        if let Some(type_id) = types.get(&type_name) {
            // Receiver type declared in this same file: its node id is
            // already known, so this is genuine containment — the same
            // relationship a Rust `impl` block or a TS/Python/Java/C++
            // class has to its methods.
            ctx.add_edge(type_id, &method_id, "contains", "EXTRACTED");
        } else {
            // Receiver type lives in another file within the same package
            // — its real node_id can't be computed from its name alone
            // here, so it's resolved by name post-indexing instead. Edge
            // points method -> struct (reverse of "contains") since the
            // method's id is the only one we actually know at this point.
            ctx.add_name_edge(&method_id, &type_name, "struct", "member_of", "INFERRED");
        }
    }

    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls(ctx, body, &method_id);
        }
    }
}

fn extract_type_decl(ctx: &mut ExtractionContext, node: Node, types: &mut HashMap<String, String>) {
    // type_declaration contains type_spec children
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "type_spec" {
                extract_type_spec(ctx, child, types);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn extract_type_spec(ctx: &mut ExtractionContext, node: Node, types: &mut HashMap<String, String>) {
    let name = child_text(ctx, node, "type_identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    if name
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        meta.insert("exported".into(), json!(true));
    }

    // Determine if it's a struct or interface
    if let Some(type_node) = node.child_by_field_name("type") {
        match type_node.kind() {
            "struct_type" => {
                // Collect fields
                let mut fields = Vec::new();
                let mut cursor = type_node.walk();
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
                if !fields.is_empty() {
                    meta.insert("fields".into(), json!(fields));
                }

                let source = ctx.node_text(node);
                let content = truncate(source, 2000);
                let id = ctx.add_node(&name, "struct", Some(start), Some(end), content, meta);
                types.insert(name, id);
            }
            "interface_type" => {
                // Collect methods
                let mut methods = Vec::new();
                let mut cursor = type_node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.kind() == "method_spec" {
                            if let Some(mname) = child_text(ctx, child, "field_identifier") {
                                methods.push(mname);
                            }
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
                if !methods.is_empty() {
                    meta.insert("methods".into(), json!(methods));
                }

                let id = ctx.add_node(&name, "interface", Some(start), Some(end), None, meta);
                types.insert(name, id);
            }
            _ => {
                // Type alias
                let source = ctx.node_text(node);
                let content = truncate(source, 500);
                let id = ctx.add_node(&name, "type_alias", Some(start), None, content, meta);
                types.insert(name, id);
            }
        }
    }
}

fn extract_imports(ctx: &mut ExtractionContext, node: Node) {
    let source = ctx.node_text(node).to_string();
    let start = node.start_position().row as u32 + 1;

    let mut meta = HashMap::new();
    meta.insert("raw".into(), json!(source));

    ctx.add_node(&source, "import", Some(start), None, None, meta);
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
    use super::super::super::parser::{CodeEdge, CodeNode};
    use super::super::super::qualified_name;

    fn extract_source(file_path: &str, source: &str) -> (Vec<CodeNode>, Vec<CodeEdge>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .expect("load go grammar");
        let tree = parser.parse(source, None).expect("parse go source");

        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: file_path.to_string(),
            language: "go".to_string(),
            source: source.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };

        extract(&mut ctx, tree.root_node());

        let module_path = qualified_name::module_path(file_path, "go");
        qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

        (ctx.nodes, ctx.edges)
    }

    #[test]
    fn method_gets_real_containment_from_same_file_receiver() {
        let src = "package server\n\ntype Handler struct{}\n\nfunc (h *Handler) serve() {}\n";
        let (nodes, edges) = extract_source("pkg/server/handler.go", src);

        let handler = nodes
            .iter()
            .find(|n| n.name == "Handler")
            .expect("Handler node");
        let serve = nodes
            .iter()
            .find(|n| n.name == "serve")
            .expect("serve node");

        // The spec's own worked example for Go qualified names.
        assert_eq!(handler.qualified_name, "pkg::server::Handler");
        assert_eq!(serve.qualified_name, "pkg::server::Handler::serve");

        // Real containment (not just a name-based edge) now that the
        // receiver is declared in the same file.
        assert!(edges.iter().any(|e| e.edge_type == "contains"
            && e.from_id == handler.id
            && e.to_id == serve.id));
        assert!(!edges.iter().any(|e| e.edge_type == "member_of"));
    }

    #[test]
    fn method_falls_back_to_member_of_when_receiver_not_in_file() {
        // "Remote" is never declared in this file (lives elsewhere in the
        // package) — same behavior as before this fix.
        let src = "package server\n\nfunc (r *Remote) serve() {}\n";
        let (nodes, edges) = extract_source("pkg/server/handler.go", src);

        let serve = nodes
            .iter()
            .find(|n| n.name == "serve")
            .expect("serve node");

        assert!(!edges.iter().any(|e| e.edge_type == "contains"));
        assert!(edges.iter().any(|e| e.edge_type == "member_of"
            && e.from_id == serve.id
            && e.to_name.as_deref() == Some("Remote")));

        // No same-file containment available — falls back to the module
        // path alone. Still correct, just flatter.
        assert_eq!(serve.qualified_name, "pkg::server::serve");
    }

    #[test]
    fn top_level_function_qualified_name() {
        let src = "package main\n\nfunc Run() {}\n";
        let (nodes, _edges) = extract_source("cmd/app/main.go", src);
        let run = nodes.iter().find(|n| n.name == "Run").expect("Run node");
        assert_eq!(run.qualified_name, "cmd::app::Run");
    }

    #[test]
    fn grouped_type_declaration_populates_all_receivers() {
        let src = "package server\n\ntype (\n\tA struct{}\n\tB struct{}\n)\n\nfunc (a *A) one() {}\nfunc (b *B) two() {}\n";
        let (nodes, edges) = extract_source("pkg/server/handler.go", src);

        let a = nodes.iter().find(|n| n.name == "A").expect("A node");
        let b = nodes.iter().find(|n| n.name == "B").expect("B node");
        let one = nodes.iter().find(|n| n.name == "one").expect("one node");
        let two = nodes.iter().find(|n| n.name == "two").expect("two node");

        assert!(edges
            .iter()
            .any(|e| e.edge_type == "contains" && e.from_id == a.id && e.to_id == one.id));
        assert!(edges
            .iter()
            .any(|e| e.edge_type == "contains" && e.from_id == b.id && e.to_id == two.id));
        assert_eq!(one.qualified_name, "pkg::server::A::one");
        assert_eq!(two.qualified_name, "pkg::server::B::two");
    }
}
