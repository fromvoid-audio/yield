//! Bounded whole-source observations for the assignment participant; no mutation authority.

use rusqlite::{Connection, OptionalExtension, params};

use super::SourceRevision;
use super::source_writer::SourceWriterError;
use crate::sample_sources::db::tags::{
    PLAYBACK_TYPE_CLASSIFICATION, normalize_tag_identity, validate_curation_tag_label,
};
use crate::sample_sources::db::util::{map_sql_error, normalize_relative_path};
use crate::sample_sources::reconciliation::RootRelativePath;

/// Requested assignment delta. Capturing its scope does not admit its execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceTagAssignmentMutation {
    /// Move every old-tag assignment to a distinct, absent normal identity.
    Rename(String),
    /// Remove every old-tag assignment without deleting the profile definition.
    Delete,
}

/// Exact bounded observation issued by the retained source owner.
///
/// Private fields prevent construction from serialized rows. This is neither a write grant
/// nor a reservation: a participant must revalidate its full scope before editing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceTagAssignmentSelection {
    revision: SourceRevision,
    old_tag_id: i64,
    old_identity: String,
    mutation: SourceTagAssignmentMutation,
    paths: Vec<RootRelativePath>,
}

impl SourceTagAssignmentSelection {
    pub(crate) fn retained_heap_bytes(&self) -> Option<usize> {
        let mutation = match &self.mutation {
            SourceTagAssignmentMutation::Rename(s) => s.capacity(),
            SourceTagAssignmentMutation::Delete => 0,
        };
        let base = self
            .revision
            .retained_heap_bytes()?
            .checked_add(self.old_identity.capacity())?
            .checked_add(mutation)?
            .checked_add(
                self.paths
                    .capacity()
                    .checked_mul(std::mem::size_of::<RootRelativePath>())?,
            )?;
        self.paths
            .iter()
            .try_fold(base, |n, p| n.checked_add(p.retained_heap_bytes()))
    }

    /// Source, physical binding and lifecycle represented by this observation.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Source-local identity, never a profile-global tag ID.
    pub fn old_tag_id(&self) -> i64 {
        self.old_tag_id
    }
    /// Canonical old tag identity.
    pub fn old_identity(&self) -> &str {
        &self.old_identity
    }
    /// Canonical requested delta.
    pub fn mutation(&self) -> &SourceTagAssignmentMutation {
        &self.mutation
    }
    /// Entire sorted assignment set; an empty set still names an existing normal tag.
    pub fn paths(&self) -> &[RootRelativePath] {
        &self.paths
    }
}

fn normal_identity(connection: &Connection, input: &str) -> Result<String, SourceWriterError> {
    validate_curation_tag_label(input)?;
    let identity = normalize_tag_identity(input)?.normalized_text;
    let sql = format!(
        "SELECT ({PLAYBACK_TYPE_CLASSIFICATION}) IS NOT NULL FROM (SELECT ?1 AS normalized_text) AS st"
    );
    let classification: bool = connection
        .query_row(&sql, [&identity], |r| r.get(0))
        .map_err(map_sql_error)?;
    if classification {
        return Err(SourceWriterError::InvalidRequest(
            "classification tag is not a normal assignment identity",
        ));
    }
    Ok(identity)
}

pub(super) fn capture(
    connection: &Connection,
    revision: SourceRevision,
    old_label: &str,
    mutation: SourceTagAssignmentMutation,
) -> Result<SourceTagAssignmentSelection, SourceWriterError> {
    let old_identity = normal_identity(connection, old_label)?;
    let old_tag_id = connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text=?1",
            [&old_identity],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .filter(|id| *id > 0)
        .ok_or(SourceWriterError::InvalidRequest(
            "old normal tag does not exist",
        ))?;
    let mutation = match mutation {
        SourceTagAssignmentMutation::Rename(label) => {
            let target = normal_identity(connection, &label)?;
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM source_tags WHERE normalized_text=?1)",
                    [&target],
                    |r| r.get(0),
                )
                .map_err(map_sql_error)?;
            if target == old_identity || exists {
                return Err(SourceWriterError::InvalidRequest(
                    "assignment rename target collides",
                ));
            }
            SourceTagAssignmentMutation::Rename(target)
        }
        SourceTagAssignmentMutation::Delete => SourceTagAssignmentMutation::Delete,
    };
    let limit = match mutation {
        SourceTagAssignmentMutation::Rename(_) => 64,
        SourceTagAssignmentMutation::Delete => 128,
    };
    // SQL guards the payload before Rust materializes it. LEFT JOIN retains invalid/orphan
    // assignments so they reject the whole scope instead of silently disappearing.
    let sql = assignment_query();
    let mut statement = connection.prepare(&sql).map_err(map_sql_error)?;
    let mut rows = statement
        .query(params![old_tag_id, limit + 1])
        .map_err(map_sql_error)?;
    let mut paths = Vec::with_capacity(limit);
    while let Some(row) = rows.next().map_err(map_sql_error)? {
        if paths.len() == limit {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let path: Option<String> = row.get(0).map_err(map_sql_error)?;
        let supported: bool = row.get(1).map_err(map_sql_error)?;
        let path = path.ok_or(SourceWriterError::SnapshotBudgetExceeded)?;
        if !supported || !crate::sample_sources::is_supported_audio(std::path::Path::new(&path)) {
            return Err(SourceWriterError::InvalidRequest(
                "assignment has no live supported manifest member",
            ));
        }
        if normalize_relative_path(std::path::Path::new(&path))? != path {
            return Err(SourceWriterError::InvalidRequest(
                "assignment path is not canonical",
            ));
        }
        paths.push(
            RootRelativePath::try_from_path(path.into())
                .map_err(|_| SourceWriterError::InvalidRequest("assignment path is malformed"))?,
        );
    }
    // All accepted assignments fit the finite set. Sort owned locators without asking SQLite
    // to spill a temporary sorter before any capacity claim/entered MEMORY policy exists.
    paths.sort_by(|a, b| {
        a.as_path()
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_path().as_os_str().as_encoded_bytes())
    });
    Ok(SourceTagAssignmentSelection {
        revision,
        old_tag_id,
        old_identity,
        mutation,
        paths,
    })
}

// Trusted fixed supported-extension inventory; this construction has no caller payload.
fn assignment_query() -> String {
    format!(
        "SELECT CASE WHEN length(CAST(wft.path AS BLOB)) <= 1024 THEN wft.path ELSE NULL END,
        wf.path IS NOT NULL AND wf.missing=0 AND ({supported})
        FROM wav_file_tags AS wft LEFT JOIN wav_files AS wf ON wf.path=wft.path
        WHERE wft.tag_id=?1 LIMIT ?2",
        supported = crate::sample_sources::supported_audio_where_clause()
            .replace("extension", "wf.extension")
            .replace("path", "wf.path")
    )
}

/// Compare the complete original finite selection with current borrowed SQLite rows.
/// Private capture validated canonical paths; exact byte membership preserves that witness.
/// No second selection, revision/mutation clone, owned row path or SQLite sorter is built.
pub(super) fn validate_current(
    connection: &Connection,
    selected: &SourceTagAssignmentSelection,
) -> Result<(), SourceWriterError> {
    let limit = match selected.mutation() {
        SourceTagAssignmentMutation::Rename(_) => 64,
        SourceTagAssignmentMutation::Delete => 128,
    };
    if selected.old_tag_id() <= 0 || selected.paths().len() > limit {
        return Err(SourceWriterError::SnapshotBudgetExceeded);
    }
    let current_id = connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text=?1",
            [selected.old_identity()],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(map_sql_error)?;
    if current_id != Some(selected.old_tag_id()) {
        return Err(SourceWriterError::StaleFence);
    }
    if let SourceTagAssignmentMutation::Rename(target) = selected.mutation() {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source_tags WHERE normalized_text=?1)",
                [target],
                |r| r.get(0),
            )
            .map_err(map_sql_error)?;
        if target == selected.old_identity() || exists {
            return Err(SourceWriterError::InvalidRequest(
                "assignment rename target collides",
            ));
        }
    }
    let sql = assignment_query();
    let mut statement = connection.prepare(&sql).map_err(map_sql_error)?;
    let mut rows = statement
        .query(params![selected.old_tag_id(), limit + 1])
        .map_err(map_sql_error)?;
    let mut seen = [false; 128];
    let mut count = 0;
    while let Some(row) = rows.next().map_err(map_sql_error)? {
        if count == limit {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let path = match row.get_ref(0).map_err(map_sql_error)? {
            rusqlite::types::ValueRef::Text(bytes) => std::str::from_utf8(bytes)
                .map_err(|_| SourceWriterError::InvalidRequest("assignment path is malformed"))?,
            rusqlite::types::ValueRef::Null => {
                return Err(SourceWriterError::SnapshotBudgetExceeded);
            }
            _ => {
                return Err(SourceWriterError::InvalidRequest(
                    "assignment path is malformed",
                ));
            }
        };
        let supported: bool = row.get(1).map_err(map_sql_error)?;
        if !supported || !crate::sample_sources::is_supported_audio(std::path::Path::new(path)) {
            return Err(SourceWriterError::InvalidRequest(
                "assignment has no live supported manifest member",
            ));
        }
        let index = selected
            .paths()
            .binary_search_by(|original| {
                original
                    .as_path()
                    .as_os_str()
                    .as_encoded_bytes()
                    .cmp(path.as_bytes())
            })
            .map_err(|_| SourceWriterError::StaleFence)?;
        if seen[index] {
            return Err(SourceWriterError::StaleFence);
        }
        seen[index] = true;
        count += 1;
    }
    if count != selected.paths().len() || seen[..count].iter().any(|present| !present) {
        return Err(SourceWriterError::StaleFence);
    }
    Ok(())
}
