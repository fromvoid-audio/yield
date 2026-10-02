use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter};
use crate::test_runtime::TestRuntimeGuard;
use std::path::Path;

struct Fixture {
    // Close the source before removing its roots; restore global configuration before
    // releasing process-local test isolation.
    writer: SourceWriter,
    root: tempfile::TempDir,
    _guard: WritableProfileGuard,
    _profile: PersistenceProfileGuard,
    _base_guard: ConfigBaseGuard,
    _base: tempfile::TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let base_guard = ConfigBaseGuard::set(base.path().to_owned());
        let profile = PersistenceProfileGuard::named("reference-fence");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            root.path(),
            &guard,
            Path::new("source"),
        )
        .unwrap();
        db.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
        db.begin_source_directory_truth_generation(1, 0).unwrap();
        let writer = SourceWriter::new_for_scan(db, SourceId::from_string("source"), 7).unwrap();
        let identity = writer.current_scan_fence().unwrap().root_identity().clone();
        writer
            .catalog_database()
            .write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(1, 1, 10, identity, None)
            .unwrap();
        Self {
            _runtime: runtime,
            _base: base,
            _base_guard: base_guard,
            _profile: profile,
            _guard: guard,
            root,
            writer,
        }
    }
}
#[test]
fn reference_read_fence_live_and_changed_revision() {
    let mut fixture = Fixture::new();
    let fence = fixture.writer.reference_read_fence().unwrap();
    assert_eq!(fence.source_revision().lifecycle_generation(), 7);
    fence.validate().unwrap();
    let request = crate::sample_sources::SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        crate::sample_sources::SourceCommandId::new(),
        fixture.writer.current_revision().unwrap(),
        vec![crate::sample_sources::SourceCurationUpdate::SetRating {
            path: crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                "one.wav".into(),
            )
            .unwrap(),
            rating: crate::sample_sources::Rating::KEEP_1,
        }],
    )
    .unwrap();
    fixture.writer.commit_curation(request).unwrap();
    assert!(matches!(
        fence.validate(),
        Err(SourceReferenceFenceError::RevisionChanged)
    ));
    let fresh = fixture.writer.reference_read_fence().unwrap();
    assert!(fresh.source_revision().revision() > fence.source_revision().revision());
}
#[test]
fn reference_read_fence_ends_on_database_handoff_even_when_lease_is_reused() {
    let fixture = Fixture::new();
    let old = fixture.writer.reference_read_fence().unwrap();
    let database = fixture.writer.into_catalog_database();
    assert!(matches!(
        old.validate(),
        Err(SourceReferenceFenceError::LifecycleEnded)
    ));
    let fresh = SourceWriter::new(database, SourceId::from_string("source"), 8).unwrap();
    assert_eq!(
        fresh
            .reference_read_fence()
            .unwrap()
            .source_revision()
            .lifecycle_generation(),
        8
    );
    assert!(matches!(
        old.validate(),
        Err(SourceReferenceFenceError::LifecycleEnded)
    ));
    let fence = fresh.reference_read_fence().unwrap();
    fresh.close().unwrap();
    assert!(matches!(
        fence.validate(),
        Err(SourceReferenceFenceError::LifecycleEnded)
    ));
}
#[test]
fn reference_read_fence_bounds_and_validates_audit_evidence_without_renewal() {
    let fixture = Fixture::new();
    let fence = fixture.writer.reference_read_fence().unwrap();
    let db = fixture.writer.catalog_database();
    let conn = &db.connection;
    let before: (i64, i64) = conn
        .query_row(
            "SELECT acquired_at, expires_at FROM source_lease",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    fence.validate().unwrap();
    let after = conn
        .query_row(
            "SELECT acquired_at, expires_at FROM source_lease",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(before, after);
    conn.execute(
        "UPDATE metadata SET value = zeroblob(1000000) WHERE key = ?1",
        [super::super::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY],
    )
    .unwrap();
    assert!(matches!(
        fence.validate(),
        Err(SourceReferenceFenceError::MalformedAudit)
    ));
    assert!(matches!(
        fixture.writer.reference_read_fence(),
        Err(SourceReferenceFenceError::MalformedAudit)
    ));
}
#[cfg(unix)]
#[test]
fn reference_read_fence_rejects_replaced_media_root() {
    let fixture = Fixture::new();
    let fence = fixture.writer.reference_read_fence().unwrap();
    let displaced = fixture.root.path().with_extension("displaced");
    std::fs::rename(fixture.root.path(), &displaced).unwrap();
    std::fs::create_dir(fixture.root.path()).unwrap();
    assert!(fence.validate().is_err());
    std::fs::remove_dir_all(displaced).unwrap();
}

#[test]
fn reference_read_fence_rejects_expired_lease_without_renewing() {
    let fixture = Fixture::new();
    let fence = fixture.writer.reference_read_fence().unwrap();
    let conn = &fixture.writer.catalog_database().connection;
    conn.execute("UPDATE source_lease SET expires_at = acquired_at", [])
        .unwrap();
    assert!(fence.validate().is_err());
    assert!(fixture.writer.reference_read_fence().is_err());
    let (acquired, expires): (i64, i64) = conn
        .query_row(
            "SELECT acquired_at, expires_at FROM source_lease",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(acquired, expires);
}
#[test]
fn reference_read_fence_rejects_ambiguous_audit_rows() {
    let fixture = Fixture::new();
    let fence = fixture.writer.reference_read_fence().unwrap();
    fixture.writer.catalog_database().connection.execute_batch("ALTER TABLE metadata RENAME TO old_metadata;
        CREATE TABLE metadata AS SELECT * FROM old_metadata;
        INSERT INTO metadata SELECT * FROM old_metadata WHERE key = 'revision'; DROP TABLE old_metadata;").unwrap();
    assert!(matches!(
        fence.validate(),
        Err(SourceReferenceFenceError::MalformedAudit)
    ));
}
#[cfg(unix)]
#[test]
fn reference_read_fence_rejects_replaced_database_file() {
    let fixture = Fixture::new();
    let fence = fixture.writer.reference_read_fence().unwrap();
    let path = fixture
        ._guard
        .profile_root()
        .join("source")
        .join(super::super::DB_FILE_NAME);
    std::fs::rename(&path, path.with_extension("displaced")).unwrap();
    std::fs::write(path, []).unwrap();
    assert!(fence.validate().is_err());
}
