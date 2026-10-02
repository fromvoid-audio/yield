//! Actual source-owner commit/readback fixture; no fabricated SourceWriterReceipt.
use super::*;
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{OperationId, ParticipantId};
use crate::sample_sources::db::SourceWriterReceipt;
use crate::sample_sources::reconciliation::RootRelativePath;
use crate::sample_sources::{
    Rating, SourceCommandId, SourceCurationUpdate, SourceDatabase, SourceId, SourceWriter,
    SourceWriterRequest,
};
use std::path::Path;

pub(super) fn actual_source_receipt(
    guard: &WritableProfileGuard,
    root: &Path,
) -> SourceWriterReceipt {
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        root,
        guard,
        Path::new("operation-link-source-db"),
    )
    .unwrap();
    database.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
    database
        .begin_source_directory_truth_generation(1, 0)
        .unwrap();
    let mut source =
        SourceWriter::new_for_scan(database, SourceId::from_string("operation-link-source"), 1)
            .unwrap();
    let root_identity = source.current_scan_fence().unwrap().root_identity().clone();
    source
        .catalog_database()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(1, 1, 10, root_identity, None)
        .unwrap();
    let request = SourceWriterRequest::new(
        OperationId::from_uuid(uuid::Uuid::from_u128(2203)),
        ParticipantId::from_uuid(uuid::Uuid::from_u128(3)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(1)),
        source.current_revision().unwrap(),
        vec![SourceCurationUpdate::SetRating {
            path: RootRelativePath::try_from_path("one.wav".into()).unwrap(),
            rating: Rating::KEEP_1,
        }],
    )
    .unwrap();
    let receipt = source.commit_curation(request.clone()).unwrap();
    assert_eq!(
        source.read_receipt_observation(request.identity()).unwrap(),
        Some(receipt.clone())
    );
    receipt
}

pub(super) fn actual_request(owner: &str, receipt: &SourceWriterReceipt) -> OperationLinkRequest {
    OperationLinkRequest::new(
        owner,
        WorkUnitKey::new(
            0,
            60,
            ParticipantId::from_uuid(uuid::Uuid::from_u128(3)),
            WorkUnitRole::SourceAffect,
        ),
        WorkUnitKey::new(
            255,
            10,
            ParticipantId::from_uuid(uuid::Uuid::from_u128(2)),
            WorkUnitRole::GlobalLibrary,
        ),
        receipt,
    )
    .unwrap()
}
