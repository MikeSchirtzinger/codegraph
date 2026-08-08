//! Java AST extractor.
//!
//! Extracts: classes, interfaces, methods, fields, imports, annotations.

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
        "class_declaration" => extract_class(ctx, node, enclosing_id),
        "interface_declaration" => extract_interface(ctx, node),
        "method_declaration" => extract_method(ctx, node, enclosing_id),
        "constructor_declaration" => extract_method(ctx, node, enclosing_id),
        "import_declaration" => extract_import(ctx, node),
        "enum_declaration" => extract_enum(ctx, node),
        _ => {
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
    }
}

fn extract_class(ctx: &mut ExtractionContext, node: Node, parent_id: Option<&str>) {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    // Check for extends/implements
    if let Some(sc) = node.child_by_field_name("superclass") {
        meta.insert("extends".into(), json!(ctx.node_text(sc)));
    }
    if let Some(ifaces) = node.child_by_field_name("interfaces") {
        meta.insert("implements".into(), json!(ctx.node_text(ifaces)));
    }

    let class_id = ctx.add_node(&name, "class", Some(start), Some(end), None, meta);

    if let Some(pid) = parent_id {
        ctx.add_edge(pid, &class_id, "contains", "EXTRACTED");
    }

    // Extract body
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                walk(ctx, cursor.node(), Some(&class_id));
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

fn extract_interface(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();
    let mut methods = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "method_declaration" {
                    if let Some(mname) = child_text(ctx, child, "identifier") {
                        methods.push(mname);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    if !methods.is_empty() {
        meta.insert("methods".into(), json!(methods));
    }

    ctx.add_node(&name, "interface", Some(start), Some(end), None, meta);
}

fn extract_method(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let mut meta = HashMap::new();

    if let Some(params) = node.child_by_field_name("parameters") {
        meta.insert("parameters".into(), json!(ctx.node_text(params)));
    }

    if let Some(ret) = node.child_by_field_name("type") {
        meta.insert("return_type".into(), json!(ctx.node_text(ret)));
    }

    let source = ctx.node_text(node);
    let content = truncate(source, 2000);
    let method_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &method_id, "contains", "EXTRACTED");
    }

    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls(ctx, body, &method_id);
        }
    }
}

fn extract_enum(ctx: &mut ExtractionContext, node: Node) {
    let name = child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();
    let mut constants = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "enum_constant" {
                    if let Some(cname) = child_text(ctx, child, "identifier") {
                        constants.push(cname);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    if !constants.is_empty() {
        meta.insert("constants".into(), json!(constants));
    }

    ctx.add_node(&name, "enum", Some(start), Some(end), None, meta);
}

fn extract_import(ctx: &mut ExtractionContext, node: Node) {
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
        if node.kind() == "method_invocation" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = ctx.node_text(name_node).to_string();
                if !name.is_empty() {
                    // Preserve the receiver/qualifier verbatim (e.g.
                    // `Db.connect` or `com.example.db.Db.connect`), like
                    // every other language's extractor already does for its
                    // call expressions — a bare `name` here silently
                    // discarded the whole qualifier, making qualified-call
                    // resolution (D2) impossible for Java. `this.foo()` is
                    // a semantically unqualified instance call, so the
                    // literal `this.` prefix is stripped rather than kept
                    // as a qualifier that can never resolve to anything.
                    let callee = match node.child_by_field_name("object") {
                        Some(object_node) => {
                            let qualified = format!("{}.{name}", ctx.node_text(object_node));
                            qualified
                                .strip_prefix("this.")
                                .map(str::to_string)
                                .unwrap_or(qualified)
                        }
                        None => name,
                    };
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
    use super::super::super::qualified_name;
    use super::*;

    type CodeNode = super::super::super::parser::CodeNode;
    type CodeEdge = super::super::super::parser::CodeEdge;

    fn extract_source(file_path: &str, source: &str) -> (Vec<CodeNode>, Vec<CodeEdge>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("load java grammar");
        let tree = parser.parse(source, None).expect("parse java source");

        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: file_path.to_string(),
            language: "java".to_string(),
            source: source.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };

        extract(&mut ctx, tree.root_node());

        let module_path = qualified_name::module_path(file_path, "java");
        qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

        (ctx.nodes, ctx.edges)
    }

    #[test]
    fn class_method_qualified_name() {
        let src = "class Handler {\n    void serve() {}\n}\n";
        let (nodes, _edges) = extract_source("com/example/Handler.java", src);
        let class = nodes
            .iter()
            .find(|n| n.name == "Handler")
            .expect("Handler node");
        let method = nodes
            .iter()
            .find(|n| n.name == "serve")
            .expect("serve node");
        assert_eq!(class.qualified_name, "com::example::Handler");
        assert_eq!(method.qualified_name, "com::example::Handler::serve");
    }

    #[test]
    fn call_expressions_capture_qualifier() {
        // Covers the D2 bug: a bare `name` field discarded the receiver,
        // making every Java call — qualified or not — indistinguishable.
        let src = "class Caller {\n    void run() {\n        foo();\n        this.foo();\n        Db.connect();\n        com.example.db.Db.connect();\n    }\n}\n";
        let (nodes, edges) = extract_source("Caller.java", src);
        let run = nodes.iter().find(|n| n.name == "run").expect("run node");

        let to_names: Vec<String> = edges
            .iter()
            .filter(|e| e.edge_type == "calls" && e.from_id == run.id)
            .map(|e| e.to_name.clone().unwrap_or_default())
            .collect();

        assert_eq!(
            to_names,
            vec![
                "foo".to_string(),                       // bare call
                "foo".to_string(),                       // this.foo() -> "this." stripped
                "Db.connect".to_string(),                // single qualifier, verbatim
                "com.example.db.Db.connect".to_string(), // full dotted path, verbatim
            ]
        );
    }
}
