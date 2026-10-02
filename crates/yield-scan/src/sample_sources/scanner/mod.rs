mod manifest;
mod prepared_exact;
mod scan;
mod scan_capability;
mod scan_db_sync;
mod scan_diff;
mod scan_diff_phase;
mod scan_fs;
mod scan_hash;
mod scan_index;
mod scan_paths;
mod scan_walk;
mod scan_writer;
mod wav_metadata;

pub use prepared_exact::{
    EXACT_SCAN_MAX_ENTRIES, EXACT_SCAN_MAX_HASH_BYTES, EXACT_SCAN_MAX_PATH_BYTES,
    PreparedExactScan, prepare_exact_scan,
};
pub use scan::{
    CatalogManifestAudit, ChangedSample, CommittedScanBatch, CommittedSourceDelta,
    CommittedSourceIndexDelta, ContentAuditActivity, ContentAuditBudget, ContentAuditStorage,
    DirectoryRepeatKind, ManifestAuditOutcome, ManifestIdentityDelta, MovedManifestIdentity,
    RenamedSample, ScanError, ScanMode, ScanStats, SourceTreeDiagnostic, SourceTreeFile,
    SourceTreeSnapshot, UpdatedSample,
    audit_manifest_for_catalog_with_budget_and_progress_and_writer, audit_source,
    audit_source_and_record, audit_source_and_record_with_budget_and_progress,
    audit_source_and_record_with_budget_and_progress_and_writer,
    audit_source_and_record_with_budget_and_progress_and_writer_outcome,
    audit_source_and_record_with_budget_and_progress_and_writer_with_request,
    audit_source_and_record_with_progress, audit_source_with_budget, complete_deferred_hashes,
    complete_deferred_hashes_with_cancel, complete_deferred_rename_candidates,
    complete_deferred_rename_candidates_with_cancel,
    complete_deferred_rename_candidates_with_cancel_and_writer,
    complete_pending_deep_hash_for_path, complete_pending_deep_hashes, hard_rescan,
    scan_in_background, scan_once, scan_with_progress, scan_with_progress_and_writer,
    scan_with_progress_and_writer_and_committed_batch,
};
pub use scan_paths::{
    sync_exact_paths_with_progress_and_writer, sync_exact_paths_with_source_writer, sync_paths,
    sync_paths_with_progress, sync_paths_with_progress_and_writer, sync_paths_with_source_writer,
    validate_exact_regular_paths,
};
pub use scan_writer::{ScanWritePhase, ScanWriter, UncoordinatedScanWriter};
pub use wav_metadata::{WavDisplayMetadata, read_wav_display_metadata_for_manifest};

#[cfg(test)]
mod fixture_database {
    pub(crate) fn open_maintained_scan_database(
        root: impl AsRef<std::path::Path>,
    ) -> Result<crate::sample_sources::SourceDatabase, crate::sample_sources::SourceDbError> {
        let root = root.as_ref();
        drop(crate::sample_sources::SourceDatabase::open_for_maintenance(
            root,
        )?);
        crate::sample_sources::SourceDatabase::open_for_scan(root)
    }
}

#[cfg(test)]
pub(crate) use fixture_database::open_maintained_scan_database;
