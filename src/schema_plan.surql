-- codegraph plan schema: the roadmap as hyperedges over the code graph.
-- ns=codegraph, db=codegraph
--
-- Loaded by `src/plan/store.rs` (include_str! + one query), NOT by
-- `db::init_schema`, which runs only `schema.surql`. `plan sync` runs this
-- before its first write, so a store that has only ever been indexed still
-- gains the plan tables the moment a roadmap is synced into it.
--
-- Every definition uses OVERWRITE rather than IF NOT EXISTS, matching
-- `schema.surql`. The difference is load-bearing: IF NOT EXISTS leaves a
-- previously defined table alone, so a store created by an older build
-- would keep that build's field list forever and silently reject a row
-- carrying a field added later. OVERWRITE re-applies the current
-- definition on every run, which is what makes running this on every sync
-- correct rather than merely harmless. OVERWRITE redefines; it does not
-- delete records, which is the same property `code_node` already relies on
-- across incremental re-indexes.
--
-- This is the standard bipartite encoding of a hypergraph (board section
-- 3.2). `work_item` IS the hyperedge; `work_touch` is its incidence list,
-- one row per (item, code entity). A `glob:` touch that matches four files
-- is four rows sharing one `raw`, which is what keeps collision detection
-- and blast radius a plain set intersection rather than a special case.

-- ============================================================================
-- work_plane: one row per plane
-- ============================================================================
DEFINE TABLE OVERWRITE work_plane SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON work_plane TYPE string;
-- The plane id as written in the file.
DEFINE FIELD OVERWRITE plane_id ON work_plane TYPE string;
-- `work_plane:<project>__<plane_id>`, from `plan::plane_record_id`. A plain
-- string field, not a SurrealDB record key, for the same reason
-- `code_node.node_id` is: it never has to be quoted and it survives being
-- copied into a JSON payload or an agent prompt unchanged.
DEFINE FIELD OVERWRITE node_id ON work_plane TYPE string;
DEFINE FIELD OVERWRITE title ON work_plane TYPE string;
-- planned | active | done | abandoned, verbatim from `plan::model::Status`.
DEFINE FIELD OVERWRITE status ON work_plane TYPE string;
-- now | next | later, verbatim from `plan::model::Horizon`.
DEFINE FIELD OVERWRITE horizon ON work_plane TYPE string;
DEFINE FIELD OVERWRITE summary ON work_plane TYPE option<string>;
-- Position in the file. File order is the author's order and every listing
-- preserves it, so it has to survive the round trip through a store whose
-- row order is not defined.
DEFINE FIELD OVERWRITE ordinal ON work_plane TYPE int;
DEFINE FIELD OVERWRITE synced_at ON work_plane TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_wp_project ON work_plane FIELDS project_id;
DEFINE INDEX OVERWRITE idx_wp_project_plane ON work_plane FIELDS project_id, plane_id UNIQUE;

-- ============================================================================
-- work_item: one row per hyperedge
-- ============================================================================
DEFINE TABLE OVERWRITE work_item SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON work_item TYPE string;
-- The work item id as written in the file, unique across the whole file.
DEFINE FIELD OVERWRITE item_id ON work_item TYPE string;
-- `work_item:<project>__<item_id>`, from `plan::item_record_id`.
DEFINE FIELD OVERWRITE node_id ON work_item TYPE string;
-- The owning plane's `plane_id`, matching board section 3.2's `plane` field.
DEFINE FIELD OVERWRITE plane ON work_item TYPE string;
-- The owning plane's `node_id`, so a join needs no string assembly.
DEFINE FIELD OVERWRITE plane_node_id ON work_item TYPE string;
DEFINE FIELD OVERWRITE title ON work_item TYPE string;
-- feature | fix | perf | refactor | docs | research.
DEFINE FIELD OVERWRITE kind ON work_item TYPE string;
DEFINE FIELD OVERWRITE status ON work_item TYPE string;
-- Item ids this item waits on. Validated to exist and to be acyclic by
-- `plan::schema::validate` before anything is written, so these are never
-- dangling by the time they land here.
DEFINE FIELD OVERWRITE depends_on ON work_item TYPE array<string>;
DEFINE FIELD OVERWRITE spec ON work_item TYPE option<string>;
DEFINE FIELD OVERWRITE notes ON work_item TYPE option<string>;
DEFINE FIELD OVERWRITE ordinal ON work_item TYPE int;
DEFINE FIELD OVERWRITE synced_at ON work_item TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_wi_project ON work_item FIELDS project_id;
DEFINE INDEX OVERWRITE idx_wi_project_item ON work_item FIELDS project_id, item_id UNIQUE;
DEFINE INDEX OVERWRITE idx_wi_project_plane ON work_item FIELDS project_id, plane;

-- ============================================================================
-- work_touch: one row per (item, code entity) incidence
-- ============================================================================
DEFINE TABLE OVERWRITE work_touch SCHEMAFULL;
DEFINE FIELD OVERWRITE project_id ON work_touch TYPE string;
-- The owning item's `node_id`. Named `item` to match board section 3.2.
DEFINE FIELD OVERWRITE item ON work_touch TYPE string;
-- The owning item's bare id, so a reader never has to parse `item`.
DEFINE FIELD OVERWRITE item_id ON work_touch TYPE string;
-- file | symbol | glob, verbatim from `plan::model::Selector`.
DEFINE FIELD OVERWRITE selector ON work_touch TYPE string;
-- The selector's value exactly as written in the file. Never normalized, so
-- an error message can quote the file back to its author.
DEFINE FIELD OVERWRITE raw ON work_touch TYPE string;
-- What the selector bound to, set only on a RESOLVED row. Which namespace
-- it is in is decided by `selector`, which is always present: a `symbol:`
-- touch binds to a `code_node.node_id`, a `file:` or `glob:` touch binds to
-- the repo relative path it matched. A path has no node of its own in this
-- graph, and inventing a synthetic one would put an id in the table that
-- nothing else in the store can join against.
DEFINE FIELD OVERWRITE to_id ON work_touch TYPE option<string>;
-- RESOLVED | AMBIGUOUS | UNRESOLVED, byte for byte the vocabulary
-- `code_edge.confidence` already carries, so `work_touch` and `code_edge`
-- rows can be filtered by the same predicate.
DEFINE FIELD OVERWRITE confidence ON work_touch TYPE string;
-- Surviving candidate node ids, AMBIGUOUS only, sorted. Never used to
-- smuggle a guess into a RESOLVED row.
DEFINE FIELD OVERWRITE candidates ON work_touch TYPE option<array<string>>;
-- Why the selector did not bind, on anything other than RESOLVED. One of
-- no_such_file_in_index, no_such_file, no_such_symbol, no_glob_match,
-- ambiguous_candidates. The first two are a distinction an agent acts on:
-- a file that is on disk but not in the index means the index is stale or
-- the path is excluded, while a file that is on neither means the plan is.
DEFINE FIELD OVERWRITE reason ON work_touch TYPE option<string>;
-- One sentence naming what was looked for and what was found, for the
-- human who has to fix the plan.
DEFINE FIELD OVERWRITE detail ON work_touch TYPE option<string>;
-- Whether the code graph holds a row for this target. Set on `file:` and
-- `glob:` rows only, where the target is a path. A path can be perfectly
-- real and carry no node at all: a spec, a README, a `.surql` schema. Such
-- a touch is RESOLVED, because the thing it names exists, with `indexed`
-- false, because nothing in the graph can be reached from it. Folding that
-- into the confidence instead would report a roadmap that cites its own
-- specs as permanently stale. `NONE` on a `symbol:` row, where the target
-- is a node id and the question does not arise.
DEFINE FIELD OVERWRITE indexed ON work_touch TYPE option<bool>;
-- Position of this row within its item, over every expanded row.
DEFINE FIELD OVERWRITE ordinal ON work_touch TYPE int;
-- Position of the `touches:` entry this row came from, within its item.
-- Rows sharing one value are the expansion of a single selector, which is
-- how a per selector tally is recovered from a per incidence table.
DEFINE FIELD OVERWRITE selector_ordinal ON work_touch TYPE int;
DEFINE FIELD OVERWRITE synced_at ON work_touch TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_wt_project ON work_touch FIELDS project_id;
-- The forward incidence lookup: every entity one item touches.
DEFINE INDEX OVERWRITE idx_wt_item ON work_touch FIELDS item;
-- The inverse incidence lookup, which is the query the collaboration story
-- rests on: every item that touches one entity.
DEFINE INDEX OVERWRITE idx_wt_to ON work_touch FIELDS project_id, to_id;
