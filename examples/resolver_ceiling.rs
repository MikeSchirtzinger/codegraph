//! Read-only spike: measure the resolver's UNRESOLVED breakdown *by cause*
//! on a self-index of this repo, and compute the *intra-project* resolution
//! rate — the denominator that actually matters for the gate (external
//! stdlib/third-party calls are structurally unresolvable in a project-scoped
//! graph and shouldn't count against us).
//!
//! Indexes the crate into an in-memory SurrealDB (mem://) with the real
//! extractor + resolver, then classifies every non-structural name-edge.
//!
//! Two facts from `src/index/resolve.rs` drive the classification:
//!  - A *bare* edge can only end UNRESOLVED when its candidate pool
//!    (bare-name + node_type + language family) is empty — R5/terminal binds
//!    over the full pool. So every recoverable false-negative is a *qualified*
//!    edge where R1+R2 missed but a bare-name candidate exists.
//!  - R1/R2 match the edge's `to_name` literally against `qualified_name`s.
//!    They have no module-relative resolution, so `crate::`/`self::`/`super::`
//!    prefixes never match. The "fix-(a)" set below is exactly the qualified
//!    recoverable edges that a module-prefix-stripping R1/R2 *would* bind —
//!    which cleanly separates real intra-crate path misses from the murky
//!    receiver-method band (`ctx.add_node`) and stdlib collisions (`Vec::new`).
//!
//! Run: `cargo run --example resolver_ceiling`

use std::collections::HashMap;

use codegraph::index::resolve::{bare_name, normalize_separators};
use codegraph::index::{index_project, IndexConfig, IndexingTier};

/// Mirror of the private `resolve::language_family` closed table.
fn language_family(language: &str) -> &str {
    match language {
        "c" | "cpp" => "c-cpp",
        "javascript" | "typescript" => "typescript",
        other => other,
    }
}

/// Strip any leading run of Rust module-path keywords (`crate::`, `self::`,
/// `super::`, the latter repeatable) — the transform a module-relative R1/R2
/// would apply before matching. Returns the module-relative remainder.
fn strip_module_prefix(mut n: &str) -> &str {
    loop {
        if let Some(r) = n.strip_prefix("crate::") {
            n = r;
        } else if let Some(r) = n.strip_prefix("self::") {
            n = r;
        } else if let Some(r) = n.strip_prefix("super::") {
            n = r;
        } else {
            return n;
        }
    }
}

fn extract_str(obj: &surrealdb_types::Object, key: &str) -> String {
    obj.get(key)
        .and_then(|v| match v {
            surrealdb_types::Value::String(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_writer(std::io::stderr)
        .init();

    let db = codegraph::db::connect(Some("mem://")).await?;
    codegraph::db::init_schema(&db).await?;

    let config = IndexConfig {
        project_id: "self".to_string(),
        root_path: std::path::PathBuf::from("."),
        tier: IndexingTier::Full,
        languages: None,
        force: true,
    };
    let res = index_project(&db, &config).await?;
    eprintln!(
        "indexed: {} files, {} nodes, {} edges",
        res.files_indexed, res.nodes_created, res.edges_created,
    );

    // --- load nodes ---
    let mut resp = db
        .query("SELECT node_id, name, node_type, language, qualified_name FROM code_node WHERE project_id = 'self'")
        .await?;
    let node_rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    let mut lang_of: HashMap<String, String> = HashMap::new();
    // (bare name, node_type, family) -> the qualified_names of the candidates
    // in that pool. The resolver's exact candidate-pool key.
    let mut pool: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for v in &node_rows {
        let surrealdb_types::Value::Object(obj) = v else { continue };
        let id = extract_str(obj, "node_id");
        let name = extract_str(obj, "name");
        let ntype = extract_str(obj, "node_type");
        let lang = extract_str(obj, "language");
        let qname = extract_str(obj, "qualified_name");
        if id.is_empty() {
            continue;
        }
        lang_of.insert(id, lang.clone());
        pool.entry((name, ntype, language_family(&lang).to_string()))
            .or_default()
            .push(qname);
    }

    // --- load the resolver's edge universe ---
    let mut resp = db
        .query(
            "SELECT from_id, to_name, to_type, edge_type, confidence FROM code_edge \
             WHERE project_id = 'self' AND confidence != 'EXTRACTED' AND edge_type != 'file_ref'",
        )
        .await?;
    let edge_rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    let mut total = 0usize;
    let (mut resolved, mut ambiguous, mut unresolved) = (0usize, 0usize, 0usize);

    // UNRESOLVED cause buckets
    let mut bare_external = 0usize;
    let mut qual_external = 0usize;
    let mut recoverable = 0usize; // qualified, pool non-empty (upper bound)
    // fix-(a): qualified recoverable that a module-relative R1/R2 would bind
    let mut fix_a_unique = 0usize; // exactly one module-relative candidate
    let mut fix_a_ambiguous = 0usize; // >1 module-relative candidate
    // murky remainder of recoverable = receiver-method band + stdlib collisions
    let mut murky = 0usize;
    let mut fix_a_names: HashMap<String, usize> = HashMap::new();
    let mut murky_names: HashMap<String, usize> = HashMap::new();

    for v in &edge_rows {
        let surrealdb_types::Value::Object(obj) = v else { continue };
        let from_id = extract_str(obj, "from_id");
        let to_name = extract_str(obj, "to_name");
        let to_type = extract_str(obj, "to_type");
        let confidence = extract_str(obj, "confidence");
        total += 1;
        match confidence.as_str() {
            "RESOLVED" => {
                resolved += 1;
                continue;
            }
            "AMBIGUOUS" => {
                ambiguous += 1;
                continue;
            }
            _ => unresolved += 1,
        }

        let src_lang = lang_of.get(&from_id).cloned().unwrap_or_default();
        let family = language_family(&src_lang).to_string();
        let normalized = normalize_separators(&to_name);
        let bare = bare_name(&normalized).to_string();
        let is_qualified = normalized.contains("::");
        let candidates = pool.get(&(bare.clone(), to_type.clone(), family)).map(Vec::as_slice).unwrap_or(&[]);

        if candidates.is_empty() {
            if is_qualified {
                qual_external += 1;
            } else {
                bare_external += 1;
            }
            continue;
        }
        // Pool non-empty. Bare-with-candidates can't be UNRESOLVED (terminal
        // binds over the full pool) — so this is qualified-recoverable.
        recoverable += 1;

        // Would a module-relative R1/R2 bind it? Strip crate/self/super, then
        // test exact-or-suffix against the candidates' qualified_names.
        let rel = strip_module_prefix(&normalized);
        let suffix = format!("::{rel}");
        let hits = candidates
            .iter()
            .filter(|qn| qn.as_str() == rel || qn.ends_with(&suffix))
            .count();
        match hits {
            0 => {
                murky += 1;
                *murky_names.entry(to_name.clone()).or_default() += 1;
            }
            1 => {
                fix_a_unique += 1;
                *fix_a_names.entry(to_name.clone()).or_default() += 1;
            }
            _ => {
                fix_a_ambiguous += 1;
                *fix_a_names.entry(to_name.clone()).or_default() += 1;
            }
        }
    }

    let pct = |n: usize, d: usize| if d == 0 { 0.0 } else { 100.0 * n as f64 / d as f64 };

    println!("\n===== RESOLVER CEILING (self-index) =====");
    println!("edges considered : {total}");
    println!("  RESOLVED   : {resolved:>5}  ({:.1}%)", pct(resolved, total));
    println!("  AMBIGUOUS  : {ambiguous:>5}  ({:.1}%)", pct(ambiguous, total));
    println!("  UNRESOLVED : {unresolved:>5}  ({:.1}%)", pct(unresolved, total));

    println!("\n----- UNRESOLVED by cause ({unresolved} total) -----");
    let external = bare_external + qual_external;
    println!(
        "  EXTERNAL (no project candidate; correct)  : {external:>5}  ({:.1}% of unresolved)",
        pct(external, unresolved)
    );
    println!("        bare      : {bare_external}");
    println!("        qualified : {qual_external}");
    println!(
        "  RECOVERABLE (candidate exists; upper bound): {recoverable:>5}  ({:.1}%)",
        pct(recoverable, unresolved)
    );
    println!(
        "     fix-(a) module-path, would bind         : {}   (unique {fix_a_unique} / ambiguous {fix_a_ambiguous})",
        fix_a_unique + fix_a_ambiguous
    );
    println!("     murky (receiver-method / stdlib collide) : {murky}");

    println!("\n----- THE RIGHT BASELINE: intra-project resolution rate -----");
    // Provably project-targeted = resolved + ambiguous + fix-(a). (The murky
    // band is excluded — mostly stdlib collisions we must NOT bind.)
    let intra_clean = resolved + ambiguous + fix_a_unique + fix_a_ambiguous;
    // Widest reading: count the whole recoverable band as intra-project.
    let intra_wide = resolved + ambiguous + recoverable;
    println!(
        "  clean denom (resolved+ambiguous+fix-a) = {intra_clean}  -> resolved = {:.1}%",
        pct(resolved, intra_clean)
    );
    println!(
        "  wide  denom (+ murky band)             = {intra_wide}  -> resolved = {:.1}%",
        pct(resolved, intra_wide)
    );
    println!(
        "  => intra-project resolution is {:.0}-{:.0}% today (vs the {:.0}% overall headline)",
        pct(resolved, intra_wide),
        pct(resolved, intra_clean),
        pct(resolved, total),
    );
    println!(
        "  fix-(a) alone lifts clean intra-project resolution to {:.1}%",
        pct(resolved + fix_a_unique, intra_clean)
    );

    let top = |label: &str, m: &HashMap<String, usize>, n: usize| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        println!("\n  top {label}:");
        for (name, count) in v.into_iter().take(n) {
            println!("    {count:>4}  {name}");
        }
    };
    top("fix-(a) recoverable to_names (module-path misses)", &fix_a_names, 20);
    top("murky recoverable to_names (receiver-method / stdlib)", &murky_names, 15);

    Ok(())
}
