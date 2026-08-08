//! Deterministic node ID generation.
//!
//! Node IDs are SHA-256 hashes of `(project_id, file_path, name, node_type, start_line)`,
//! truncated to a 32-char hex string. Same code entity gets the same ID across re-indexing
//! runs, enabling clean upsert semantics.

use sha2::{Digest, Sha256};

/// Generate a deterministic node ID from its identity tuple.
pub fn generate_node_id(
    project_id: &str,
    file_path: &str,
    name: &str,
    node_type: &str,
    start_line: Option<u32>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b"|");
    hasher.update(file_path.as_bytes());
    hasher.update(b"|");
    hasher.update(name.as_bytes());
    hasher.update(b"|");
    hasher.update(node_type.as_bytes());
    hasher.update(b"|");
    hasher.update(start_line.unwrap_or(0).to_le_bytes());
    let hash = hasher.finalize();
    // Truncate to 16 bytes (32 hex chars) — collision probability is negligible
    hex::encode(&hash[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_across_calls() {
        let a = generate_node_id("proj1", "src/main.rs", "main", "function", Some(10));
        let b = generate_node_id("proj1", "src/main.rs", "main", "function", Some(10));
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn different_projects_different_ids() {
        let a = generate_node_id("proj1", "src/main.rs", "main", "function", Some(10));
        let b = generate_node_id("proj2", "src/main.rs", "main", "function", Some(10));
        assert_ne!(a, b);
    }

    #[test]
    fn different_lines_different_ids() {
        let a = generate_node_id("proj1", "src/main.rs", "Foo", "struct", Some(10));
        let b = generate_node_id("proj1", "src/main.rs", "Foo", "struct", Some(20));
        assert_ne!(a, b);
    }
}
