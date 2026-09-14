//! The `work_plane` / `work_item` / `work_touch` tables. **Owned by lane L2.**
//!
//! `.codegraph/planes.yaml` is the truth and these tables are an index over
//! it, the same relationship codegraph already has with source code. So a
//! sync **replaces** a project's rows rather than merging into them: there
//! is no state here worth preserving that the file does not already carry,
//! and a merge would let a row outlive the line that created it.
//!
//! ## Where the DDL runs
//!
//! `db::init_schema` runs only `src/schema.surql`. The plan tables live in
//! `src/schema_plan.surql` and are applied by [`ensure_schema`], which
//! [`replace_project`] calls before its first write. Every definition in
//! that file is `DEFINE ... OVERWRITE`, so applying it on every sync is
//! idempotent and also self healing: a store created by an older build
//! picks up a field added later instead of rejecting rows that carry it.
//! `OVERWRITE` redefines a table, it does not delete its records, which is
//! the property `code_node` already depends on across incremental
//! re-indexes.
//!
//! ## Atomicity
//!
//! The replace is one `BEGIN TRANSACTION ... COMMIT TRANSACTION` containing
//! the three deletes and the three inserts, submitted as a single
//! multi-statement query. A crash part way through therefore leaves the
//! previous roadmap intact rather than half of the new one: there is no
//! visible state between the delete and the insert. This is why there is no
//! generation column and no flip: the driver gives real transactions on
//! both the embedded and the remote engines, so the weaker scheme would buy
//! nothing and cost a column that every reader would have to filter on.

use anyhow::{Context, Result};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;
use surrealdb_types::SurrealValue;

use crate::plan::model::{
    Horizon, Kind, ResolvedTouch, Selector, Status, TouchConfidence,
};
use crate::plan::resolve::UnresolvedReason;
use crate::plan::{ITEM_TABLE, PLANE_TABLE, TOUCH_TABLE};

/// Rows per `INSERT` statement. A roadmap is small enough that this never
/// splits in practice; it exists so a pathological one cannot build a
/// single bind large enough to be refused.
const WRITE_CHUNK_SIZE: usize = 1_000;

// ============================================================================
// Row shapes
// ============================================================================

/// One `work_plane` row. Field for field what `src/schema_plan.surql`
/// declares, minus `synced_at`, whose schema DEFAULT fills it.
///
/// Serialized through SurrealDB's own serializer (`None` becomes `NONE`,
/// which is what the SCHEMAFULL `option<string>` fields accept) rather than
/// serde_json (`None` becomes `NULL`, which they reject). The same derive
/// reads the rows back, so the write shape and the read shape cannot drift.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PlaneRow {
    pub project_id: String,
    pub plane_id: String,
    pub node_id: String,
    pub title: String,
    pub status: String,
    pub horizon: String,
    pub summary: Option<String>,
    pub ordinal: i64,
}

/// One `work_item` row: the hyperedge itself.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ItemRow {
    pub project_id: String,
    pub item_id: String,
    pub node_id: String,
    pub plane: String,
    pub plane_node_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub depends_on: Vec<String>,
    pub spec: Option<String>,
    pub notes: Option<String>,
    pub ordinal: i64,
}

/// One `work_touch` row: one (item, code entity) incidence.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct TouchRow {
    pub project_id: String,
    pub item: String,
    pub item_id: String,
    pub selector: String,
    pub raw: String,
    pub to_id: Option<String>,
    pub confidence: String,
    pub candidates: Option<Vec<String>>,
    pub reason: Option<String>,
    pub detail: Option<String>,
    /// Whether the code graph holds a row for `to_id`. `None` on a
    /// `symbol:` row. See `src/schema_plan.surql` for why this is a column
    /// and not a confidence level.
    pub indexed: Option<bool>,
    pub ordinal: i64,
    pub selector_ordinal: i64,
}

/// The column list every read of each table selects, so a read and a write
/// can never disagree about which fields exist.
const PLANE_FIELDS: &str = "project_id, plane_id, node_id, title, status, horizon, summary, ordinal";
const ITEM_FIELDS: &str = "project_id, item_id, node_id, plane, plane_node_id, title, kind, status, depends_on, spec, notes, ordinal";
const TOUCH_FIELDS: &str = "project_id, item, item_id, selector, raw, to_id, confidence, candidates, reason, detail, indexed, ordinal, selector_ordinal";

impl TouchRow {
    /// The model's view of this row.
    ///
    /// An unparseable `selector` or `confidence` would mean a row written
    /// by a build that knows a value this one does not. Rather than
    /// guessing, both fall back to the most conservative reading: an
    /// unknown selector reads as `file` (the only selector whose target is
    /// a plain path, so it can never be mistaken for a bound node id) and
    /// an unknown confidence reads as UNRESOLVED, which understates
    /// certainty instead of overstating it.
    pub fn to_resolved(&self) -> ResolvedTouch {
        let selector = self.selector.parse::<Selector>().unwrap_or(Selector::File);
        let confidence = self
            .confidence
            .parse::<TouchConfidence>()
            .unwrap_or(TouchConfidence::Unresolved);
        let mut candidates = self.candidates.clone().unwrap_or_default();
        candidates.sort();
        ResolvedTouch {
            selector,
            raw: self.raw.clone(),
            to_id: match confidence {
                TouchConfidence::Resolved => self.to_id.clone(),
                _ => None,
            },
            confidence,
            candidates: match confidence {
                TouchConfidence::Ambiguous => candidates,
                _ => Vec::new(),
            },
        }
    }

    /// The stored reason, when it is one this build knows.
    pub fn reason(&self) -> Option<UnresolvedReason> {
        self.reason
            .as_deref()
            .and_then(UnresolvedReason::from_str_opt)
    }

    /// True when this row points at code that is not in the graph.
    pub fn is_unresolved(&self) -> bool {
        self.confidence == TouchConfidence::Unresolved.as_str()
    }

    /// True when this row bound to exactly one target.
    pub fn is_resolved(&self) -> bool {
        self.confidence == TouchConfidence::Resolved.as_str()
    }

    /// True when this row bound to a path the code graph holds nothing for.
    /// Such a row is a real target for `touching` and `collisions` and a
    /// dead end for `blast`, which has no node to walk from.
    pub fn is_unindexed(&self) -> bool {
        self.indexed == Some(false)
    }
}

impl PlaneRow {
    /// Parsed lifecycle state. An unreadable value reads as `planned`, the
    /// state that claims the least.
    pub fn status(&self) -> Status {
        self.status.parse().unwrap_or(Status::Planned)
    }

    /// Parsed horizon. An unreadable value reads as `later`.
    pub fn horizon(&self) -> Horizon {
        self.horizon.parse().unwrap_or(Horizon::Later)
    }
}

impl ItemRow {
    /// Parsed change kind. An unreadable value reads as `feature`.
    pub fn kind(&self) -> Kind {
        self.kind.parse().unwrap_or(Kind::Feature)
    }

    /// Parsed lifecycle state. An unreadable value reads as `planned`,
    /// which matters: `collisions` only considers `active` items, so an
    /// unreadable status can never manufacture a collision.
    pub fn status(&self) -> Status {
        self.status.parse().unwrap_or(Status::Planned)
    }
}

/// One project's whole roadmap as stored, in file order.
///
/// Everything after `sync` reads this once and works in memory. A roadmap
/// is the size of a `TODO.md`, so the alternative (a query per view) would
/// buy nothing and would make file order, which is the author's order,
/// depend on the store's row order.
///
/// Deliberately not `Serialize`: nothing outside this crate should be
/// handed raw rows. Every public result type is assembled in
/// [`crate::plan::ops`] out of the model types, so the `--json` shape is
/// the model's, not the store's.
#[derive(Debug, Clone, Default)]
pub struct PlanSnapshot {
    /// Planes in file order.
    pub planes: Vec<PlaneRow>,
    /// Items in file order.
    pub items: Vec<ItemRow>,
    /// Incidences, grouped by item in file order and ordered within an item.
    pub touches: Vec<TouchRow>,
}

impl PlanSnapshot {
    /// True when this project has never been synced.
    pub fn is_empty(&self) -> bool {
        self.planes.is_empty() && self.items.is_empty()
    }

    /// One item by its bare id.
    pub fn item(&self, item_id: &str) -> Option<&ItemRow> {
        self.items.iter().find(|i| i.item_id == item_id)
    }

    /// One plane by its bare id.
    pub fn plane(&self, plane_id: &str) -> Option<&PlaneRow> {
        self.planes.iter().find(|p| p.plane_id == plane_id)
    }

    /// Every incidence of one item, in order.
    pub fn touches_of(&self, item_node_id: &str) -> Vec<&TouchRow> {
        self.touches
            .iter()
            .filter(|t| t.item == item_node_id)
            .collect()
    }
}

// ============================================================================
// Schema
// ============================================================================

/// Apply the plan tables' DDL. Idempotent, and cheap enough to run before
/// every write. See the module docs for why it lives here and not in
/// `db::init_schema`.
pub async fn ensure_schema(db: &Surreal<Any>) -> Result<()> {
    db.query(include_str!("../schema_plan.surql"))
        .await
        .context("plan schema DDL execution failed")?
        .check()
        .context("plan schema DDL validation failed")?;
    Ok(())
}

// ============================================================================
// Reads
// ============================================================================

/// Load one project's whole roadmap, in file order.
pub async fn load_snapshot(db: &Surreal<Any>, project_id: &str) -> Result<PlanSnapshot> {
    ensure_schema(db).await?;

    let mut resp = db
        .query(format!(
            "SELECT {PLANE_FIELDS} FROM {PLANE_TABLE} WHERE project_id = $pid"
        ))
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading planes failed")?;
    let mut planes: Vec<PlaneRow> = resp.take(0).context("reading plane rows failed")?;

    let mut resp = db
        .query(format!(
            "SELECT {ITEM_FIELDS} FROM {ITEM_TABLE} WHERE project_id = $pid"
        ))
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading work items failed")?;
    let mut items: Vec<ItemRow> = resp.take(0).context("reading work item rows failed")?;

    let mut resp = db
        .query(format!(
            "SELECT {TOUCH_FIELDS} FROM {TOUCH_TABLE} WHERE project_id = $pid"
        ))
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading touches failed")?;
    let mut touches: Vec<TouchRow> = resp.take(0).context("reading touch rows failed")?;

    // Row order out of the store is undefined. File order is the author's
    // order and every view preserves it, so it is restored here once
    // rather than in each of the seven readers.
    planes.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then(a.plane_id.cmp(&b.plane_id)));
    items.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then(a.item_id.cmp(&b.item_id)));
    touches.sort_by(|a, b| {
        a.item_id
            .cmp(&b.item_id)
            .then(a.ordinal.cmp(&b.ordinal))
            .then(a.raw.cmp(&b.raw))
    });

    Ok(PlanSnapshot {
        planes,
        items,
        touches,
    })
}

/// The plane ids and item ids this project currently has stored.
///
/// Read before a replace so the sync report can say how much of the old
/// roadmap the new file dropped, which is the difference between "the
/// roadmap changed" and "the roadmap shrank".
pub async fn load_ids(db: &Surreal<Any>, project_id: &str) -> Result<(Vec<String>, Vec<String>)> {
    ensure_schema(db).await?;

    let mut resp = db
        .query(format!(
            "SELECT VALUE plane_id FROM {PLANE_TABLE} WHERE project_id = $pid; \
             SELECT VALUE item_id FROM {ITEM_TABLE} WHERE project_id = $pid"
        ))
        .bind(("pid", project_id.to_string()))
        .await
        .context("loading stored plan ids failed")?;
    let planes: Vec<String> = resp.take(0).context("reading stored plane ids failed")?;
    let items: Vec<String> = resp.take(1).context("reading stored item ids failed")?;
    Ok((planes, items))
}

// ============================================================================
// Writes
// ============================================================================

/// Replace every stored row for one project with the given ones, in one
/// transaction. See the module docs on atomicity.
pub async fn replace_project(
    db: &Surreal<Any>,
    project_id: &str,
    planes: &[PlaneRow],
    items: &[ItemRow],
    touches: &[TouchRow],
) -> Result<()> {
    ensure_schema(db).await?;

    let mut statements = vec![
        "BEGIN TRANSACTION".to_string(),
        format!("DELETE {TOUCH_TABLE} WHERE project_id = $pid"),
        format!("DELETE {ITEM_TABLE} WHERE project_id = $pid"),
        format!("DELETE {PLANE_TABLE} WHERE project_id = $pid"),
    ];

    // An `INSERT INTO t $rows` with an empty array is not worth relying on,
    // and an empty roadmap is a legitimate thing to sync (a file whose last
    // plane was just deleted), so the statement is omitted rather than
    // guarded at the server. Chunk variables are named rather than
    // positional so a reader of the assembled query can see which table
    // each bind feeds.
    let mut plane_binds: Vec<(String, Vec<PlaneRow>)> = Vec::new();
    let mut item_binds: Vec<(String, Vec<ItemRow>)> = Vec::new();
    let mut touch_binds: Vec<(String, Vec<TouchRow>)> = Vec::new();

    for (n, chunk) in planes.chunks(WRITE_CHUNK_SIZE).enumerate() {
        let var = format!("plane_rows_{n}");
        statements.push(format!("INSERT INTO {PLANE_TABLE} ${var}"));
        plane_binds.push((var, chunk.to_vec()));
    }
    for (n, chunk) in items.chunks(WRITE_CHUNK_SIZE).enumerate() {
        let var = format!("item_rows_{n}");
        statements.push(format!("INSERT INTO {ITEM_TABLE} ${var}"));
        item_binds.push((var, chunk.to_vec()));
    }
    for (n, chunk) in touches.chunks(WRITE_CHUNK_SIZE).enumerate() {
        let var = format!("touch_rows_{n}");
        statements.push(format!("INSERT INTO {TOUCH_TABLE} ${var}"));
        touch_binds.push((var, chunk.to_vec()));
    }

    statements.push("COMMIT TRANSACTION".to_string());

    let mut query = db
        .query(statements.join(";\n"))
        .bind(("pid", project_id.to_string()));
    for bind in plane_binds {
        query = query.bind(bind);
    }
    for bind in item_binds {
        query = query.bind(bind);
    }
    for bind in touch_binds {
        query = query.bind(bind);
    }

    query
        .await
        .context("plan sync transaction failed")?
        .check()
        .context("plan sync transaction returned an error")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model::Touch;

    fn touch_row(confidence: &str, to_id: Option<&str>, candidates: Option<Vec<&str>>) -> TouchRow {
        TouchRow {
            project_id: "p".into(),
            item: "work_item:p__A-1".into(),
            item_id: "A-1".into(),
            selector: "symbol".into(),
            raw: "connect".into(),
            to_id: to_id.map(str::to_string),
            confidence: confidence.into(),
            candidates: candidates.map(|c| c.into_iter().map(str::to_string).collect()),
            reason: None,
            detail: None,
            indexed: None,
            ordinal: 0,
            selector_ordinal: 0,
        }
    }

    #[test]
    fn a_resolved_row_keeps_its_target_and_drops_candidates() {
        let rt = touch_row("RESOLVED", Some("abc"), Some(vec!["abc", "def"])).to_resolved();
        assert_eq!(rt.confidence, TouchConfidence::Resolved);
        assert_eq!(rt.to_id.as_deref(), Some("abc"));
        assert!(rt.candidates.is_empty(), "a bound row carries no candidates");
    }

    #[test]
    fn an_ambiguous_row_keeps_candidates_and_never_a_target() {
        let rt = touch_row("AMBIGUOUS", Some("abc"), Some(vec!["def", "abc"])).to_resolved();
        assert_eq!(rt.confidence, TouchConfidence::Ambiguous);
        assert_eq!(rt.to_id, None, "an ambiguous row must never carry a pick");
        assert_eq!(rt.candidates, vec!["abc".to_string(), "def".to_string()]);
    }

    #[test]
    fn an_unreadable_confidence_understates_rather_than_overstates() {
        let rt = touch_row("SOMETHING_NEWER", Some("abc"), None).to_resolved();
        assert_eq!(rt.confidence, TouchConfidence::Unresolved);
        assert_eq!(rt.to_id, None);
    }

    #[test]
    fn the_model_round_trips_through_a_row() {
        let touch = Touch::new(Selector::Glob, "src/**");
        let original = ResolvedTouch::resolved(&touch, "src/cli.rs");
        let row = TouchRow {
            project_id: "p".into(),
            item: "work_item:p__A-1".into(),
            item_id: "A-1".into(),
            selector: original.selector.as_str().into(),
            raw: original.raw.clone(),
            to_id: original.to_id.clone(),
            confidence: original.confidence.as_str().into(),
            candidates: None,
            reason: None,
            detail: None,
            indexed: Some(true),
            ordinal: 0,
            selector_ordinal: 0,
        };
        assert_eq!(row.to_resolved(), original);
    }
}
