use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter};
use crate::test_runtime::TestRuntimeGuard;
use std::path::Path;

struct Fixture {
    writer: Option<SourceWriter>,
    root: tempfile::TempDir,
    _guard: WritableProfileGuard,
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: tempfile::TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().unwrap();
        let config = ConfigBaseGuard::set(base.path().to_owned());
        let profile = PersistenceProfileGuard::named("file-observation");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            root.path(),
            &guard,
            Path::new("source"),
        )
        .unwrap();
        db.upsert_file(Path::new("nested/é.wav"), 10, -1).unwrap();
        // Seeded authoritative metadata is storage input, not physical hash qualification.
        db.connection
            .execute(
                "UPDATE wav_files SET file_identity='object-1',content_hash=?1",
                ["a".repeat(64)],
            )
            .unwrap();
        db.connection.execute_batch("INSERT INTO source_index_entries(path,path_encoding,classification,file_size,modified_ns,file_identity,format_policy_version) VALUES('plain.txt',0,'unsupported_non_audio',3,2,'index-object',1);").unwrap();
        db.begin_source_directory_truth_generation(1, 0).unwrap();
        let writer =
            SourceWriter::new_for_scan(db, SourceId::from_string("actual-source"), 7).unwrap();
        let identity = writer.current_scan_fence().unwrap().root_identity().clone();
        writer
            .catalog_database()
            .write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(1, 1, 10, identity, None)
            .unwrap();
        Self {
            writer: Some(writer),
            root,
            _guard: guard,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
    fn writer(&self) -> &SourceWriter {
        self.writer.as_ref().unwrap()
    }
    fn connection(&self) -> &Connection {
        &self.writer().catalog_database().connection
    }
    fn observe(&self, path: &str) -> Result<SourceFileObservation, SourceFileObservationError> {
        self.writer()
            .observe_file_identity(StoredRelativePath::new(path).unwrap())
    }
}
#[test]
fn file_identity_observation_live_audio_index_unicode_and_content_gate() {
    let f = Fixture::new();
    assert_eq!(f.writer().catalog_database().root.as_path(), f.root.path());
    let observation = f.observe("nested/é.wav").unwrap();
    observation.validate().unwrap();
    assert_eq!(observation.path().as_str(), "nested/é.wav");
    assert_eq!(
        observation.source_revision().source_id().as_str(),
        "actual-source"
    );
    assert_eq!(observation.source_revision().lifecycle_generation(), 7);
    assert_eq!(
        observation.kind(),
        SourceFileObservationKind::SupportedAudio
    );
    assert_eq!(observation.file_size(), 10);
    assert_eq!(observation.modified_ns(), -1);
    assert_eq!(observation.file_identity(), Some("object-1"));
    let ready = observation.into_content_identity().unwrap();
    assert_eq!(ready.content_hash(), "a".repeat(64));
    ready.validate().unwrap();
    let index = f.observe("plain.txt").unwrap();
    assert_eq!(
        index.kind(),
        SourceFileObservationKind::IndexOnly(SourceIndexClassification::UnsupportedNonAudio)
    );
    assert_eq!(index.file_size(), 3);
    assert_eq!(index.format_policy_version(), Some(1));
    assert_eq!(index.file_identity(), Some("index-object"));
    index.validate().unwrap();
    assert!(matches!(
        index.into_content_identity(),
        Err(SourceFileObservationError::ContentAuditRequired)
    ));
    f.connection()
        .execute("UPDATE wav_files SET content_hash=NULL", [])
        .unwrap();
    assert!(matches!(
        f.observe("nested/é.wav").unwrap().into_content_identity(),
        Err(SourceFileObservationError::ContentAuditRequired)
    ));
}
#[test]
fn file_identity_observation_exact_row_recheck_catches_auxiliary_changes() {
    let f = Fixture::new();
    let observation = f.observe("nested/é.wav").unwrap();
    let ready = observation.clone().into_content_identity().unwrap();
    let fence = f.writer().reference_read_fence().unwrap();
    f.connection()
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    fence.validate().unwrap();
    assert!(matches!(
        observation.validate(),
        Err(SourceFileObservationError::IdentityChanged)
    ));
    assert!(matches!(
        ready.validate(),
        Err(SourceFileObservationError::IdentityChanged)
    ));
    let index = f.observe("plain.txt").unwrap();
    f.connection()
        .execute(
            "UPDATE source_index_entries SET format_policy_version=2",
            [],
        )
        .unwrap();
    assert!(matches!(
        index.validate(),
        Err(SourceFileObservationError::IdentityChanged)
    ));
}
#[test]
fn file_identity_observation_rejects_changed_revision_and_ended_writer() {
    let mut f = Fixture::new();
    let observation = f.observe("nested/é.wav").unwrap();
    let writer = f.writer.as_mut().unwrap();
    let update = crate::sample_sources::SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        crate::sample_sources::SourceCommandId::new(),
        writer.current_revision().unwrap(),
        vec![crate::sample_sources::SourceCurationUpdate::SetRating {
            path: crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                "nested/é.wav".into(),
            )
            .unwrap(),
            rating: crate::sample_sources::Rating::KEEP_1,
        }],
    )
    .unwrap();
    writer.commit_curation(update).unwrap();
    assert!(matches!(
        observation.validate(),
        Err(SourceFileObservationError::Fence(
            SourceReferenceFenceError::RevisionChanged
        ))
    ));
    let fresh = f.observe("nested/é.wav").unwrap();
    let writer = f.writer.take().unwrap();
    let db = writer.into_catalog_database();
    f.writer = Some(SourceWriter::new(db, SourceId::from_string("actual-source"), 8).unwrap());
    assert!(matches!(
        fresh.validate(),
        Err(SourceFileObservationError::Fence(
            SourceReferenceFenceError::LifecycleEnded
        ))
    ));
    let final_observation = f.observe("nested/é.wav").unwrap();
    f.writer.take().unwrap().close().unwrap();
    assert!(matches!(
        final_observation.validate(),
        Err(SourceFileObservationError::Fence(
            SourceReferenceFenceError::LifecycleEnded
        ))
    ));
}
#[test]
fn file_identity_observation_bounds_missing_unavailable_and_malformed() {
    let f = Fixture::new();
    assert!(matches!(
        f.observe("absent.wav"),
        Err(SourceFileObservationError::Missing)
    ));
    for sql in [
        "UPDATE wav_files SET file_identity=zeroblob(1000000)",
        "UPDATE wav_files SET file_identity=printf('%1000000s','x')",
    ] {
        f.connection().execute(sql, []).unwrap();
        assert!(matches!(
            f.observe("nested/é.wav"),
            Err(SourceFileObservationError::Malformed)
        ));
    }
    f.connection()
        .execute(
            "UPDATE wav_files SET file_identity='object-1',content_hash=?1",
            ["x".repeat(1000000)],
        )
        .unwrap();
    assert!(matches!(
        f.observe("nested/é.wav"),
        Err(SourceFileObservationError::Malformed)
    ));
    f.connection()
        .execute("UPDATE wav_files SET content_hash='not-a-full-hash'", [])
        .unwrap();
    assert!(matches!(
        f.observe("nested/é.wav"),
        Err(SourceFileObservationError::Malformed)
    ));
    f.connection()
        .execute("UPDATE wav_files SET content_hash=NULL,missing=1", [])
        .unwrap();
    assert!(matches!(
        f.observe("nested/é.wav"),
        Err(SourceFileObservationError::Unavailable)
    ));
    f.connection()
        .execute("UPDATE source_index_entries SET file_size=NULL", [])
        .unwrap();
    assert!(matches!(
        f.observe("plain.txt"),
        Err(SourceFileObservationError::Unavailable)
    ));
    f.connection()
        .execute(
            "UPDATE source_index_entries SET file_size=3,path_encoding=1",
            [],
        )
        .unwrap();
    assert!(matches!(
        f.observe("plain.txt"),
        Err(SourceFileObservationError::Malformed)
    ));
    f.connection()
        .execute_batch(
            "PRAGMA ignore_check_constraints=ON;
             UPDATE source_index_entries SET path_encoding=0,classification='audio';
             PRAGMA ignore_check_constraints=OFF;",
        )
        .unwrap();
    assert!(matches!(
        f.observe("plain.txt"),
        Err(SourceFileObservationError::Malformed)
    ));
    let invalid: StoredRelativePath = serde_json::from_str("\"../escape\"").unwrap();
    assert!(matches!(
        f.writer().observe_file_identity(invalid),
        Err(SourceFileObservationError::InvalidPath)
    ));
}
#[test]
fn file_identity_observation_rejects_overlapping_and_duplicate_namespaces() {
    let f = Fixture::new();
    f.connection().execute_batch("INSERT INTO source_index_entries(path,path_encoding,classification,file_size,modified_ns,format_policy_version) VALUES('nested/é.wav',0,'unsupported_audio',10,-1,1);").unwrap();
    assert!(matches!(
        f.observe("nested/é.wav"),
        Err(SourceFileObservationError::Ambiguous)
    ));
    f.connection()
        .execute(
            "DELETE FROM source_index_entries WHERE path='nested/é.wav'",
            [],
        )
        .unwrap();
    f.observe("nested/é.wav").unwrap();
    f.connection().execute_batch("ALTER TABLE wav_files RENAME TO original_wav_files; CREATE TABLE wav_files AS SELECT * FROM original_wav_files; INSERT INTO wav_files SELECT * FROM original_wav_files;").unwrap();
    assert!(matches!(
        f.observe("nested/é.wav"),
        Err(SourceFileObservationError::Ambiguous)
    ));
}
#[test]
fn file_identity_observation_retained_query_does_not_reopen_or_renew_and_expiry_fails() {
    let f = Fixture::new();
    let conn = f.connection();
    let before: (i64, i64) = conn
        .query_row("SELECT acquired_at,expires_at FROM source_lease", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // Observe/readback cannot create another source database open or extend its lease.
    #[cfg(debug_assertions)]
    let counter = super::super::test_scope_source_db_open_total_count(f.root.path());
    #[cfg(debug_assertions)]
    let opens = super::super::test_source_db_open_total_count(f.root.path());
    let observation = f.observe("nested/é.wav").unwrap();
    observation.validate().unwrap();
    #[cfg(debug_assertions)]
    assert_eq!(
        super::super::test_source_db_open_total_count(f.root.path()),
        opens
    );
    #[cfg(debug_assertions)]
    drop(counter);
    let after: (i64, i64) = conn
        .query_row("SELECT acquired_at,expires_at FROM source_lease", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(before, after);
    conn.execute("UPDATE source_lease SET expires_at=acquired_at", [])
        .unwrap();
    assert!(observation.validate().is_err());
    assert!(f.observe("nested/é.wav").is_err());
    let (acquired, expires): (i64, i64) = conn
        .query_row("SELECT acquired_at,expires_at FROM source_lease", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(acquired, expires);
}
#[cfg(unix)]
#[test]
fn file_identity_observation_replaced_media_root_fails_closed() {
    let f = Fixture::new();
    let observation = f.observe("nested/é.wav").unwrap();
    let displaced = f.root.path().with_extension("displaced");
    std::fs::rename(f.root.path(), &displaced).unwrap();
    std::fs::create_dir(f.root.path()).unwrap();
    assert!(observation.validate().is_err());
    assert!(f.observe("nested/é.wav").is_err());
    std::fs::remove_dir_all(displaced).unwrap();
}
#[cfg(unix)]
#[test]
fn file_identity_observation_replaced_database_fails_closed() {
    let f = Fixture::new();
    let observation = f.observe("nested/é.wav").unwrap();
    let path = f.writer().catalog_database().db_path.clone();
    let displaced = path.with_extension("displaced");
    std::fs::rename(&path, &displaced).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    assert!(observation.validate().is_err());
    assert!(f.observe("nested/é.wav").is_err());
    std::fs::remove_file(displaced).unwrap();
}
