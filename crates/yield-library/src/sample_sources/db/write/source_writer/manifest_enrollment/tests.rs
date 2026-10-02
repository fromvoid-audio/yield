//! Real retained source/SQLite command tests. Seeded hashes are metadata inputs, not qualification.
use super::*;
use crate::{
    app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard},
    operation_journal::ParticipantId,
    sample_sources::{SourceDatabase, SourceId},
    test_runtime::TestRuntimeGuard,
};
use std::path::Path;
use tempfile::TempDir;
struct Fixture {
    writer: Option<SourceWriter>,
    root: TempDir,
    guard: WritableProfileGuard,
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let config = ConfigBaseGuard::set(base.path().into());
        let profile = PersistenceProfileGuard::named("enrollment-owner");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            root.path(),
            &guard,
            Path::new("enroll-db"),
        )
        .unwrap();
        let writer =
            SourceWriter::new_for_scan(db, SourceId::from_string("actual-enrollment-source"), 7)
                .unwrap();
        Self {
            writer: Some(writer),
            root,
            guard,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
    fn owner(&self) -> &SourceWriter {
        self.writer.as_ref().unwrap()
    }
    fn request(&self) -> ManifestEnrollmentRequest {
        ManifestEnrollmentRequest::new(
            OperationId::new(),
            WorkUnitKey::new(0, 60, ParticipantId::new(), WorkUnitRole::SourceAffect),
            self.owner(),
            vec![ManifestEnrollmentEntry {
                path: StoredRelativePath::new("音.wav").unwrap(),
                kind: ManifestEnrollmentKind::Audio,
                size: 10,
                modified_ns: -1,
                file_identity: "actual-file-object".into(),
                content_hash: Some("a".repeat(64)),
            }],
        )
        .unwrap()
    }
    fn batch(&self, r: &ManifestEnrollmentRequest) -> SourceWriteBatch<'_> {
        let mut b = self.owner().catalog_database().write_batch().unwrap();
        for e in r.entries() {
            let p = Path::new(e.path.as_str());
            b.upsert_file_with_hash(p, e.size, e.modified_ns, e.content_hash.as_deref().unwrap())
                .unwrap();
            b.set_file_identity(p, Some(&e.file_identity)).unwrap();
        }
        b
    }
    fn connection(&self) -> &rusqlite::Connection {
        &self.owner().database.connection
    }
}
#[test]
fn manifest_enrollment_owner_atomic_replay_conflict_and_restart() {
    let mut f = Fixture::new();
    let r = f.request();
    let bytes = r.proposal_bytes().to_vec();
    let before = r.current.revision();
    let first = f
        .owner()
        .commit_manifest_enrollment(&r, f.batch(&r), None)
        .unwrap();
    assert!(!first.replayed);
    first.receipt.validate().unwrap();
    assert_eq!(first.receipt.committed_revision(), before + 1);
    assert_eq!(first.receipt.work_key(), r.payload.work);
    assert_eq!(first.receipt.entries(), r.entries());
    // Replay takes original custody and fresh current context, preserving later unrelated metadata.
    f.connection()
        .execute(
            "UPDATE wav_files SET file_size=999,content_hash=?1",
            ["b".repeat(64)],
        )
        .unwrap();
    let fresh = ManifestEnrollmentRequest::restore(&bytes, f.owner()).unwrap();
    let replay = f
        .owner()
        .replay_manifest_enrollment(&fresh)
        .unwrap()
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt.manifest(), first.receipt.manifest());
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        before + 1
    );
    assert_eq!(
        f.owner()
            .database
            .entry_for_path(Path::new("音.wav"))
            .unwrap()
            .unwrap()
            .file_size,
        999
    );
    let duplicate = f
        .owner()
        .commit_manifest_enrollment(&fresh, f.batch(&fresh), None)
        .unwrap();
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt.manifest(), first.receipt.manifest());
    assert_eq!(
        f.owner()
            .database
            .entry_for_path(Path::new("音.wav"))
            .unwrap()
            .unwrap()
            .file_size,
        999
    );
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        before + 1
    );
    let mut conflict = fresh.clone();
    conflict.payload.entries[0].size = 11;
    conflict.bytes = serde_json::to_vec(&conflict.payload).unwrap();
    assert!(matches!(
        f.owner().replay_manifest_enrollment(&conflict),
        Err(ManifestEnrollmentError::Conflict)
    ));
    let db = f.writer.take().unwrap().into_catalog_database();
    db.close().unwrap();
    assert!(matches!(
        first.receipt.validate(),
        Err(ManifestEnrollmentError::OwnerEnded)
    ));
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        f.root.path(),
        &f.guard,
        Path::new("enroll-db"),
    )
    .unwrap();
    f.writer = Some(
        SourceWriter::new_for_scan(db, SourceId::from_string("actual-enrollment-source"), 8)
            .unwrap(),
    );
    assert!(matches!(
        f.owner().replay_manifest_enrollment(&fresh),
        Err(ManifestEnrollmentError::OwnerEnded)
    ));
    let fresh = ManifestEnrollmentRequest::restore(&bytes, f.owner()).unwrap();
    let recovered = f
        .owner()
        .replay_manifest_enrollment(&fresh)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.receipt.manifest(), first.receipt.manifest());
    recovered.receipt.validate().unwrap();
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        before + 1
    );
}
#[test]
fn manifest_enrollment_owner_rejects_cancel_stale_foreign_batch_and_inexact_facts() {
    let f = Fixture::new();
    let r = f.request();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        f.owner()
            .commit_manifest_enrollment(&r, f.batch(&r), Some(&cancel)),
        Err(ManifestEnrollmentError::Canceled)
    ));
    assert!(f.owner().replay_manifest_enrollment(&r).unwrap().is_none());
    let mut incorrect = r.clone();
    incorrect.payload.entries[0].size = 11;
    incorrect.bytes = serde_json::to_vec(&incorrect.payload).unwrap();
    assert!(matches!(
        f.owner()
            .commit_manifest_enrollment(&incorrect, f.batch(&r), None),
        Err(ManifestEnrollmentError::Input)
    ));
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        r.current.revision()
    );
    assert!(
        f.owner()
            .database
            .entry_for_path(Path::new("音.wav"))
            .unwrap()
            .is_none()
    );
    let other = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        other.path(),
        &f.guard,
        Path::new("foreign-db"),
    )
    .unwrap();
    let other = SourceWriter::new_for_scan(db, SourceId::from_string("foreign"), 7).unwrap();
    assert!(matches!(
        f.owner().commit_manifest_enrollment(
            &r,
            other.catalog_database().write_batch().unwrap(),
            None
        ),
        Err(ManifestEnrollmentError::Input)
    ));
    f.owner()
        .database
        .set_metadata("revision", &(r.current.revision() + 1).to_string())
        .unwrap();
    assert!(matches!(
        f.owner().commit_manifest_enrollment(&r, f.batch(&r), None),
        Err(ManifestEnrollmentError::Owner(
            SourceWriterError::StaleRevision { .. }
        ))
    ));
}
#[test]
fn manifest_enrollment_owner_sql_abort_rolls_back_delta_revision_and_receipt() {
    let f = Fixture::new();
    let r = f.request();
    f.connection().execute_batch("CREATE TRIGGER abort_enrollment BEFORE INSERT ON source_manifest_enrollment_receipts BEGIN SELECT RAISE(ABORT,'injected');END;").unwrap();
    assert!(
        f.owner()
            .commit_manifest_enrollment(&r, f.batch(&r), None)
            .is_err()
    );
    assert!(f.owner().replay_manifest_enrollment(&r).unwrap().is_none());
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        r.current.revision()
    );
    assert!(
        f.owner()
            .database
            .entry_for_path(Path::new("音.wav"))
            .unwrap()
            .is_none()
    );
    f.connection()
        .execute_batch("DROP TRIGGER abort_enrollment")
        .unwrap();
    f.owner()
        .commit_manifest_enrollment(&r, f.batch(&r), None)
        .unwrap();
}
#[test]
fn manifest_enrollment_owner_bounds_canonical_custody_and_retained_evidence() {
    let f = Fixture::new();
    let r = f.request();
    let mut oversized = r.entries()[0].clone();
    oversized.file_identity = "x".repeat(513);
    assert!(
        ManifestEnrollmentRequest::new(
            r.payload.operation,
            r.payload.work,
            f.owner(),
            vec![oversized]
        )
        .is_err()
    );
    assert!(ManifestEnrollmentRequest::restore(&vec![0; 65537], f.owner()).is_err());
    let mut noncanonical = r.bytes.clone();
    noncanonical.push(b' ');
    assert!(ManifestEnrollmentRequest::restore(&noncanonical, f.owner()).is_err());
    let mut unsupported = r.clone();
    unsupported.payload.version = 2;
    let bytes = serde_json::to_vec(&unsupported.payload).unwrap();
    assert!(ManifestEnrollmentRequest::restore(&bytes, f.owner()).is_err());
    f.owner()
        .commit_manifest_enrollment(&r, f.batch(&r), None)
        .unwrap();
    let fresh = ManifestEnrollmentRequest::restore(&r.bytes, f.owner()).unwrap();
    f.connection().execute_batch("PRAGMA ignore_check_constraints=ON;UPDATE source_manifest_enrollment_receipts SET result=zeroblob(1000000);PRAGMA ignore_check_constraints=OFF;").unwrap();
    assert!(matches!(
        f.owner().replay_manifest_enrollment(&fresh),
        Err(ManifestEnrollmentError::Evidence)
    ));
}
#[test]
fn manifest_enrollment_owner_capacity_preserves_replay_and_rolls_back_new_work() {
    let f = Fixture::new();
    let r = f.request();
    f.owner()
        .commit_manifest_enrollment(&r, f.batch(&r), None)
        .unwrap();
    // Bounded-shaped rows fill actual ledger retention. They are not issued receipts.
    f.connection().execute_batch("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<8191) INSERT INTO source_manifest_enrollment_receipts(command_key,payload,payload_hash,result,result_hash) SELECT 'capacity-'||x,x'01',zeroblob(32),x'01',zeroblob(32) FROM n;").unwrap();
    let fresh = ManifestEnrollmentRequest::restore(&r.bytes, f.owner()).unwrap();
    assert!(
        f.owner()
            .replay_manifest_enrollment(&fresh)
            .unwrap()
            .unwrap()
            .replayed
    );
    let next = f.request();
    assert!(matches!(
        f.owner()
            .commit_manifest_enrollment(&next, f.batch(&next), None),
        Err(ManifestEnrollmentError::Capacity)
    ));
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        fresh.current.revision()
    );
}
#[test]
fn manifest_enrollment_owner_ambiguous_receipts_and_result_corruption_fail_closed() {
    let f = Fixture::new();
    let r = f.request();
    f.owner()
        .commit_manifest_enrollment(&r, f.batch(&r), None)
        .unwrap();
    let fresh = ManifestEnrollmentRequest::restore(&r.bytes, f.owner()).unwrap();
    f.connection()
        .execute_batch("UPDATE source_manifest_enrollment_receipts SET result_hash=zeroblob(32)")
        .unwrap();
    assert!(matches!(
        f.owner().replay_manifest_enrollment(&fresh),
        Err(ManifestEnrollmentError::Evidence)
    ));
    f.connection().execute_batch("ALTER TABLE source_manifest_enrollment_receipts RENAME TO saved_enrollment;CREATE TABLE source_manifest_enrollment_receipts AS SELECT * FROM saved_enrollment;INSERT INTO source_manifest_enrollment_receipts SELECT * FROM saved_enrollment;").unwrap();
    assert!(matches!(
        f.owner().replay_manifest_enrollment(&fresh),
        Err(ManifestEnrollmentError::Evidence)
    ));
}

#[test]
fn manifest_enrollment_owner_byte_capacity_rejects_new_work_without_eviction() {
    let f = Fixture::new();
    let r = f.request();
    f.owner()
        .commit_manifest_enrollment(&r, f.batch(&r), None)
        .unwrap();
    f.connection().execute_batch("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<129) INSERT INTO source_manifest_enrollment_receipts(command_key,payload,payload_hash,result,result_hash) SELECT 'byte-capacity-'||x,zeroblob(65536),zeroblob(32),zeroblob(65536),zeroblob(32) FROM n;").unwrap();
    let fresh = ManifestEnrollmentRequest::restore(&r.bytes, f.owner()).unwrap();
    assert!(
        f.owner()
            .replay_manifest_enrollment(&fresh)
            .unwrap()
            .unwrap()
            .replayed
    );
    let next = f.request();
    assert!(matches!(
        f.owner()
            .commit_manifest_enrollment(&next, f.batch(&next), None),
        Err(ManifestEnrollmentError::Capacity)
    ));
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        fresh.current.revision()
    );
    let rows: i64 = f
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM source_manifest_enrollment_receipts",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 130);
}
