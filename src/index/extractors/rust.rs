//! Rust AST extractor — the deepest extractor.
//!
//! Extracts: functions, structs, enums, traits, impl blocks, use declarations,
//! modules, macro invocations. Tracks call edges within function bodies,
//! impl-for and impl-trait relationships, generics, lifetimes, async/unsafe.

use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

use super::super::parser::ExtractionContext;
use super::super::IndexingTier;

pub fn extract(ctx: &mut ExtractionContext, root: Node) {
    let mut cursor = root.walk();
    extract_children(ctx, root, &mut cursor, None);
}

fn extract_children(
    ctx: &mut ExtractionContext,
    _parent: Node,
    cursor: &mut tree_sitter::TreeCursor,
    enclosing_id: Option<&str>,
) {
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let node = cursor.node();
        extract_node(ctx, node, enclosing_id);
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    cursor.goto_parent();
}

fn extract_node(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    match node.kind() {
        "function_item" => extract_function(ctx, node, enclosing_id),
        "struct_item" => extract_struct(ctx, node),
        "enum_item" => extract_enum(ctx, node),
        "trait_item" => extract_trait(ctx, node),
        "impl_item" => extract_impl(ctx, node),
        "use_declaration" => extract_use(ctx, node, enclosing_id),
        "mod_item" => extract_mod(ctx, node),
        "macro_invocation" if ctx.tier == IndexingTier::Full => {
            extract_macro_invocation(ctx, node, enclosing_id);
        }
        _ => {
            // Recurse into children for nested items
            let mut cursor = node.walk();
            extract_children(ctx, node, &mut cursor, enclosing_id);
        }
    }
}

fn extract_function(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    let name = find_child_text(ctx, node, "identifier")
        .or_else(|| find_child_text(ctx, node, "metavariable"))
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();

    // Check qualifiers
    let source = ctx.node_text(node);
    if source.starts_with("async ") || source.contains(" async ") {
        meta.insert("async".into(), json!(true));
    }
    if source.starts_with("unsafe ") || source.contains(" unsafe ") {
        meta.insert("unsafe".into(), json!(true));
    }
    if source.starts_with("pub ") || source.contains(" pub ") {
        meta.insert("visibility".into(), json!("public"));
    }

    // Extract return type
    if let Some(ret) = node.child_by_field_name("return_type") {
        meta.insert("return_type".into(), json!(ctx.node_text(ret)));
    }

    // Extract parameters
    if let Some(params) = node.child_by_field_name("parameters") {
        let params_text = ctx.node_text(params);
        meta.insert("parameters".into(), json!(params_text));
    }

    // Extract generics
    if let Some(type_params) = node.child_by_field_name("type_parameters") {
        meta.insert("generics".into(), json!(ctx.node_text(type_params)));
    }

    let content = Some(truncate_safe(source, 2000));

    let fn_id = ctx.add_node(&name, "function", Some(start), Some(end), content, meta);

    // Add contains edge from enclosing
    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &fn_id, "contains", "EXTRACTED");
    }

    // Extract call edges from function body
    if ctx.tier != IndexingTier::Fast {
        if let Some(body) = node.child_by_field_name("body") {
            extract_calls(ctx, body, &fn_id);
            extract_closure_bindings(ctx, body, &fn_id);
        }
    }
}

/// G4 (`specs/receipts/extractor-gaps-20260914.md`): a closure bound to a name is a definition.
///
/// `let base_event = |base: &BaseEvent| { … };` in a private Rust codebase
/// was not modeled, so
/// after an unrelated `fn base_event()` was deleted from a sibling file the
/// graph held zero live definitions for the name, every one of the 24 calls
/// to the closure stayed UNRESOLVED, and the stale scan's deleted-name
/// branch reported all of them. That was the only false positive in four
/// replay audits. With the binding modeled, R3 (same-file bare name) binds
/// those calls, they never reach the stale scan, and the finding disappears
/// at its source rather than being filtered out downstream.
///
/// The closure's own body is deliberately *not* walked again for calls: the
/// enclosing function's `extract_calls` already walked the whole body,
/// including this subtree, and attributing the same call twice would inflate
/// every edge count for no new information.
fn extract_closure_bindings(ctx: &mut ExtractionContext, body: Node, enclosing_id: &str) {
    let mut cursor = body.walk();
    walk_closure_bindings(ctx, &mut cursor, enclosing_id);
}

fn walk_closure_bindings(
    ctx: &mut ExtractionContext,
    cursor: &mut tree_sitter::TreeCursor,
    enclosing_id: &str,
) {
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "let_declaration" {
            let value = node.child_by_field_name("value");
            let pattern = node.child_by_field_name("pattern");
            if let (Some(value), Some(pattern)) = (value, pattern) {
                // Only a plain `let name = |…|`. A destructuring pattern
                // binds no single symbol another file could call.
                if value.kind() == "closure_expression" && pattern.kind() == "identifier" {
                    let name = ctx.node_text(pattern).to_string();
                    if !name.is_empty() {
                        let start = node.start_position().row as u32 + 1;
                        let end = value.end_position().row as u32 + 1;
                        let content = Some(truncate_safe(ctx.node_text(node), 2000));
                        let id = ctx.add_node(
                            &name,
                            "function",
                            Some(start),
                            Some(end),
                            content,
                            HashMap::new(),
                        );
                        ctx.add_edge(enclosing_id, &id, "contains", "EXTRACTED");
                    }
                }
            }
        }

        walk_closure_bindings(ctx, cursor, enclosing_id);

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    cursor.goto_parent();
}

fn extract_struct(ctx: &mut ExtractionContext, node: Node) {
    let name = find_child_text(ctx, node, "type_identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();

    // Collect field names
    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "field_declaration" {
                    if let Some(field_name) = find_child_text(ctx, child, "field_identifier") {
                        fields.push(field_name);
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

    // Check for derives
    extract_derives(ctx, node, &mut meta);

    // Generics
    if let Some(type_params) = node.child_by_field_name("type_parameters") {
        meta.insert("generics".into(), json!(ctx.node_text(type_params)));
    }

    let source = ctx.node_text(node);
    let content = Some(truncate_safe(source, 2000));

    ctx.add_node(&name, "struct", Some(start), Some(end), content, meta);
}

fn extract_enum(ctx: &mut ExtractionContext, node: Node) {
    let name = find_child_text(ctx, node, "type_identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();

    // Collect variants
    let mut variants = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "enum_variant" {
                    if let Some(vname) = find_child_text(ctx, child, "identifier") {
                        variants.push(vname);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    if !variants.is_empty() {
        meta.insert("variants".into(), json!(variants));
    }

    extract_derives(ctx, node, &mut meta);

    let source = ctx.node_text(node);
    let content = Some(truncate_safe(source, 2000));

    ctx.add_node(&name, "enum", Some(start), Some(end), content, meta);
}

fn extract_trait(ctx: &mut ExtractionContext, node: Node) {
    let name = find_child_text(ctx, node, "type_identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();

    // Collect method signatures
    let mut methods = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "function_signature_item" || child.kind() == "function_item" {
                    if let Some(mname) = find_child_text(ctx, child, "identifier") {
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

    // Generics
    if let Some(type_params) = node.child_by_field_name("type_parameters") {
        meta.insert("generics".into(), json!(ctx.node_text(type_params)));
    }

    let source = ctx.node_text(node);
    let content = Some(truncate_safe(source, 1000));

    ctx.add_node(&name, "trait", Some(start), Some(end), content, meta);
}

fn extract_impl(ctx: &mut ExtractionContext, node: Node) {
    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    // Parse the impl: `impl [Trait for] Type`
    let _source = ctx.node_text(node);
    let mut impl_type = String::new();
    let mut impl_trait = None;

    // Find the type being impl'd
    if let Some(t) = node.child_by_field_name("type") {
        impl_type = ctx.node_text(t).to_string();
    }

    // Find the trait (if trait impl)
    if let Some(t) = node.child_by_field_name("trait") {
        impl_trait = Some(ctx.node_text(t).to_string());
    }

    if impl_type.is_empty() {
        return;
    }

    let name = if let Some(ref tr) = impl_trait {
        format!("impl {tr} for {impl_type}")
    } else {
        format!("impl {impl_type}")
    };

    let mut meta = HashMap::new();
    meta.insert("impl_for".into(), json!(impl_type));
    if let Some(ref tr) = impl_trait {
        meta.insert("impl_trait".into(), json!(tr));
    }
    meta.insert("inherent".into(), json!(impl_trait.is_none()));

    let impl_id = ctx.add_node(&name, "impl", Some(start), Some(end), None, meta);

    // Create implements edge: impl block -> trait (if trait impl). The trait
    // may be declared in a different file, so its real node_id can't be
    // computed here — resolved by name in the post-indexing pass instead.
    // (The impl block, not the raw type, is used as the edge source since
    // `impl_id` is a real id we just created; the type's own node id can't
    // be derived from its name alone either.)
    if let Some(ref trait_name) = impl_trait {
        ctx.add_name_edge(&impl_id, trait_name, "trait", "implements", "INFERRED");
    }

    // Extract methods inside the impl block
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "function_item" {
                    extract_function(ctx, child, Some(&impl_id));
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

fn extract_use(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    let source = ctx.node_text(node).to_string();
    let start = node.start_position().row as u32 + 1;

    let mut meta = HashMap::new();
    meta.insert("raw".into(), json!(source));

    let id = ctx.add_node(&source, "import", Some(start), None, None, meta);

    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &id, "imports", "EXTRACTED");
    }
}

fn extract_mod(ctx: &mut ExtractionContext, node: Node) {
    let name = find_child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    let mut meta = HashMap::new();
    let source = ctx.node_text(node);
    if source.starts_with("pub ") {
        meta.insert("visibility".into(), json!("public"));
    }

    let mod_id = ctx.add_node(&name, "module", Some(start), Some(end), None, meta);

    // If inline module (has body), extract its children
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        extract_children(ctx, body, &mut cursor, Some(&mod_id));
    }
}

fn extract_macro_invocation(ctx: &mut ExtractionContext, node: Node, enclosing_id: Option<&str>) {
    let name = find_child_text(ctx, node, "identifier").unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let start = node.start_position().row as u32 + 1;
    let meta = HashMap::new();

    let id = ctx.add_node(&name, "macro_call", Some(start), None, None, meta);

    if let Some(parent_id) = enclosing_id {
        ctx.add_edge(parent_id, &id, "calls", "EXTRACTED");
    }
}

/// Walk a function body to find call_expression and method_call_expression nodes.
fn extract_calls(ctx: &mut ExtractionContext, node: Node, caller_id: &str) {
    let mut cursor = node.walk();
    walk_calls(ctx, node, &mut cursor, caller_id);
}

fn walk_calls(
    ctx: &mut ExtractionContext,
    _parent: Node,
    cursor: &mut tree_sitter::TreeCursor,
    caller_id: &str,
) {
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let node = cursor.node();
        match node.kind() {
            "call_expression" => {
                // function_name(args) — extract the function name
                if let Some(func) = node.child_by_field_name("function") {
                    let callee = ctx.node_text(func).to_string();
                    if !callee.is_empty() {
                        ctx.add_name_edge(caller_id, &callee, "function", "calls", "INFERRED");
                    }
                }
            }
            "method_call_expression" | "field_expression" => {
                // Capture method name for method calls like obj.method()
                // In tree-sitter-rust, method_call_expression doesn't exist —
                // it's a call_expression whose function is a field_expression.
                // But we handle both patterns for robustness.
            }
            _ => {}
        }

        // Recurse into children
        walk_calls(ctx, node, cursor, caller_id);

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    cursor.goto_parent();
}

/// Extract derive attributes from a struct/enum.
fn extract_derives(
    ctx: &ExtractionContext,
    node: Node,
    meta: &mut HashMap<String, serde_json::Value>,
) {
    let mut derives = Vec::new();

    // Walk preceding attribute_item nodes
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        if sib.kind() == "attribute_item" {
            let text = ctx.node_text(sib);
            if text.contains("derive") {
                // Parse: #[derive(Foo, Bar, Baz)]
                if let Some(start) = text.find('(') {
                    if let Some(end) = text.rfind(')') {
                        let inner = &text[start + 1..end];
                        for d in inner.split(',') {
                            let d = d.trim();
                            if !d.is_empty() {
                                derives.push(d.to_string());
                            }
                        }
                    }
                }
            }
        } else {
            break;
        }
        sibling = sib.prev_sibling();
    }

    if !derives.is_empty() {
        meta.insert("derives".into(), json!(derives));
    }
}

/// Truncate a string at a char boundary.
fn truncate_safe(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Find the last char boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Find the text of the first child with the given kind.
fn find_child_text(ctx: &ExtractionContext, node: Node, kind: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::super::super::qualified_name;
    use super::*;

    fn extract_source(file_path: &str, source: &str) -> Vec<super::super::super::parser::CodeNode> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        let tree = parser.parse(source, None).expect("parse rust source");

        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            source: source.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };

        extract(&mut ctx, tree.root_node());

        let module_path = qualified_name::module_path(file_path, "rust");
        qualified_name::assign(&mut ctx.nodes, &ctx.edges, &module_path);

        ctx.nodes
    }

    #[test]
    fn top_level_function_qualified_name() {
        let src = "pub fn resolve_deps() {}\n";
        let nodes = extract_source("src/graph/dependencies.rs", src);
        let f = nodes
            .iter()
            .find(|n| n.name == "resolve_deps")
            .expect("resolve_deps node");
        assert_eq!(f.qualified_name, "graph::dependencies::resolve_deps");
    }

    #[test]
    fn impl_method_qualified_name_uses_bare_type() {
        let src = "struct IndexingTier;\n\nimpl std::str::FromStr for IndexingTier {\n    type Err = ();\n    fn from_str(s: &str) -> Result<Self, ()> { Err(()) }\n}\n";
        let nodes = extract_source("src/index/mod.rs", src);
        let f = nodes
            .iter()
            .find(|n| n.name == "from_str")
            .expect("from_str node");
        // Not "impl FromStr for IndexingTier::from_str" — the containment
        // segment for an impl block is the bare type it's attached to.
        assert_eq!(f.qualified_name, "index::IndexingTier::from_str");
    }

    #[test]
    fn nested_mod_function_qualified_name() {
        let src = "mod tests {\n    fn deterministic_across_calls() {}\n}\n";
        let nodes = extract_source("src/index/node_id.rs", src);
        let f = nodes
            .iter()
            .find(|n| n.name == "deterministic_across_calls")
            .expect("test fn node");
        assert_eq!(
            f.qualified_name,
            "index::node_id::tests::deterministic_across_calls"
        );
    }

    #[test]
    fn crate_root_files_produce_bare_qualified_names() {
        // src/main.rs and src/lib.rs are the crate root itself, not a
        // module named "main"/"lib" — a top-level fn there should be bare,
        // not prefixed with "main::"/"lib::". Pinned end-to-end (real
        // parse + real module_path) rather than asserted from the rule.
        let src = "pub fn foo() {}\n";

        let main_nodes = extract_source("src/main.rs", src);
        let main_foo = main_nodes
            .iter()
            .find(|n| n.name == "foo")
            .expect("foo node in src/main.rs");
        assert_eq!(main_foo.qualified_name, "foo");

        let lib_nodes = extract_source("src/lib.rs", src);
        let lib_foo = lib_nodes
            .iter()
            .find(|n| n.name == "foo")
            .expect("foo node in src/lib.rs");
        assert_eq!(lib_foo.qualified_name, "foo");
    }

    /// G4 (`specs/receipts/extractor-gaps-20260914.md`), the shape
    /// measured on a private Rust codebase: a closure bound to a name, called
    /// from the function that binds it. Unmodeled, the
    /// name had no live definition anywhere once an unrelated `fn
    /// base_event()` was deleted elsewhere, and all 24 call sites were
    /// reported as stale references to the deleted function.
    #[test]
    fn a_named_closure_binding_is_a_definition() {
        let src = "fn agui_to_proto() {\n    let base_event = |base: &BaseEvent| {\n        base.id()\n    };\n    base_event(&a);\n    base_event(&b);\n}\n";
        let nodes = extract_source("src/event_converter.rs", src);
        let closure = nodes
            .iter()
            .find(|n| n.name == "base_event")
            .expect("the closure binding must be a definition node");
        assert_eq!(closure.node_type, "function");
        assert_eq!(
            closure.qualified_name,
            "event_converter::agui_to_proto::base_event",
            "it is contained by the function that binds it"
        );
    }

    /// The specificity half of G4: an ordinary `let` is not a definition,
    /// and a destructuring pattern binds no single callable name.
    #[test]
    fn a_plain_let_binding_is_not_a_definition() {
        let src = "fn run() {\n    let total = compute();\n    let (a, b) = pair();\n}\n";
        let nodes = extract_source("src/app.rs", src);
        let names: Vec<&String> = nodes.iter().map(|n| &n.name).collect();
        assert!(!names.iter().any(|n| *n == "total"), "got {names:?}");
        assert!(!names.iter().any(|n| *n == "a"), "got {names:?}");
    }

    /// A closure's calls stay attributed to the function that owns the body,
    /// counted once. The closure node exists to be a resolution target, not
    /// to re-attribute work the enclosing walk already did.
    #[test]
    fn a_closure_body_is_not_walked_for_calls_twice() {
        let src = "fn outer() {\n    let inner = || {\n        helper();\n    };\n    inner();\n}\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        let tree = parser.parse(src, None).expect("parse rust source");
        let mut ctx = ExtractionContext {
            project_id: "test".to_string(),
            file_path: "src/app.rs".to_string(),
            language: "rust".to_string(),
            source: src.to_string(),
            tier: IndexingTier::Full,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        extract(&mut ctx, tree.root_node());

        let helper_edges = ctx
            .edges
            .iter()
            .filter(|e| e.to_name.as_deref() == Some("helper"))
            .count();
        assert_eq!(helper_edges, 1, "one call, one edge");
    }
}
