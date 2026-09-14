//! `codegraph context`: write the session-bootstrap markdown file agents
//! read at the start of a session. **Owned by lane L3.**
//!
//! This file is the writer and nothing else. Every byte of the document is
//! built by [`crate::landscape::context_markdown`], which is where the
//! landscape itself is built. The two have to describe the same partition of
//! the same tree, and the cheapest way to guarantee that is to keep the
//! embedding next to the thing being embedded rather than to have a second
//! file reach across for it.
//!
//! The board's section 3.4 is the reason the roadmap lands here: every
//! agent already wired to read `.codegraph/context.md` inherits it with no
//! further integration work.

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use crate::landscape;
use crate::plan;

/// Generate the session bootstrap context file for `project_id`.
///
/// Reads the roadmap from `.codegraph/planes.yaml` under the current
/// directory, the same default `codegraph landscape` uses. A project with no
/// planes file still gets a context file: the roadmap section says there is
/// no roadmap and how to make one.
pub async fn generate_context(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    output: &Path,
) -> Result<()> {
    let planes_path = plan::default_planes_path(Path::new("."));
    generate_context_with_planes(db, project_id, output, &planes_path).await
}

/// [`generate_context`] with the roadmap file named explicitly.
pub async fn generate_context_with_planes(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    output: &Path,
    planes_path: &Path,
) -> Result<()> {
    let md = landscape::context_markdown(db, project_id, planes_path).await?;

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(output, &md)?;
    println!("Context written to {}", output.display());

    Ok(())
}
