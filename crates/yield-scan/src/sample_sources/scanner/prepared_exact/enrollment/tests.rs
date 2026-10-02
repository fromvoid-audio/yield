//! Real scanner/held-file/source-owner integration; no production physical qualification.
use super::*;
use tempfile::TempDir;
use yield_library::{
    app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard},
    operation_journal::ParticipantId,
    sample_sources::{SourceDatabase, SourceId},
    test_runtime::TestRuntimeGuard,
};
struct Fixture {
    owner: Option<SourceWriter>,
    source: TempDir,
    _guard: WritableProfileGuard,
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
        let profile = PersistenceProfileGuard::named("scanner-enrollment");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let source = tempfile::tempdir().unwrap();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            source.path(),
            &guard,
            Path::new("enrollment-db"),
        )
        .unwrap();
        let owner =
            SourceWriter::new_for_scan(db, SourceId::from_string("scanner-enrollment-source"), 7)
                .unwrap();
        Self {
            owner: Some(owner),
            source,
            _guard: guard,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
    fn owner(&self) -> &SourceWriter {
        self.owner.as_ref().unwrap()
    }
    fn put(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.source.path().join(name), bytes).unwrap();
    }
    fn paths(names: &[&str]) -> Vec<RootRelativePath> {
        names
            .iter()
            .map(|p| RootRelativePath::try_from_path(Path::new(p).to_path_buf()).unwrap())
            .collect()
    }
    fn key() -> WorkUnitKey {
        WorkUnitKey::new(
            0,
            70,
            ParticipantId::new(),
            yield_library::operation_journal::work_plan::WorkUnitRole::DestinationSourceReconcile,
        )
    }
}
#[test]
fn prepared_enrollment_actual_audio_index_atomic_receipt_and_lost_ack() {
    let f = Fixture::new();
    f.put("音.wav", b"audio");
    f.put("notes.txt", b"notes");
    let paths = Fixture::paths(&["音.wav", "notes.txt"]);
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    let bytes = request.proposal_bytes().to_vec();
    let before = prepared.expected_revision();
    let committed = prepared.commit_enrollment(&request, None).unwrap();
    assert!(!committed.replayed);
    committed.receipt.validate().unwrap();
    assert_eq!(committed.receipt.committed_revision(), before + 1);
    assert_eq!(committed.receipt.manifest().touched_path_changes.len(), 1);
    assert_eq!(
        committed
            .receipt
            .manifest()
            .source_index_commit
            .as_ref()
            .unwrap()
            .upserted_entries
            .len(),
        1
    );
    let hash = blake3::hash(b"audio").to_hex().to_string();
    assert_eq!(
        committed
            .receipt
            .entries()
            .iter()
            .find(|e| e.path.as_str() == "音.wav")
            .unwrap()
            .content_hash
            .as_deref(),
        Some(hash.as_str())
    );
    // A lost acknowledgement recovers the stored original without reopening the files.
    std::fs::remove_file(f.source.path().join("音.wav")).unwrap();
    let restored = ManifestEnrollmentRequest::restore(&bytes, f.owner()).unwrap();
    let recovered = f
        .owner()
        .replay_manifest_enrollment(&restored)
        .unwrap()
        .unwrap();
    assert!(recovered.replayed);
    assert_eq!(recovered.receipt.manifest(), committed.receipt.manifest());
    assert_eq!(
        f.owner().current_scan_fence().unwrap().revision(),
        before + 1
    );
    assert!(
        f.owner()
            .catalog_database()
            .get_metadata("last_scan_completed_at")
            .unwrap()
            .is_none()
    );
}
#[test]
fn prepared_enrollment_rejects_substitution_cancel_and_mismatched_proposal_without_delta() {
    let f = Fixture::new();
    f.put("a.wav", b"old");
    f.put("n.txt", b"index");
    let paths = Fixture::paths(&["a.wav", "n.txt"]);
    let before = f.owner().current_scan_fence().unwrap().revision();
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    std::fs::rename(
        f.source.path().join("a.wav"),
        f.source.path().join("saved.wav"),
    )
    .unwrap();
    f.put("a.wav", b"new");
    assert!(prepared.commit_enrollment(&request, None).is_err());
    assert!(
        f.owner()
            .replay_manifest_enrollment(&request)
            .unwrap()
            .is_none()
    );
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    assert!(matches!(
        prepared.commit_enrollment(&request, Some(&AtomicBool::new(true))),
        Err(ScanError::Canceled)
    ));
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let original = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    let mut entries = original.entries().to_vec();
    entries[0].size += 1;
    let mismatched =
        ManifestEnrollmentRequest::new(OperationId::new(), Fixture::key(), f.owner(), entries)
            .unwrap();
    assert!(prepared.commit_enrollment(&mismatched, None).is_err());
    assert_eq!(f.owner().current_scan_fence().unwrap().revision(), before);
    assert!(
        f.owner()
            .catalog_database()
            .entry_for_path(Path::new("a.wav"))
            .unwrap()
            .is_none()
    );
    assert!(
        f.owner()
            .catalog_database()
            .source_index_snapshot()
            .unwrap()
            .entries
            .is_empty()
    );
}
#[test]
fn prepared_enrollment_old_custody_cannot_create_new_receipt_after_revision_change() {
    let f = Fixture::new();
    f.put("a.wav", b"content");
    let paths = Fixture::paths(&["a.wav"]);
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    let bytes = request.proposal_bytes().to_vec();
    f.owner()
        .catalog_database()
        .set_metadata(
            "revision",
            &(request.current_fence().revision() + 1).to_string(),
        )
        .unwrap();
    assert!(prepared.commit_enrollment(&request, None).is_err());
    let restored = ManifestEnrollmentRequest::restore(&bytes, f.owner()).unwrap();
    assert!(
        f.owner()
            .replay_manifest_enrollment(&restored)
            .unwrap()
            .is_none()
    );
    let fresh = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    assert!(fresh.commit_enrollment(&restored, None).is_err());
    assert!(
        f.owner()
            .catalog_database()
            .entry_for_path(Path::new("a.wav"))
            .unwrap()
            .is_none()
    );
}
#[test]
fn prepared_enrollment_receipt_ends_on_real_source_writer_handoff() {
    let mut f = Fixture::new();
    f.put("a.wav", b"content");
    let paths = Fixture::paths(&["a.wav"]);
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    let committed = prepared.commit_enrollment(&request, None).unwrap();
    let bytes = request.proposal_bytes().to_vec();
    let db = f.owner.take().unwrap().into_catalog_database();
    f.owner = Some(
        SourceWriter::new_for_scan(db, SourceId::from_string("scanner-enrollment-source"), 8)
            .unwrap(),
    );
    assert!(matches!(
        committed.receipt.validate(),
        Err(ManifestEnrollmentError::OwnerEnded)
    ));
    let restored = ManifestEnrollmentRequest::restore(&bytes, f.owner()).unwrap();
    let replay = f
        .owner()
        .replay_manifest_enrollment(&restored)
        .unwrap()
        .unwrap();
    replay.receipt.validate().unwrap();
    assert_eq!(replay.receipt.manifest(), committed.receipt.manifest());
}
#[cfg(unix)]
#[test]
fn prepared_enrollment_receipt_rejects_replaced_source_root() {
    let f = Fixture::new();
    f.put("a.wav", b"content");
    let paths = Fixture::paths(&["a.wav"]);
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    let result = prepared.commit_enrollment(&request, None).unwrap();
    let replacement = tempfile::tempdir().unwrap();
    let held = replacement.path().join("original");
    std::fs::rename(f.source.path(), &held).unwrap();
    std::fs::create_dir(f.source.path()).unwrap();
    assert!(result.receipt.validate().is_err());
}

#[test]
fn prepared_enrollment_bounds_corrupt_index_revision_before_preparation_and_commit() {
    let f = Fixture::new();
    f.put("a.wav", b"content");
    let paths = Fixture::paths(&["a.wav"]);
    let key = yield_library::sample_sources::db::META_SOURCE_INDEX_REVISION;
    let before = f.owner().current_scan_fence().unwrap().revision();
    let set_index = |value: &str| {
        let mut batch = f.owner().catalog_database().write_batch().unwrap();
        batch.set_metadata(key, value).unwrap();
        batch.commit_auxiliary_state().unwrap();
    };
    set_index(&"9".repeat(1000000));
    assert!(prepare_exact_scan(f.owner(), &paths, None).is_err());
    set_index("0");
    let prepared = prepare_exact_scan(f.owner(), &paths, None).unwrap();
    let request = prepared
        .enrollment_request(OperationId::new(), Fixture::key())
        .unwrap();
    set_index(&"9".repeat(1000000));
    assert!(prepared.commit_enrollment(&request, None).is_err());
    assert_eq!(f.owner().current_scan_fence().unwrap().revision(), before);
    assert!(
        f.owner()
            .catalog_database()
            .entry_for_path(Path::new("a.wav"))
            .unwrap()
            .is_none()
    );
    assert!(
        f.owner()
            .replay_manifest_enrollment(&request)
            .unwrap()
            .is_none()
    );
}
