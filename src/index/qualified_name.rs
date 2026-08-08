//! Computes each node's `qualified_name`: a language-mechanical module path
//! (derived purely from the file path — no reading of `package`/`namespace`
//! declarations, no build-config awareness) plus its containment chain
//! (walked via `contains` edges) plus its own name, joined with the
//! canonical `::` separator.
//!
//! This runs once per file, immediately after the language-specific
//! extractor returns (see `parser::parse_file`), as a single
//! language-agnostic pass over whatever `contains` edges that extractor
//! emitted. It doesn't know anything about any particular language's
//! grammar — only that a `contains` edge points from a container's node id
//! to the node id it contains.

use std::collections::HashMap;

use super::parser::{CodeEdge, CodeNode};

/// Compute the language-mechanical module-path prefix for a file, already
/// joined with `::`. This is the root every node's `qualified_name` in the
/// file is built on top of.
///
/// Each language gets its own narrow, well-established rule for turning a
/// relative file path into a module path; none of it reads file content or
/// build configuration, so it's exactly as "dumb" as the extractors that
/// build on top of it:
/// - **Rust**: Cargo always roots sources at `src/`, which is dropped; and
///   `mod.rs` / `lib.rs` / `main.rs` name their *own* directory rather than
///   a child module, so that filename is dropped too.
/// - **Go**: the package is the directory — a file's own name never
///   appears in an import path, so it's dropped entirely.
/// - **Python**: `__init__.py` names its own package directory the same
///   way Rust's `mod.rs` does, so it's dropped; otherwise the file's own
///   (extensionless) name is the last segment.
/// - **Java**: like Go, the file's own name is dropped. Java has no
///   top-level functions — every extractable entity is already nested in a
///   named class/interface/enum/record — and javac requires a file's
///   public class name to match its filename, so keeping the filename
///   would just double it (`Handler.java` + `class Handler` ->
///   `Handler::Handler`) for no benefit. This intentionally does *not* try
///   to strip a build-tool source root (e.g. Maven's `src/main/java`) —
///   that's a speculative rule this mechanical pass avoids, unlike the
///   filename, which is a real language-enforced redundancy.
/// - **C / C++ / TypeScript / JavaScript**: the full directory chain plus
///   the bare filename, unstripped — these permit genuine container-less,
///   top-level functions, so the filename is the only thing identifying
///   them; none of these languages enforce a single universal source-root
///   convention the way Cargo does, either.
pub fn module_path(file_path: &str, language: &str) -> String {
    let normalized = file_path.replace('\\', "/");
    let mut segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

    match language {
        "rust" => {
            if segments.first() == Some(&"src") {
                segments.remove(0);
            }
            if let Some(last) = segments.pop() {
                segments.push(strip_ext(last));
            }
            if matches!(segments.last(), Some(&"mod") | Some(&"lib") | Some(&"main")) {
                segments.pop();
            }
        }
        "go" | "java" => {
            // Go: the package is the directory. Java: every top-level
            // entity is already a named class/interface/enum/record, and
            // the filename is required to match the public one anyway —
            // either way the file's own name is dropped.
            segments.pop();
        }
        "python" => {
            if let Some(last) = segments.pop() {
                segments.push(strip_ext(last));
            }
            if segments.last() == Some(&"__init__") {
                segments.pop();
            }
        }
        _ => {
            // c, cpp, typescript, javascript — these allow genuine
            // top-level, container-less functions, so the filename is kept
            // (it's the only thing identifying them).
            if let Some(last) = segments.pop() {
                segments.push(strip_ext(last));
            }
        }
    }

    segments.join("::")
}

/// Strip a single trailing `.ext` from a path segment (e.g.
/// `"dependencies.rs"` -> `"dependencies"`). Segments with no dot, or where
/// the dot is the first character (a dotfile), are returned unchanged.
fn strip_ext(segment: &str) -> &str {
    match segment.rsplit_once('.') {
        Some((stem, _ext)) if !stem.is_empty() => stem,
        _ => segment,
    }
}

/// The name to use for a node's own segment when it appears as an ancestor
/// in a qualified name — usually just the node's `name`, but a Rust `impl`
/// block's display name is `"impl Trait for Type"` (or `"impl Type"`), and
/// qualified paths use `Type::method`, never `impl Type::method`.
fn containment_segment(node: &CodeNode) -> &str {
    if node.node_type == "impl" {
        if let Some(ty) = node.metadata.get("impl_for").and_then(|v| v.as_str()) {
            return ty;
        }
    }
    &node.name
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}::{segment}")
    }
}

/// Assign `qualified_name` on every node in `nodes`, using `edges`'s
/// `contains` edges to reconstruct each node's containment chain and
/// `module_path` as the root prefix for nodes with no container.
pub fn assign(nodes: &mut [CodeNode], edges: &[CodeEdge], module_path: &str) {
    let index_of: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect();

    // child id -> parent id, from `contains` edges only.
    let parent_of: HashMap<&str, &str> = edges
        .iter()
        .filter(|e| e.edge_type == "contains")
        .map(|e| (e.to_id.as_str(), e.from_id.as_str()))
        .collect();

    let mut memo: HashMap<String, String> = HashMap::new();
    let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();

    for id in &ids {
        if !memo.contains_key(id) {
            let mut stack = Vec::new();
            let qn = resolve(
                id,
                nodes,
                &index_of,
                &parent_of,
                module_path,
                &mut memo,
                &mut stack,
            );
            memo.insert(id.clone(), qn);
        }
    }

    for node in nodes.iter_mut() {
        if let Some(qn) = memo.get(&node.id) {
            node.qualified_name = qn.clone();
        }
    }
}

/// Resolve `id`'s fully-qualified name: its container's (recursively
/// resolved) qualified name, joined with `id`'s own containment segment —
/// or `module_path` joined with that segment if `id` has no container.
fn resolve(
    id: &str,
    nodes: &[CodeNode],
    index_of: &HashMap<String, usize>,
    parent_of: &HashMap<&str, &str>,
    module_path: &str,
    memo: &mut HashMap<String, String>,
    stack: &mut Vec<String>,
) -> String {
    if let Some(existing) = memo.get(id) {
        return existing.clone();
    }

    let Some(&idx) = index_of.get(id) else {
        // Not one of our nodes (shouldn't happen: `contains` edges are only
        // ever written between nodes created in the same file pass).
        return module_path.to_string();
    };
    let segment = containment_segment(&nodes[idx]);

    // Guard against a cycle in `contains` edges (shouldn't occur for
    // well-formed extractor output) so this can never recurse forever.
    let qn = if stack.iter().any(|s| s == id) {
        join(module_path, segment)
    } else {
        match parent_of.get(id) {
            Some(&parent_id) => {
                stack.push(id.to_string());
                let parent_qn = resolve(
                    parent_id, nodes, index_of, parent_of, module_path, memo, stack,
                );
                stack.pop();
                join(&parent_qn, segment)
            }
            None => join(module_path, segment),
        }
    };

    memo.insert(id.to_string(), qn.clone());
    qn
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn node(id: &str, name: &str, node_type: &str) -> CodeNode {
        CodeNode {
            id: id.to_string(),
            name: name.to_string(),
            node_type: node_type.to_string(),
            language: "rust".to_string(),
            start_line: None,
            end_line: None,
            content: None,
            metadata: Map::new(),
            qualified_name: String::new(),
        }
    }

    fn contains(from_id: &str, to_id: &str) -> CodeEdge {
        CodeEdge {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            to_name: None,
            to_type: None,
            edge_type: "contains".to_string(),
            confidence: "EXTRACTED".to_string(),
        }
    }

    // -- module_path: Rust --------------------------------------------

    #[test]
    fn rust_drops_src_and_extension() {
        assert_eq!(
            module_path("src/graph/dependencies.rs", "rust"),
            "graph::dependencies"
        );
    }

    #[test]
    fn rust_mod_rs_names_its_own_directory() {
        assert_eq!(
            module_path("src/index/extractors/mod.rs", "rust"),
            "index::extractors"
        );
    }

    #[test]
    fn rust_lib_and_main_are_crate_root() {
        assert_eq!(module_path("src/lib.rs", "rust"), "");
        assert_eq!(module_path("src/main.rs", "rust"), "");
    }

    #[test]
    fn rust_without_src_prefix_is_unaffected() {
        assert_eq!(module_path("index/node_id.rs", "rust"), "index::node_id");
    }

    // -- module_path: Go ------------------------------------------------

    #[test]
    fn go_package_is_directory_not_filename() {
        assert_eq!(module_path("pkg/server/handler.go", "go"), "pkg::server");
    }

    #[test]
    fn go_root_file_has_empty_package_path() {
        assert_eq!(module_path("main.go", "go"), "");
    }

    // -- module_path: Python ---------------------------------------------

    #[test]
    fn python_dotted_path_from_directories() {
        assert_eq!(
            module_path("mypkg/utils/helpers.py", "python"),
            "mypkg::utils::helpers"
        );
    }

    #[test]
    fn python_init_names_its_own_package() {
        assert_eq!(module_path("mypkg/__init__.py", "python"), "mypkg");
    }

    // -- module_path: fallback languages (java, c, cpp, ts, js) ---------

    #[test]
    fn java_drops_filename_no_maven_stripping() {
        // Filename dropped (like Go) since javac requires it to match the
        // public class name — no Maven `src/main/java` stripping though,
        // since that's a speculative build-layout guess, not a language rule.
        assert_eq!(
            module_path("src/main/java/com/example/Handler.java", "java"),
            "src::main::java::com::example"
        );
    }

    #[test]
    fn typescript_no_src_stripping() {
        assert_eq!(module_path("src/app.ts", "typescript"), "src::app");
    }

    #[test]
    fn c_cpp_full_path() {
        assert_eq!(module_path("lib/util.cpp", "cpp"), "lib::util");
    }

    // -- assign: containment chains --------------------------------------

    #[test]
    fn top_level_node_gets_module_prefix_only() {
        let mut nodes = vec![node("f1", "resolve_deps", "function")];
        let edges: Vec<CodeEdge> = vec![];
        assign(&mut nodes, &edges, "graph::dependencies");
        assert_eq!(nodes[0].qualified_name, "graph::dependencies::resolve_deps");
    }

    #[test]
    fn top_level_node_with_empty_module_path_is_bare_name() {
        let mut nodes = vec![node("f1", "main", "function")];
        let edges: Vec<CodeEdge> = vec![];
        assign(&mut nodes, &edges, "");
        assert_eq!(nodes[0].qualified_name, "main");
    }

    #[test]
    fn contained_node_chains_through_container() {
        let mut nodes = vec![
            node("c1", "Handler", "struct"),
            node("m1", "serve", "function"),
        ];
        let edges = vec![contains("c1", "m1")];
        assign(&mut nodes, &edges, "pkg::server");
        let by_id: Map<&str, &CodeNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        assert_eq!(by_id["c1"].qualified_name, "pkg::server::Handler");
        assert_eq!(by_id["m1"].qualified_name, "pkg::server::Handler::serve");
    }

    #[test]
    fn multi_level_containment_chain() {
        let mut nodes = vec![
            node("mod1", "tests", "module"),
            node("f1", "deterministic_across_calls", "function"),
        ];
        let edges = vec![contains("mod1", "f1")];
        assign(&mut nodes, &edges, "index::node_id");
        let by_id: Map<&str, &CodeNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        assert_eq!(
            by_id["f1"].qualified_name,
            "index::node_id::tests::deterministic_across_calls"
        );
    }

    #[test]
    fn impl_block_uses_bare_type_not_display_name() {
        let mut impl_meta = Map::new();
        impl_meta.insert("impl_for".to_string(), serde_json::json!("IndexingTier"));
        let mut impl_node = node("impl1", "impl FromStr for IndexingTier", "impl");
        impl_node.metadata = impl_meta;

        let mut nodes = vec![impl_node, node("m1", "from_str", "function")];
        let edges = vec![contains("impl1", "m1")];
        assign(&mut nodes, &edges, "index");

        let by_id: Map<&str, &CodeNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        // The impl's own qualified_name is also clean, since the same
        // containment segment is used everywhere it's referenced.
        assert_eq!(by_id["impl1"].qualified_name, "index::IndexingTier");
        assert_eq!(by_id["m1"].qualified_name, "index::IndexingTier::from_str");
    }

    #[test]
    fn cycle_guard_does_not_infinite_loop() {
        // Pathological input that should never occur from a real extractor
        // (a `contains` cycle) — must terminate rather than blow the stack.
        let mut nodes = vec![node("a", "A", "module"), node("b", "B", "module")];
        let edges = vec![contains("a", "b"), contains("b", "a")];
        assign(&mut nodes, &edges, "root");
        // No panic/hang is the primary assertion; values just need to be
        // well-formed non-empty strings.
        let by_id: Map<&str, &CodeNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        assert!(!by_id["a"].qualified_name.is_empty());
        assert!(!by_id["b"].qualified_name.is_empty());
    }
}
