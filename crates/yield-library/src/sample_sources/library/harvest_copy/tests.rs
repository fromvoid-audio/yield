//! Actual owner/SQLite metadata fixtures; stored hashes/capacity inputs are not qualification.
use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
use crate::operation_journal::{
    OperationJournalOwner, OperationJournalOwnerStatus, OperationJournalStore, ParticipantId,
};
use crate::operation_journal::{
    codec::StoredOperationRecord,
    coordinator::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        UniversalCommandArgs,
    },
    model::{
        OperationActor, OperationIntent, OperationKind, UniversalCommandKind, UniversalIntent,
    },
    participants::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantKind,
        ParticipantRequirement, ParticipantSet, StoredSourceFence,
    },
    work_plan::{WorkUnitRole, derive_work_plan},
};
use crate::sample_sources::db::{
    ManifestEnrollmentEntry, ManifestEnrollmentKind, ManifestEnrollmentRequest,
};
use crate::sample_sources::library::{
    GlobalLibraryWriter, GlobalLibraryWriterStatus, ReferenceSnapshotRequest,
    SourceRegistryParticipantRequest, SourceRegistrySnapshot,
};
use crate::sample_sources::{
    SampleSource, SourceDatabase, SourceId, SourceMetadataStorage, SourceRole, SourceWriter,
};
use crate::test_runtime::TestRuntimeGuard;
use rusqlite::{Connection, params};
use std::{
    path::Path,
    time::{Duration, Instant},
};
mod capacity;
mod ownership;
mod rejection;
mod replay;
mod setup;
mod wal_maintenance;
struct Fixture {
    global: Option<GlobalLibraryWriter>,
    journal: Option<OperationJournalOwner>,
    sources: Option<[SourceWriter; 2]>,
    family: FrozenCopyObservation,
    enrollment: ManifestEnrollmentReceipt,
    registry: SourceRegistryParticipantReceipt,
    guard: Option<WritableProfileGuard>,
    roots: [tempfile::TempDir; 2],
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: tempfile::TempDir,
    _runtime: TestRuntimeGuard,
}
fn wait_global(w: &GlobalLibraryWriter) {
    let end = Instant::now() + Duration::from_secs(3);
    while w.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < end, "{:?}", w.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn source(
    root: &tempfile::TempDir,
    guard: &WritableProfileGuard,
    id: &str,
    path: &str,
) -> SourceWriter {
    let db = SourceDatabase::open_for_catalog_with_profile_guard(root.path(), guard, Path::new(id))
        .unwrap();
    db.upsert_file(Path::new(path), 10, 1).unwrap();
    // Seed exact source metadata, then perform the actual owner's audited manifest transaction.
    let mut batch = db.write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new(path), 10, 1, &"a".repeat(64))
        .unwrap();
    batch
        .set_file_identity(Path::new(path), Some(&format!("object-{id}")))
        .unwrap();
    batch.commit().unwrap();
    db.begin_source_directory_truth_generation(1, 0).unwrap();
    let writer = SourceWriter::new_for_scan(db, SourceId::from_string(id), 1).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let revision = fence.revision();
    let root = fence.root_identity().clone();
    writer
        .catalog_database()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(1, revision, 10, root, None)
        .unwrap();
    writer
}
fn content(source: &SourceWriter, path: &str) -> SourceContentIdentityObservation {
    source
        .observe_file_identity(StoredRelativePath::new(path).unwrap())
        .unwrap()
        .into_content_identity()
        .unwrap()
}
fn stored(f: &SourceContentIdentityObservation) -> StoredSourceFence {
    let r = f.file().source_revision();
    StoredSourceFence::new(
        r.source_id().clone(),
        std::str::from_utf8(r.root_identity().as_bytes()).unwrap(),
        r.database_identity(),
        r.lifecycle_generation(),
        r.revision(),
        r.lease_epoch(),
    )
    .unwrap()
}
impl Fixture {
    fn global(&self) -> &GlobalLibraryWriter {
        self.global.as_ref().unwrap()
    }
    fn sources(&self) -> &[SourceWriter; 2] {
        self.sources.as_ref().unwrap()
    }
    fn guard(&self) -> &WritableProfileGuard {
        self.guard.as_ref().unwrap()
    }
    fn db(&self) -> Connection {
        Connection::open(
            self.guard()
                .profile_root()
                .join(super::super::LIBRARY_DB_FILE_NAME),
        )
        .unwrap()
    }
    fn request(&self) -> HarvestCopyRequest {
        let parent = content(&self.sources()[0], "one.wav");
        let child = content(&self.sources()[1], "new.wav");
        let snapshot = self
            .global()
            .reference_snapshot(
                ReferenceSnapshotRequest::new(
                    self.guard().owner_identity(),
                    payload::File::from_observation(&parent)
                        .unwrap()
                        .key()
                        .unwrap(),
                    payload::File::from_observation(&child)
                        .unwrap()
                        .key()
                        .unwrap(),
                    self.sources()[0].reference_read_fence().unwrap(),
                    self.sources()[1].reference_read_fence().unwrap(),
                )
                .unwrap(),
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        HarvestCopyRequest::new(
            self.guard(),
            self.family.clone(),
            parent,
            child,
            self.enrollment.clone(),
            self.registry.clone(),
            snapshot,
            super::super::HarvestMetadataSnapshot {
                rating: Some(2),
                tags: vec!["drum".into()],
                playback_type: Some("one-shot".into()),
            },
            "yield-test".into(),
        )
        .unwrap()
    }
    fn apply(&self, r: HarvestCopyRequest) -> Result<HarvestCopyResult, LibraryError> {
        self.global()
            .apply_harvest_copy(r)
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }
    fn restore(&self, bytes: &[u8]) -> Result<HarvestCopyRequest, LibraryError> {
        HarvestCopyRequest::restore(
            self.guard(),
            bytes,
            self.family.clone(),
            content(&self.sources()[0], "one.wav"),
            content(&self.sources()[1], "new.wav"),
            self.enrollment.clone(),
            self.registry.clone(),
        )
    }
    fn source_connection(&self, index: usize) -> Connection {
        Connection::open(
            self.guard()
                .profile_root()
                .join(["origin", "destination"][index])
                .join(crate::sample_sources::db::DB_FILE_NAME),
        )
        .unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut g) = self.global.take() {
            g.shutdown();
        }
        if let Some(mut j) = self.journal.take() {
            assert!(j.shutdown());
        }
        if let Some(s) = self.sources.take() {
            for writer in s {
                writer.into_catalog_database().close().unwrap();
            }
        }
        self.guard.take();
    }
}
