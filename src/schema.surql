-- codegraph schema
-- ns=codegraph, db=codegraph
-- All definitions idempotent via IF NOT EXISTS / OVERWRITE
--
-- Generic multi-language code graph, isolated by project_id.
-- Node table:  code_node
-- Edge table:  code_edge (calls, imports, references, implements, contains)
-- Support:     file_metadata (incremental indexing), project_registry (multi-tenant)

-- ============================================================================
-- GENERIC CODE GRAPH (works on any repo, isolated by project_id)
-- ============================================================================

DEFINE TABLE OVERWRITE code_node SCHEMAFULL;
DEFINE FIELD OVERWRITE node_id ON code_node TYPE option<string>;
DEFINE FIELD OVERWRITE project_id ON code_node TYPE string;
DEFINE FIELD OVERWRITE name ON code_node TYPE string;
DEFINE FIELD OVERWRITE node_type ON code_node TYPE string;
DEFINE FIELD OVERWRITE language ON code_node TYPE string;
DEFINE FIELD OVERWRITE file_path ON code_node TYPE string;
DEFINE FIELD OVERWRITE start_line ON code_node TYPE option<int>;
DEFINE FIELD OVERWRITE end_line ON code_node TYPE option<int>;
DEFINE FIELD OVERWRITE content ON code_node TYPE option<string>;
DEFINE FIELD OVERWRITE metadata ON code_node TYPE option<object> FLEXIBLE;
-- Language-mechanical module path + containment chain + name, `::`
-- separated (e.g. `graph::dependencies::resolve_deps`, `pkg::server::Handler::serve`).
-- Computed once per file from its path + `contains` edges — see
-- `index::qualified_name`. Used by the resolver (R1) for exact/suffix
-- matching (rule cascade R1/R2).
DEFINE FIELD OVERWRITE qualified_name ON code_node TYPE string;
DEFINE FIELD OVERWRITE created_at ON code_node TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_cn_project ON code_node FIELDS project_id;
DEFINE INDEX OVERWRITE idx_cn_project_file ON code_node FIELDS project_id, file_path;
DEFINE INDEX OVERWRITE idx_cn_project_type ON code_node FIELDS project_id, node_type;
DEFINE INDEX OVERWRITE idx_cn_name ON code_node FIELDS name;
DEFINE INDEX OVERWRITE idx_cn_qualified_name ON code_node FIELDS project_id, qualified_name;

-- Generic code edges (regular records, not RELATE — simpler, avoids record
-- link resolution issues for cross-file references that may not exist yet).
-- edge_type values: calls, imports, references, implements, contains,
-- member_of, file_ref (the last derived, file-level — see below).
-- confidence values pre-resolution: EXTRACTED (real to_id, known at parse
-- time — e.g. `contains`, or a same-file macro-invocation `calls` edge),
-- INFERRED (a name-edge, to_id = '', awaiting the resolver).
--
-- Name-based edges (target looked up by name, not known at parse time — a
-- calls/member_of/implements reference that may live in another file) are
-- written with to_id = '' plus to_name/to_type set. The R1 resolver
-- (index::resolve) then runs a deterministic cascade over every to_id = ''
-- row and overwrites confidence to RESOLVED (to_id filled in) | AMBIGUOUS
-- (candidates filled in) | UNRESOLVED (neither — an external/stdlib call,
-- or a stale reference to a renamed/removed symbol). Graph queries
-- (calls/deps/rdeps/circular/impact — see graph::*) read to_id/confidence
-- directly at query time (R2); hub_nodes/coupling still match by (to_name,
-- to_type), a coarser aggregate metric that doesn't need per-edge identity.
DEFINE TABLE OVERWRITE code_edge SCHEMALESS;
-- Forward traversal (deps, calls) keys on from_id. Reverse traversal
-- (rdeps, impact) keys on to_id, now that the resolver gives most edges a
-- real, distinct target id — post-resolution, only the AMBIGUOUS/
-- UNRESOLVED minority still share to_id = '', so this is no longer the
-- giant single-key bucket it would have been pre-resolution. (to_name,
-- to_type) stays indexed for hub_nodes/coupling's coarser aggregate.
DEFINE INDEX OVERWRITE idx_ce_from ON code_edge FIELDS project_id, from_id;
DEFINE INDEX OVERWRITE idx_ce_to ON code_edge FIELDS project_id, to_id;
DEFINE INDEX OVERWRITE idx_ce_toname ON code_edge FIELDS project_id, to_name, to_type;

-- Resolution-layer fields, materialized by the R1 resolver pass (not by
-- extractors) — landed here so R1 has a validated target to write into.
-- `confidence` is written by extractors today with EXTRACTED|INFERRED (see
-- the comment above); once the resolver runs, it overwrites `confidence`
-- project-wide with RESOLVED|AMBIGUOUS|UNRESOLVED instead — the field is
-- typed as a plain string so both eras validate. `resolved_by` (r1..r6
-- provenance tag), `resolution_gen`, and `candidates` (AMBIGUOUS only) are
-- unset until the resolver runs.
DEFINE FIELD OVERWRITE confidence ON code_edge TYPE string;
DEFINE FIELD OVERWRITE resolved_by ON code_edge TYPE option<string>;
DEFINE FIELD OVERWRITE resolution_gen ON code_edge TYPE option<int>;
DEFINE FIELD OVERWRITE candidates ON code_edge TYPE option<array<string>>;

-- Per-file indexing state for incremental rebuilds
DEFINE TABLE OVERWRITE file_metadata SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON file_metadata TYPE string;
DEFINE FIELD OVERWRITE file_path ON file_metadata TYPE string;
DEFINE FIELD OVERWRITE content_hash ON file_metadata TYPE string;
DEFINE FIELD OVERWRITE file_size ON file_metadata TYPE option<int>;
DEFINE FIELD OVERWRITE language ON file_metadata TYPE option<string>;
DEFINE FIELD OVERWRITE node_count ON file_metadata TYPE option<int>;
DEFINE FIELD OVERWRITE last_indexed_at ON file_metadata TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_fm_unique ON file_metadata FIELDS project_id, file_path UNIQUE;

-- Deletion tracking (spine step 2 kill-test closure — design doc §10 option 1).
-- One row per symbol *definition* that an incremental re-index observed
-- disappearing from a file (rename, delete, move-out, or whole-file delete).
-- Written by index::index_project's capture step; healed (row deleted) when a
-- later index run re-defines the same (qualified_name, file_path); wiped
-- project-wide on `--force` (a force run re-baselines "what exists" without
-- diffing, so deletion history is deliberately reset — documented degradation).
--
-- Read by facade::impact_verdict_for_paths, which unions these names into the
-- symbol set it queries so `find_stale_references` can fire for names no
-- longer defined in the touched files. Rows are query *targets*, never
-- verdicts: a stale row for a since-fixed name matches zero UNRESOLVED edges
-- and contributes nothing, which is what keeps this false-positive-free.
-- `import` nodes are excluded at capture (an import statement is a reference,
-- not a definition — same exclusion the facade applies to touched symbols).
DEFINE TABLE OVERWRITE deleted_symbol SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON deleted_symbol TYPE string;
DEFINE FIELD OVERWRITE name ON deleted_symbol TYPE string;
DEFINE FIELD OVERWRITE qualified_name ON deleted_symbol TYPE string;
DEFINE FIELD OVERWRITE node_type ON deleted_symbol TYPE string;
DEFINE FIELD OVERWRITE file_path ON deleted_symbol TYPE string;
DEFINE FIELD OVERWRITE deleted_at ON deleted_symbol TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_ds_project_file ON deleted_symbol FIELDS project_id, file_path;
DEFINE INDEX OVERWRITE idx_ds_project_qn ON deleted_symbol FIELDS project_id, qualified_name;

-- Structural fingerprints (lane 3 — structural-fingerprints branch).
-- One row per (qualified_name, file_path) definition key: the FNV-1a hash of
-- the canonical certificate of the symbol's rooted 1-hop typed neighborhood
-- (see `src/canon.rs` + `src/index/fingerprint.rs`). Names never enter the
-- hash — structure, edge types, and node kinds only — so a pure rename
-- preserves it. Written by `index::index_project`'s fingerprint pass (step
-- 5b, after resolution) on BOTH full and incremental runs: every run shifts
-- `hash` into `prev_hash` (exactly one prior generation is retained), and a
-- symbol that disappeared keeps a one-generation tombstone (`hash = NONE`)
-- so `facade::structural_delta_for_paths` can classify removals and pair
-- renames. Wiped project-wide on `--force`, like `deleted_symbol` — a force
-- run re-baselines and fingerprint history deliberately resets (documented
-- degradation). `edge_count`/`name_edge_count` describe the neighborhood
-- (total edges / non-`contains` edges) for `codegraph clones` thresholds.
DEFINE TABLE OVERWRITE fingerprint SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON fingerprint TYPE string;
DEFINE FIELD OVERWRITE qualified_name ON fingerprint TYPE string;
DEFINE FIELD OVERWRITE file_path ON fingerprint TYPE string;
DEFINE FIELD OVERWRITE node_type ON fingerprint TYPE string;
DEFINE FIELD OVERWRITE hash ON fingerprint TYPE option<string>;
DEFINE FIELD OVERWRITE edge_count ON fingerprint TYPE int;
DEFINE FIELD OVERWRITE name_edge_count ON fingerprint TYPE int;
DEFINE FIELD OVERWRITE generation ON fingerprint TYPE int;
DEFINE FIELD OVERWRITE prev_hash ON fingerprint TYPE option<string>;
DEFINE FIELD OVERWRITE prev_generation ON fingerprint TYPE option<int>;
DEFINE FIELD OVERWRITE updated_at ON fingerprint TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_fp_project_file ON fingerprint FIELDS project_id, file_path;
DEFINE INDEX OVERWRITE idx_fp_project_key ON fingerprint FIELDS project_id, qualified_name, file_path UNIQUE;
DEFINE INDEX OVERWRITE idx_fp_project_hash ON fingerprint FIELDS project_id, hash;

-- Project registry for multi-tenant isolation
DEFINE TABLE OVERWRITE project_registry SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON project_registry TYPE string;
DEFINE FIELD OVERWRITE name ON project_registry TYPE string;
DEFINE FIELD OVERWRITE root_path ON project_registry TYPE string;
DEFINE FIELD OVERWRITE primary_language ON project_registry TYPE option<string>;
DEFINE FIELD OVERWRITE node_count ON project_registry TYPE option<int>;
DEFINE FIELD OVERWRITE edge_count ON project_registry TYPE option<int>;
DEFINE FIELD OVERWRITE file_count ON project_registry TYPE option<int>;
DEFINE FIELD OVERWRITE last_indexed_at ON project_registry TYPE option<datetime>;
DEFINE FIELD OVERWRITE created_at ON project_registry TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_pr_project_id ON project_registry FIELDS project_id UNIQUE;
