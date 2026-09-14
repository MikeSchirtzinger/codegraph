//! TypeScript / JavaScript AST extractor.
//!
//! Extracts: functions, classes, interfaces, type aliases, imports, exports,
//! method definitions, named value bindings (arrow functions anywhere,
//! object literals at module level), and module-scope calls.
//!
//! The last two exist because of two gaps measured against
//! `continuedev/continue` at `5b5532f7`, recorded in
//! `specs/receipts/extractor-gaps-20260914.md`:
//!
//! - **G1**: `export const searchAndReplaceInFileTool: Tool = { … }` produced
//!   no node at all. Indexing that whole file alone gave 10 nodes, 9 of them
//!   imports, and zero edges, so when the file was deleted there was no name
//!   for deletion tracking to record and the structural gate had nothing to
//!   query. A binding is a definition in TypeScript whether its value is a
//!   function or an object, and idiomatic TS ships whole modules as object
//!   literals.
//! - **G2**: `preprocess.test.ts` produced 11 nodes, every one an import, and
//!   zero edges. All of its code sits inside `describe`/`it` callbacks, which
//!   have no enclosing *named* definition, and only a named definition's body
//!   was ever walked for calls. Every reference a test file makes was
//!   invisible to the graph.
//!
//! ## The attribution rule
//!
//! **A call belongs to the nearest enclosing definition, once.** Three walks
//! record calls here, and each stops where the next one starts, so the rule
//! holds without any of them knowing about the others' results:
//!
//! - `extract_function` walks a named function's body, stopping at any
//!   binding that became a definition (`is_claimed_binding`).
//! - `bind_value` walks a claimed binding's value.
//! - `extract_module_scope_calls` walks everything a definition did not
//!   claim, stopping at every declaration that owns its own walk
//!   (`owns_its_own_scope`).
//!
//! The two stop sets are deliberately different sizes. The module-scope pass
//! stops at a class, whose methods it must not claim; the function-body walk
//! does not, because `extract_method` never walks a body, so a nested
//! class's calls have to stay with the function around them rather than
//! vanish. That gap is recorded in the receipt, not fixed here.

use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

use super::super::parser::ExtractionContext;
use super::super::IndexingTier;

pub fn extract(ctx: &mut ExtractionContext, root: Node) {
    walk(ctx, root);
    if ctx.tier != IndexingTier::Fast {
        extract_module_scope_calls(ctx, root);
    }
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
            extract_value_bindings(ctx, node);
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
                // G1: `export const x = { … }` / `export const f = () => {}`.
                // Without this arm the early `return` in `walk` meant an
                // exported binding was never reached by anything at all.
                "lexical_declaration" | "variable_declaration" => {
                    extract_value_bindings(ctx, child);
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Named value bindings: `const f = () => {}` (any scope) and
/// `const x = { … }` (module scope only).
///
/// The object-literal half is G1. It is deliberately restricted to
/// module-level declarations, meaning a binding directly under `program`, or
/// an `export_statement` that is itself directly under `program`. A
/// module-level binding is part of the module's surface and is exactly what
/// another file imports, deletes, or renames; an object literal bound inside
/// a function body is a local value, and modeling every one of those as a
/// project-wide definition would flood the candidate pools that the resolver
/// cascade keys on bare name. `const f = function () {}` is still not
/// modeled: it is the same class as the arrow case but was not one of the
/// measured gaps, so it stays recorded rather than guessed at.
fn extract_value_bindings(ctx: &mut ExtractionContext, node: Node) {
    let module_level = is_module_level(node);
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

                if let Some(value) = child.child_by_field_name("value") {
                    match value.kind() {
                        "arrow_function" => bind_value(ctx, node, value, &name, "function"),
                        "object" if module_level => bind_value(ctx, node, value, &name, "object"),
                        _ => {}
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Add the definition node for one binding and walk its value for calls.
///
/// Walking the value is the binding half of G2: an arrow function's body
/// used to produce a node with no outgoing edges at all, so a module written
/// as `export const handler = async () => { … }` contributed no references.
fn bind_value(
    ctx: &mut ExtractionContext,
    decl: Node,
    value: Node,
    name: &str,
    node_type: &str,
) {
    let start = decl.start_position().row as u32 + 1;
    let end = value.end_position().row as u32 + 1;

    let mut meta = HashMap::new();
    if decl.parent().map(|p| p.kind()) == Some("export_statement") {
        meta.insert("exported".into(), json!(true));
    }

    let content = truncate(ctx.node_text(decl), 2000);
    let id = ctx.add_node(name, node_type, Some(start), Some(end), content, meta);

    if ctx.tier != IndexingTier::Fast {
        extract_calls_from(ctx, value, &id);
    }
}

/// Is this declaration at module level? That means directly under
/// `program`, or under an `export_statement` that is itself directly under
/// `program`.
fn is_module_level(node: Node) -> bool {
    match node.parent() {
        None => false,
        Some(parent) if parent.kind() == "program" => true,
        Some(parent) if parent.kind() == "export_statement" => {
            parent.parent().map(|p| p.kind()) == Some("program")
        }
        Some(_) => false,
    }
}

/// G2: every call that is **not** inside something this extractor already
/// modeled as a definition, attributed to one synthetic module-scope node
/// for the file.
///
/// A TypeScript file's top level is executable, and in test files it is
/// where all of the code lives: `describe("…", () => { it("…", async () => {
/// … }) })` is one module-level call expression whose callbacks are
/// anonymous, so no named definition encloses anything inside it. Before
/// this pass, `preprocess.test.ts` produced zero edges and its five calls
/// through `searchAndReplaceInFileTool` were absent from the graph.
///
/// The pass prunes at every construct that owns its own call extraction, so
/// no call is attributed twice. The node is created lazily: a file whose
/// code is entirely inside definitions gets no module node, which is most
/// non-test files.
///
/// The node's `node_type` is `module`, not `function`, and that is
/// load-bearing. The resolver's candidate pool is keyed on (bare name,
/// `to_type`, language family), and a `calls` capture always arrives with
/// `to_type = "function"`, so a module node can never become a false
/// resolution target for a call that happens to share the file's basename.
fn extract_module_scope_calls(ctx: &mut ExtractionContext, root: Node) {
    let mut callees: Vec<String> = Vec::new();
    collect_module_scope_calls(ctx, root, &mut callees);
    if callees.is_empty() {
        return;
    }

    let name = module_scope_name(&ctx.file_path);
    let end = root.end_position().row as u32 + 1;
    let id = ctx.add_node(&name, "module", Some(1), Some(end), None, HashMap::new());
    for callee in callees {
        ctx.add_name_edge(&id, &callee, "function", "calls", "INFERRED");
    }
}

/// Every construct whose body is already walked for calls by the owner that
/// extracted it, or which cannot contain module-scope code at all. Recursion
/// stops here so a call is never recorded twice.
fn owns_its_own_scope(node: Node) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "import_statement"
    ) || is_claimed_binding(node)
}

/// Did `extract_value_bindings` turn this `variable_declarator` into a
/// definition node? A claimed binding owns every call in its value, so every
/// other walk stops at it and the attribution rule stays one sentence:
/// **a call belongs to the nearest enclosing definition, once.**
///
/// This has to be exact rather than "any declarator". An object literal
/// below module level is never claimed (see `extract_value_bindings`), so
/// the enclosing function still owns the calls inside it; stopping there
/// would drop them from the graph with nothing to pick them up.
fn is_claimed_binding(node: Node) -> bool {
    if node.kind() != "variable_declarator" {
        return false;
    }
    let Some(value) = node.child_by_field_name("value") else {
        return false;
    };
    match value.kind() {
        "arrow_function" => true,
        // `node.parent()` is the lexical/variable declaration itself, which
        // is what decides module level.
        "object" => node.parent().is_some_and(is_module_level),
        _ => false,
    }
}

fn collect_module_scope_calls(ctx: &ExtractionContext, node: Node, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let child = cursor.node();
        if !owns_its_own_scope(child) {
            if child.kind() == "call_expression" {
                if let Some(func) = child.child_by_field_name("function") {
                    let callee = ctx.node_text(func);
                    if !callee.is_empty() && callee.len() < 100 {
                        out.push(callee.to_string());
                    }
                }
            }
            collect_module_scope_calls(ctx, child, out);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// The module node's bare name: the file's basename with its final extension
/// removed (`tools/preprocess.test.ts` -> `preprocess.test`). Where the
/// basename carries a dot it is not an identifier a caller could write at
/// all.
///
/// The `module` type keeps it out of the *resolver's* candidate pools, which
/// are keyed on (bare name, `to_type`, language family) and never hold a
/// `module` for a `calls` capture. That is not the same as being invisible
/// to every name-keyed lookup in the codebase, and one of them did see it:
/// `plan::resolve` keyed its bare-name index on the name alone, so
/// `- symbol: utils` bound to the module node for `utils.ts`. That is fixed
/// there, by excluding `module` the way `import` was already excluded, with
/// `a_module_node_never_satisfies_a_symbol_touch` in `tests/plan_ops.rs`
/// pinning it. Any new name-keyed index has the same question to answer.
fn module_scope_name(file_path: &str) -> String {
    let base = file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_path);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => base.to_string(),
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
        // A binding that became its own definition owns what it calls. Not
        // stopping here attributed every call inside a nested arrow both to
        // this caller and to the binding, three `calls` edges for two source
        // calls. Deliberately narrower than the module-scope pass's stop
        // set: a nested class's methods get no walk of their own, so their
        // calls must stay with the function around them rather than vanish.
        if !is_claimed_binding(node) {
            if node.kind() == "call_expression" {
                if let Some(func) = node.child_by_field_name("function") {
                    let callee = ctx.node_text(func).to_string();
                    if !callee.is_empty() && callee.len() < 100 {
                        ctx.add_name_edge(caller_id, &callee, "function", "calls", "INFERRED");
                    }
                }
            }

            walk_calls(ctx, cursor, caller_id);
        }

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
        extract_ctx(file_path, source).nodes
    }

    /// The whole context, for tests that assert on edges as well as nodes.
    fn extract_ctx(file_path: &str, source: &str) -> ExtractionContext {
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

        ctx
    }

    fn callees(ctx: &ExtractionContext) -> Vec<String> {
        ctx.edges
            .iter()
            .filter(|e| e.edge_type == "calls")
            .filter_map(|e| e.to_name.clone())
            .collect()
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

    /// G1, verbatim from `continuedev/continue` at `5b5532f7`:
    /// `extensions/cli/src/tools/searchAndReplace/index.ts` exports its tool
    /// as a typed object literal. Indexing that file alone produced 10 nodes,
    /// 9 imports and 1 interface, and no node for the export itself, so
    /// deleting the file recorded no deleted symbol and the structural gate
    /// had nothing to query.
    #[test]
    fn exported_const_object_literal_is_a_definition() {
        let src = r#"
import { validateArgs } from "./parseArgs.js";

export interface Tool {
  name: string;
}

export const searchAndReplaceInFileTool: Tool = {
  name: "Search and Replace",
  preprocess: async (args: unknown) => {
    validateArgs(args);
    return { args };
  },
};
"#;
        let ctx = extract_ctx("tools/searchAndReplace/index.ts", src);
        let tool = ctx
            .nodes
            .iter()
            .find(|n| n.name == "searchAndReplaceInFileTool")
            .expect("the exported const must be a definition node");
        assert_eq!(tool.node_type, "object");
        assert_eq!(
            tool.qualified_name,
            "tools::searchAndReplace::index::searchAndReplaceInFileTool"
        );
        assert_eq!(tool.metadata.get("exported"), Some(&json!(true)));

        // And its body contributes references, so the object is not an
        // inert name.
        assert!(
            callees(&ctx).iter().any(|c| c == "validateArgs"),
            "calls inside the object literal are references, got {:?}",
            callees(&ctx)
        );
    }

    /// An object literal bound inside a function body is a local value, not
    /// a project-wide definition. This is the specificity half of G1: the
    /// fix must not flood the resolver's bare-name candidate pools with
    /// every local config object in the tree.
    #[test]
    fn inner_scope_object_literal_is_not_a_definition() {
        let src = "function run() {\n  const options = { retries: 3 };\n  return options;\n}\n";
        let nodes = extract_source("src/app.ts", src);
        assert!(
            !nodes.iter().any(|n| n.name == "options"),
            "a local object literal is not a definition, got {:?}",
            nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
    }

    /// G2, the shape of `extensions/cli/src/tools/preprocess.test.ts` at the
    /// same commit: every line of code sits inside `describe`/`it`
    /// callbacks. The file produced 11 nodes, all imports, and zero edges,
    /// so the five calls through the deleted `searchAndReplaceInFileTool`
    /// were absent from the graph entirely.
    #[test]
    fn calls_inside_describe_and_it_callbacks_are_extracted() {
        let src = r#"
import { searchAndReplaceInFileTool } from "./searchAndReplace/index.js";

describe("preprocess", () => {
  it("returns the edited file", async () => {
    const result = await searchAndReplaceInFileTool.preprocess({
      filepath: "a.ts",
    });
    expect(result).toBeDefined();
  });
});
"#;
        let ctx = extract_ctx("tools/preprocess.test.ts", src);
        let names = callees(&ctx);
        assert!(
            names.iter().any(|c| c == "searchAndReplaceInFileTool.preprocess"),
            "the call through the imported tool must be captured, got {names:?}"
        );
        assert!(names.iter().any(|c| c == "describe"), "got {names:?}");
        assert!(names.iter().any(|c| c == "it"), "got {names:?}");

        let module = ctx
            .nodes
            .iter()
            .find(|n| n.node_type == "module")
            .expect("module-scope node");
        assert_eq!(module.name, "preprocess.test");
        assert!(
            ctx.edges
                .iter()
                .filter(|e| e.edge_type == "calls")
                .all(|e| e.from_id == module.id),
            "every module-scope call hangs off the module node"
        );
    }

    /// The lazy half: a file whose code is entirely inside named
    /// definitions gets no module node, and its calls are still attributed
    /// to the function that makes them.
    #[test]
    fn a_file_with_no_module_scope_code_gets_no_module_node() {
        let src = "function run() {\n  helper();\n}\n";
        let ctx = extract_ctx("src/app.ts", src);
        assert!(
            !ctx.nodes.iter().any(|n| n.node_type == "module"),
            "no module-scope call, so no module node"
        );
        let run = ctx.nodes.iter().find(|n| n.name == "run").expect("run node");
        assert!(ctx
            .edges
            .iter()
            .any(|e| e.from_id == run.id && e.to_name.as_deref() == Some("helper")));
    }

    /// A call is attributed once, not twice: the module-scope pass must stop
    /// at every construct that already walked its own body.
    #[test]
    fn module_scope_pass_does_not_double_count() {
        let src = "function run() {\n  helper();\n}\nrun();\n";
        let ctx = extract_ctx("src/app.ts", src);
        let names = callees(&ctx);
        assert_eq!(
            names.iter().filter(|c| *c == "helper").count(),
            1,
            "got {names:?}"
        );
        assert_eq!(names.iter().filter(|c| *c == "run").count(), 1, "got {names:?}");
    }

    /// Every call is attributed to the nearest enclosing definition, once.
    ///
    /// A binding that became a definition owns the calls in its value, so
    /// the enclosing function's walk has to stop at it. Before the stop,
    /// this five-line file produced three `calls` edges for two source
    /// calls: `outer` claimed both, because its body walk saw the whole
    /// subtree, and `inner` claimed `helper` again when `walk` recursed into
    /// the body and reached the binding.
    #[test]
    fn a_nested_binding_owns_its_calls_and_the_outer_walk_stops() {
        let src = "function outer() {\n  const inner = () => {\n    helper();\n  };\n  inner();\n}\n";
        let ctx = extract_ctx("src/app.ts", src);

        let names = callees(&ctx);
        assert_eq!(names.len(), 2, "two source calls, two edges: {names:?}");

        let outer = ctx.nodes.iter().find(|n| n.name == "outer").expect("outer");
        let inner = ctx.nodes.iter().find(|n| n.name == "inner").expect("inner");

        let edge_from = |id: &str| -> Vec<&str> {
            ctx.edges
                .iter()
                .filter(|e| e.edge_type == "calls" && e.from_id == id)
                .filter_map(|e| e.to_name.as_deref())
                .collect()
        };
        assert_eq!(
            edge_from(&outer.id),
            vec!["inner"],
            "the outer function calls the binding, and nothing through it"
        );
        assert_eq!(
            edge_from(&inner.id),
            vec!["helper"],
            "the binding owns the call in its own body"
        );
    }

    /// The stop is precise, not a blanket refusal to walk declarations. An
    /// object literal below module level is never claimed as a definition,
    /// so the enclosing function keeps the calls inside it. Pruning every
    /// declarator would have dropped this call from the graph entirely.
    #[test]
    fn an_inner_object_literal_leaves_its_calls_with_the_function() {
        let src = "function run() {\n  const options = { retries: backoff() };\n  return options;\n}\n";
        let ctx = extract_ctx("src/app.ts", src);
        let run = ctx.nodes.iter().find(|n| n.name == "run").expect("run");
        assert!(
            ctx.edges.iter().any(|e| e.from_id == run.id
                && e.to_name.as_deref() == Some("backoff")),
            "got {:?}",
            callees(&ctx)
        );
        assert_eq!(callees(&ctx).len(), 1, "counted once: {:?}", callees(&ctx));
    }
}
