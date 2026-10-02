//! Retained-owner continuation provenance across revision, lifecycle and physical rebinding.
use super::*;
use crate::sample_sources::{BrowserMetadataCursor, SourceRevision};
use std::time::{Duration, Instant};

/// Read the production worker primitive under its normal bounded budget.
fn read(
    writer: &SourceWriter,
    revision: &SourceRevision,
    cursor: Option<&BrowserMetadataCursor>,
) -> SourceSnapshotPage {
    writer
        .read_browser_metadata_page_with_deadline(
            revision,
            cursor,
            1,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap()
}

/// Obtain a genuine owner-issued continuation without constructing its provenance.
fn cursor(writer: &SourceWriter, revision: &SourceRevision) -> BrowserMetadataCursor {
    let SourceSnapshotPage::FreshAsOf { page, .. } = read(writer, revision, None) else {
        panic!("fresh initial page")
    };
    assert_eq!(page.files[0].relative_path, Path::new("one.wav"));
    page.next_cursor.unwrap()
}

#[test]
fn snapshot_cursor_rejects_refreshed_revision_with_old_continuation_and_restarts_at_first_row() {
    let fixture = CurationFixture::new_profile_bound();
    let mut writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("cursor-revision"),
        7,
    )
    .unwrap();
    let old = writer.current_revision().unwrap();
    let after = cursor(&writer, &old);
    writer
        .commit_curation(
            SourceWriterRequest::new(
                crate::operation_journal::OperationId::new(),
                crate::operation_journal::ParticipantId::new(),
                SourceCommandId::new(),
                old.clone(),
                vec![SourceCurationUpdate::SetLocked {
                    path: CurationFixture::path("one.wav"),
                    locked: true,
                }],
            )
            .unwrap(),
        )
        .unwrap();
    let current = writer.current_revision().unwrap();
    assert!(current.revision() > old.revision());
    let changes = writer.database_for_tests().connection.total_changes();
    assert!(
        matches!(read(&writer, &current, Some(&after)), SourceSnapshotPage::Stale { current: observed } if observed == current)
    );
    let SourceSnapshotPage::FreshAsOf { page, .. } = read(&writer, &current, None) else {
        panic!("restart must be fresh")
    };
    assert_eq!(page.files[0].relative_path, Path::new("one.wav"));
    assert!(page.files[0].locked);
    let SourceSnapshotPage::FreshAsOf { page, .. } =
        read(&writer, &current, page.next_cursor.as_ref())
    else {
        panic!("new continuation must be fresh")
    };
    assert_eq!(page.files[0].relative_path, Path::new("two.wav"));
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    writer.close().unwrap();
}

#[test]
fn snapshot_cursor_rejects_lifecycle_rebind_with_same_database_and_numeric_revision() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("cursor-lifecycle");
    let writer = SourceWriter::new(fixture.database, source.clone(), 7).unwrap();
    let old = writer.current_revision().unwrap();
    let after = cursor(&writer, &old);
    let writer = SourceWriter::new(writer.into_catalog_database(), source, 8).unwrap();
    let current = writer.current_revision().unwrap();
    assert_eq!(old.revision(), current.revision());
    assert_eq!(old.database_identity(), current.database_identity());
    assert_eq!(old.lease_epoch(), current.lease_epoch());
    assert_ne!(old.lifecycle_generation(), current.lifecycle_generation());
    assert!(
        matches!(read(&writer, &current, Some(&after)), SourceSnapshotPage::Stale { current: observed } if observed == current)
    );
    assert!(matches!(
        read(&writer, &current, None),
        SourceSnapshotPage::FreshAsOf { .. }
    ));
    writer.close().unwrap();
}

#[test]
fn snapshot_cursor_rejects_actual_lease_handoff_with_same_lifecycle_and_numeric_revision() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("cursor-lease-handoff");
    let writer = SourceWriter::new(fixture.database, source.clone(), 7).unwrap();
    let old = writer.current_revision().unwrap();
    let after = cursor(&writer, &old);
    writer.close().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let writer = SourceWriter::new(database, source, 7).unwrap();
    let current = writer.current_revision().unwrap();
    assert_eq!(old.revision(), current.revision());
    assert_eq!(old.database_identity(), current.database_identity());
    assert_eq!(old.lifecycle_generation(), current.lifecycle_generation());
    assert!(current.lease_epoch() > old.lease_epoch());
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    assert!(
        matches!(owner.read_browser_metadata_page(&current, Some(&after), 1).unwrap(), SourceSnapshotPage::Stale { current: observed } if observed == current)
    );
    assert!(matches!(
        owner.read_browser_metadata_page(&current, None, 1).unwrap(),
        SourceSnapshotPage::FreshAsOf { .. }
    ));
    owner.begin_retirement().join().unwrap().close().unwrap();
}

#[test]
fn snapshot_cursor_rejects_low_level_unbound_continuation() {
    let fixture = CurationFixture::new_profile_bound();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("cursor-unbound"), 7).unwrap();
    let current = writer.current_revision().unwrap();
    let unbound = writer
        .database_for_tests()
        .browser_metadata_page(current.revision(), None, 1)
        .unwrap()
        .next_cursor
        .unwrap();
    assert!(
        matches!(read(&writer, &current, Some(&unbound)), SourceSnapshotPage::Stale { current: observed } if observed == current)
    );
    let bound = cursor(&writer, &current);
    assert!(matches!(
        read(&writer, &current, Some(&bound)),
        SourceSnapshotPage::FreshAsOf { .. }
    ));
    writer.close().unwrap();
}

#[test]
fn snapshot_cursor_rejects_physical_source_rebind_with_same_source_id_and_numeric_revision() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("cursor-physical-rebind");
    let writer = SourceWriter::new(fixture.database, source.clone(), 7).unwrap();
    let old = writer.current_revision().unwrap();
    let after = cursor(&writer, &old);
    writer.close().unwrap();
    let replacement_root = tempfile::tempdir().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        replacement_root.path(),
        &fixture.profile_guard,
        Path::new("snapshot-rebound"),
    )
    .unwrap();
    database.upsert_file(Path::new("one.wav"), 90, 20).unwrap();
    database.upsert_file(Path::new("two.wav"), 100, 21).unwrap();
    database
        .begin_source_directory_truth_generation(1, 0)
        .unwrap();
    let root = RootIdentity::from_bytes(
        database
            .source_lease
            .as_ref()
            .unwrap()
            .media_root_binding()
            .identity()
            .as_bytes()
            .to_vec(),
    );
    database
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(1, 2, 22, root, None)
        .unwrap();
    let writer = SourceWriter::new(database, source, 7).unwrap();
    let current = writer.current_revision().unwrap();
    assert_eq!(old.source_id(), current.source_id());
    assert_eq!(old.revision(), current.revision());
    assert_eq!(old.lifecycle_generation(), current.lifecycle_generation());
    assert_eq!(old.lease_epoch(), current.lease_epoch());
    assert_ne!(old.database_identity(), current.database_identity());
    assert_ne!(old.root_binding_identity(), current.root_binding_identity());
    let changes = writer.database_for_tests().connection.total_changes();
    writer.database_for_tests().connection.authorizer(Some(
        |context: rusqlite::hooks::AuthContext<'_>| match context.action {
            rusqlite::hooks::AuthAction::Read {
                table_name: "wav_files",
                ..
            } => rusqlite::hooks::Authorization::Deny,
            _ => rusqlite::hooks::Authorization::Allow,
        },
    ));
    assert!(
        matches!(read(&writer, &current, Some(&after)), SourceSnapshotPage::Stale { current: observed } if observed == current)
    );
    writer
        .database_for_tests()
        .connection
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>);
    let SourceSnapshotPage::FreshAsOf { page, .. } = read(&writer, &current, None) else {
        panic!("rebound restart must be fresh")
    };
    assert_eq!(page.files[0].relative_path, Path::new("one.wav"));
    assert_eq!(page.files[0].file_size, 90);
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    writer.close().unwrap();
}
