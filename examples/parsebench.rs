//! Times ONLY the tree-sitter parse+extract phase over a directory tree,
//! with no database in the loop. Answers: what share of `codegraph index`
//! is actually parsing?
use std::path::Path;
use std::time::Instant;
use codegraph::index::{parser::parse_file, IndexingTier};

fn main() {
    let root = std::env::args().nth(1).expect("usage: parsebench <dir>");
    let root = Path::new(&root);
    let mut files = Vec::new();
    for e in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() {
            let p = e.path();
            if p.components().any(|c| c.as_os_str() == "target") { continue; }
            if matches!(p.extension().and_then(|x| x.to_str()),
                        Some("rs"|"ts"|"tsx"|"js"|"py"|"go"|"java"|"c"|"h"|"cpp"|"hpp")) {
                files.push(p.to_path_buf());
            }
        }
    }
    let bytes: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let t0 = Instant::now();
    let (mut nodes, mut edges, mut ok) = (0usize, 0usize, 0usize);
    for p in &files {
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
        if let Ok(pf) = parse_file(p, &rel, "profbench", IndexingTier::Full) {
            nodes += pf.nodes.len(); edges += pf.edges.len(); ok += 1;
        }
    }
    let el = t0.elapsed();
    println!("files={} parsed_ok={} bytes={} ({:.1} MiB)", files.len(), ok, bytes, bytes as f64/1048576.0);
    println!("nodes={} edges={}", nodes, edges);
    println!("parse+extract wall = {:.3} s   -> {:.1} MiB/s, {:.0} files/s",
             el.as_secs_f64(), (bytes as f64/1048576.0)/el.as_secs_f64(), ok as f64/el.as_secs_f64());
}
