//! Structural clone detection over persisted fingerprints.
//!
//! Two symbols whose rooted 1-hop neighborhood certificates hash identically
//! have (up to 64-bit FNV collision) the same structure: same edge types,
//! same directions, same neighbor kinds, same multiplicities — regardless of
//! any name involved. `codegraph clones` groups them.
//!
//! EXCLUSIONS (what is deliberately NOT reported, and why):
//! * Tombstone rows (`hash = NONE`) — not live symbols.
//! * Neighborhoods smaller than `min_edges` (CLI `--min-edges`, default 3):
//!   a leaf function with one caller is structurally identical to every
//!   other such function; below a minimum amount of structure, "identical
//!   certificate" carries no clone signal.
//! * Symbols whose neighborhood has zero non-`contains` edges
//!   (`name_edge_count == 0`): a pure containment shape — e.g. every module
//!   holding N same-kind children matches every other — is arity, not
//!   behavior; grouping those is trivially-same-shape noise at any size.
//! * Groups with a single member — a clone needs a counterpart.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::graph::get_str;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneMember {
    /// Bare symbol name (display; from `code_node`, `""` if the node row is
    /// gone mid-flight).
    pub name: String,
    pub qualified_name: String,
    pub node_type: String,
    pub file_path: String,
    pub start_line: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneGroup {
    /// The shared certificate hash (hex, as persisted).
    pub fingerprint: String,
    /// Neighborhood size — identical certificates have identical edge
    /// counts, so this is a group property, not a member one.
    pub edge_count: i64,
    pub members: Vec<CloneMember>,
}

/// Group live fingerprints with identical certificates, above the
/// minimum-structure threshold. Largest groups first (then by fingerprint
/// for determinism); members ordered by (file, line, qualified name).
pub async fn find_clone_groups(
    db: &Surreal<Any>,
    project_id: &str,
    min_edges: usize,
) -> Result<Vec<CloneGroup>> {
    // Display metadata for members: bare name + start_line per
    // (qualified_name, file_path) key. A key with several nodes (struct +
    // impl) displays its first-by-line member.
    let mut resp = db
        .query(
            "SELECT name, qualified_name, file_path, start_line FROM code_node \
             WHERE project_id = $pid AND node_type NOT IN ['import', 'macro_call']",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading code_node display metadata failed")?;
    let node_rows: Vec<surrealdb_types::Value> = resp.take(0)?;
    let mut display: BTreeMap<(String, String), (String, Option<i64>)> = BTreeMap::new();
    for v in &node_rows {
        let surrealdb_types::Value::Object(obj) = v else { continue };
        let key = (get_str(obj, "qualified_name"), get_str(obj, "file_path"));
        let line = obj.get("start_line").and_then(|v| match v {
            surrealdb_types::Value::Number(n) => n.to_int(),
            _ => None,
        });
        let rank = |l: Option<i64>| l.unwrap_or(i64::MAX);
        let replace = match display.get(&key) {
            None => true,
            Some((_, old)) => rank(line) < rank(*old),
        };
        if replace {
            display.insert(key, (get_str(obj, "name"), line));
        }
    }

    let mut resp = db
        .query(
            "SELECT qualified_name, file_path, node_type, hash, edge_count, name_edge_count \
             FROM fingerprint WHERE project_id = $pid",
        )
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading fingerprints failed")?;
    let fp_rows: Vec<surrealdb_types::Value> = resp.take(0)?;

    let mut by_hash: BTreeMap<String, (i64, Vec<CloneMember>)> = BTreeMap::new();
    for v in &fp_rows {
        let surrealdb_types::Value::Object(obj) = v else { continue };
        let hash = get_str(obj, "hash");
        if hash.is_empty() {
            continue; // tombstone — not a live symbol
        }
        let get_int = |k: &str| {
            obj.get(k)
                .and_then(|v| match v {
                    surrealdb_types::Value::Number(n) => n.to_int(),
                    _ => None,
                })
                .unwrap_or(0)
        };
        let edge_count = get_int("edge_count");
        if edge_count < min_edges as i64 {
            continue; // below the minimum-structure threshold
        }
        if get_int("name_edge_count") == 0 {
            continue; // pure containment — trivially same-shape at any size
        }
        let qualified_name = get_str(obj, "qualified_name");
        let file_path = get_str(obj, "file_path");
        let (name, start_line) = display
            .get(&(qualified_name.clone(), file_path.clone()))
            .cloned()
            .unwrap_or_default();
        by_hash.entry(hash).or_insert((edge_count, Vec::new())).1.push(CloneMember {
            name,
            qualified_name,
            node_type: get_str(obj, "node_type"),
            file_path,
            start_line,
        });
    }

    let mut groups: Vec<CloneGroup> = by_hash
        .into_iter()
        .filter(|(_, (_, members))| members.len() >= 2)
        .map(|(fingerprint, (edge_count, mut members))| {
            members.sort_by(|a, b| {
                (&a.file_path, a.start_line, &a.qualified_name)
                    .cmp(&(&b.file_path, b.start_line, &b.qualified_name))
            });
            CloneGroup { fingerprint, edge_count, members }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
    });
    Ok(groups)
}
