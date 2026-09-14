# Verbatim development-history record

Quoted from the private development history with `git show -s 2cf8aeb`.

<!-- credo-lint:allow-fenced verbatim commit message quoted from the private development history -->
```
commit 2cf8aeb91587a33fdf30cd7c841c64aa6e5ab0ae
Author: Mike Schirtzinger <155995654+MikeSchirtzinger@users.noreply.github.com>
Date:   Sat Jul 11 03:20:39 2026 -0400

    perf(index): batch node/edge writes + resolver delete-and-reinsert (~3.5× faster)
    
    Full self-index (109 files, 868 nodes, 4k edges) drops 76.6s → ~20s, with
    byte-identical resolution output (729 resolved / 24 ambiguous / 3.3k
    unresolved). Two changes, both replacing per-record awaited round-trips:
    
    - store_parsed_file: one `INSERT INTO code_node [ ... ]` / `code_edge [ ... ]`
      per file instead of one awaited CREATE per node and per edge (~5k sequential
      round-trips). Rows are bound as typed `SurrealValue` structs, NOT
      serde_json::json! — SurrealDB maps a Rust `None` to `NONE`, whereas
      serde_json maps it to `NULL`, and the SCHEMAFULL `content option<string>`
      (`none | string`) rejects `NULL`. Store phase: ~40s-region → ~15s.
    
    - resolve_project write-back: delete-and-reinsert the whole unresolved set
      (1 DELETE + chunked bulk INSERT) instead of 4k+ UPDATE-by-compound-WHERE
      executions. An INSERT is an append with no per-row index lookup + rewrite,
      which is far cheaper on surrealkv. Resolver phase: 29.8s → 3.2s (9×). The
      incremental pass keeps its targeted per-edge UPDATE path (small affected
      set; correctness over throughput). Equivalence covered by the existing
      incremental-oracle property test.
    
    Also fixes a latent bug: the old edge loop discarded every insert's Result
    (`let _ = db.query(...)`); failures now propagate.
```
