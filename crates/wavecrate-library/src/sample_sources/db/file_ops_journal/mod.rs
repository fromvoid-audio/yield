//! Durable per-source file-operation journal and crash-recovery helpers.
//!
//! Why this exists:
//! - copy/move flows can crash after filesystem work but before both databases
//!   reflect the final state
//! - the journal records enough metadata to reconcile source and target DB rows
//!   on the next startup
//!
//! Stage contract:
//! - `Intent`: journal row exists, no filesystem mutation is assumed yet
//! - `Staged`: data exists at `staged_relative` and has not been finalized
//! - `TargetDb`: the target-side DB update has committed
//! - `SourceDb`: the source-side DB cleanup has committed for moves

use super::{SourceDatabase, SourceDbError};

mod decode;
mod entry;
mod reader;
mod reconcile;
mod recovery_io;
mod store;
#[cfg(test)]
mod tests;

pub(crate) use recovery_io::BoundDatabaseRoot;

pub use entry::{
    CopyJournalEntryInit, FileOpJournalEntry, FileOpStage, MoveJournalEntryInit, new_op_id,
    staged_relative_for_target,
};
pub use reader::{
    LegacyJournalCursor, LegacyJournalDedupIdentity, LegacyJournalDedupIdentityResult,
    LegacyJournalDedupIdentityUnavailable, LegacyJournalEntry, LegacyJournalField,
    LegacyJournalKey, LegacyJournalPage, LegacyJournalRetentionReason, LegacyJournalRowOutcome,
    LegacyJournalScanError, MAX_LEGACY_JOURNAL_BYTES_PER_PAGE, MAX_LEGACY_JOURNAL_FIELD_BYTES,
    MAX_LEGACY_JOURNAL_PREVIEW_BYTES, MAX_LEGACY_JOURNAL_ROWS_PER_PAGE,
};
/// Summary of the work performed while reconciling pending journal entries.
pub type FileOpReconcileSummary = reconcile::FileOpReconcileSummary;
/// Result of loading journal rows during tests.
pub type ListedJournalEntries = decode::ListedJournalEntries;

/// Insert a new journal entry before mutating the filesystem.
pub fn insert_entry(db: &SourceDatabase, entry: &FileOpJournalEntry) -> Result<(), SourceDbError> {
    store::FileOpJournalStore::new(db).insert(entry)
}

/// Update a journal entry stage and optional metadata after filesystem work.
pub fn update_stage(
    db: &SourceDatabase,
    id: &str,
    stage: FileOpStage,
    file_size: Option<u64>,
    modified_ns: Option<i64>,
) -> Result<(), SourceDbError> {
    store::FileOpJournalStore::new(db).update_stage(id, stage, file_size, modified_ns)
}

/// Remove a resolved journal entry after reconciliation.
pub fn remove_entry(db: &SourceDatabase, id: &str) -> Result<(), SourceDbError> {
    store::FileOpJournalStore::new(db).remove(id)
}

/// Load all pending journal entries for reconciliation.
pub fn list_entries(db: &SourceDatabase) -> Result<ListedJournalEntries, SourceDbError> {
    store::FileOpJournalStore::new(db).list()
}

/// Read one bounded page of legacy source-local journal rows.
///
/// The page requires the retained source/profile ownership boundary and performs no filesystem
/// or journal mutation.  Decoded rows remain legacy observations; they do not become universal
/// operation records or execution authority.
pub fn scan_page(
    db: &SourceDatabase,
    cursor: Option<LegacyJournalCursor>,
) -> Result<LegacyJournalPage, LegacyJournalScanError> {
    reader::scan_page(db, cursor)
}

/// Reconcile all pending file ops against the filesystem and database.
///
/// Normal builds require a profile-owned source database before recovery can reach any
/// filesystem finalizer. Fixture builds retain their legacy test-only recovery route.
pub fn reconcile_pending_ops(db: &SourceDatabase) -> Result<FileOpReconcileSummary, String> {
    db.validate_profile_owned_mutation()
        .map_err(|error| error.to_string())?;
    reconcile::reconcile_pending_ops(db)
}
