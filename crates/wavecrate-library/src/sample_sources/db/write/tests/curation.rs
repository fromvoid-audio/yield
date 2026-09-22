use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tempfile::TempDir;

use super::helpers::revision_value;
use crate::app_dirs::{
    ConfigBaseGuard, PersistenceProfileGuard, ProfileOwnershipError, WritableProfileGuard,
};
#[cfg(unix)]
use crate::sample_sources::db::SourceLeaseError;
#[cfg(unix)]
use crate::sample_sources::db::file_ops_journal::{CopyJournalEntryInit, FileOpJournalEntry};
use crate::sample_sources::db::{DatabaseRootBindingGuard, SourceDirectoryTruthState};
use crate::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use crate::sample_sources::{
    Rating, SOURCE_CURATION_BATCH_LIMIT, SOURCE_CURATION_MAX_TAG_LABEL_BYTES, SampleCollection,
    SourceCommandId, SourceCurationUpdate, SourceDatabase, SourceDbError, SourceId,
    SourcePlaybackType, SourceWriter, SourceWriterError, SourceWriterRequest,
};

struct CurationFixture {
    _runtime: crate::test_runtime::TestRuntimeGuard,
    _config_base_dir: TempDir,
    _config_base: ConfigBaseGuard,
    _profile: PersistenceProfileGuard,
    _source: TempDir,
    #[cfg(unix)]
    profile_child: Option<PathBuf>,
    database: SourceDatabase,
    profile_guard: WritableProfileGuard,
    root_identity: RootIdentity,
    revision: u64,
}

impl CurationFixture {
    fn new() -> Self {
        Self::new_with_profile_child(None)
    }

    fn new_profile_bound() -> Self {
        Self::new_with_profile_child(Some(PathBuf::from("curation-profile-bound")))
    }

    fn new_profile_bound_with_extra_files(extra_files: usize) -> Self {
        Self::new_with_profile_child_and_extra_files(
            Some(PathBuf::from("curation-profile-bound")),
            extra_files,
        )
    }

    fn new_with_profile_child(profile_child: Option<PathBuf>) -> Self {
        Self::new_with_profile_child_and_extra_files(profile_child, 0)
    }

    fn new_with_profile_child_and_extra_files(
        profile_child: Option<PathBuf>,
        extra_files: usize,
    ) -> Self {
        let runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        // macOS exposes the default temporary root through /var -> /private/var. Resolve that
        // ambient alias before exercising the no-follow profile capability.
        let temporary_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
        let config_base_dir = tempfile::tempdir_in(temporary_root).expect("config base");
        let config_base = ConfigBaseGuard::set(config_base_dir.path().to_path_buf());
        let profile = PersistenceProfileGuard::named("source-curation-tests");
        let profile_guard = WritableProfileGuard::acquire_current().expect("profile guard");
        let source = tempfile::tempdir().expect("source root");
        let database = match profile_child.as_ref() {
            Some(profile_child) => SourceDatabase::open_for_catalog_with_profile_guard(
                source.path(),
                &profile_guard,
                profile_child,
            )
            .expect("profile-bound source db"),
            None => SourceDatabase::open_for_source_write(source.path()).expect("source db"),
        };
        database
            .upsert_file(Path::new("one.wav"), 10, 1)
            .expect("first file");
        database
            .upsert_file(Path::new("two.wav"), 20, 2)
            .expect("second file");
        for index in 0..extra_files {
            database
                .upsert_file(Path::new(&format!("extra-{index:03}.wav")), 1, 1)
                .expect("extra file");
        }
        database
            .begin_source_directory_truth_generation(1, 0)
            .expect("directory generation");
        let root_identity = match profile_child.as_ref() {
            Some(_) => RootIdentity::from_bytes(
                database
                    .source_lease
                    .as_ref()
                    .expect("profile-bound source lease")
                    .media_root_binding()
                    .identity()
                    .as_bytes()
                    .to_vec(),
            ),
            None => RootIdentity::from_bytes(b"curation-root".to_vec()),
        };
        database
            .write_batch()
            .expect("audit batch")
            .commit_manifest_audit_with_staged_directory_truth(
                1,
                (2 + extra_files) as u64,
                10,
                root_identity.clone(),
                None,
            )
            .expect("authoritative audit");
        let revision = database.get_revision().expect("audit revision");
        assert_eq!(revision, (3 + extra_files) as u64);

        Self {
            _runtime: runtime,
            _config_base_dir: config_base_dir,
            _config_base: config_base,
            _profile: profile,
            _source: source,
            #[cfg(unix)]
            profile_child,
            database,
            profile_guard,
            root_identity,
            revision,
        }
    }

    fn path(path: &str) -> RootRelativePath {
        RootRelativePath::try_from_path(PathBuf::from(path)).expect("root-relative path")
    }

    fn commit(
        &self,
        updates: &[SourceCurationUpdate],
    ) -> Result<
        crate::sample_sources::SourceCurationCommitReceipt,
        crate::sample_sources::SourceDbError,
    > {
        self.database.commit_curation_with_profile_guard(
            &self.profile_guard,
            self.revision,
            &self.root_identity,
            updates,
        )
    }
}

/// Test-only binding whose retained identity can be invalidated on every host.
///
/// Unix tests below exercise actual profile-child replacement. This seam keeps the same
/// profile-bound database path while proving the mutation fence does not depend on a particular
/// platform's rename semantics.
struct ControllableDatabaseRootBinding {
    valid: Arc<AtomicBool>,
}

impl DatabaseRootBindingGuard for ControllableDatabaseRootBinding {
    fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError> {
        if self.valid.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ProfileOwnershipError::ProfileRootReplaced {
                path: profile_guard.profile_root().to_path_buf(),
            })
        }
    }
}

fn metadata_number(database: &SourceDatabase, key: &str) -> u64 {
    database
        .get_metadata(key)
        .expect("metadata")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

#[test]
fn source_writer_manifest_snapshot_pages_are_fenced_and_leave_files_unchanged() {
    fn file_state(path: &Path) -> Option<(u64, std::time::SystemTime)> {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.len(), metadata.modified().expect("modified time")))
    }

    let fixture = CurationFixture::new_profile_bound();
    let mut writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("source-manifest-snapshot"),
        1,
    )
    .expect("source writer");
    let expected = writer.current_revision().expect("source revision");
    let database_path = writer.database_for_tests().db_path.clone();
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", database_path.display()));
    let before = [
        file_state(&database_path),
        file_state(&wal_path),
        file_state(&shm_path),
    ];
    let changes_before = writer.database_for_tests().connection.total_changes();
    let cancellation = Arc::new(AtomicBool::new(false));
    let first = writer
        .read_manifest_snapshot_page(&expected, None, 1, &cancellation)
        .expect("first page");
    assert_eq!(first.revision, expected);
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].relative_path, PathBuf::from("one.wav"));
    first.publication_fence.validate().expect("live page fence");
    let cursor = first.next_cursor.expect("continuation");
    let second = writer
        .read_manifest_snapshot_page(&expected, Some(&cursor), 1, &cancellation)
        .expect("second page");
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].relative_path, PathBuf::from("two.wav"));
    assert!(second.next_cursor.is_none());
    let after = [
        file_state(&database_path),
        file_state(&wal_path),
        file_state(&shm_path),
    ];
    assert_eq!(
        after, before,
        "owner reads must leave DB and sidecar metadata unchanged"
    );
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes_before,
        "owner reads must execute no SQL mutation"
    );

    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(81)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(82)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(83)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("revision-changing curation");
    writer.commit_curation(request).expect("commit curation");
    assert!(matches!(
        writer.read_manifest_snapshot_page(&expected, Some(&cursor), 1, &cancellation),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    let published_fence = second.publication_fence.clone();
    writer.close().expect("close source writer");
    assert!(published_fence.validate().is_err());
    let reopened_database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        fixture.profile_child.as_ref().expect("profile child"),
    )
    .expect("reopen source database");
    let reopened = SourceWriter::new(
        reopened_database,
        SourceId::from_string("source-manifest-snapshot"),
        2,
    )
    .expect("new lifecycle owner");
    assert!(matches!(
        reopened.read_manifest_snapshot_page(&expected, Some(&cursor), 1, &cancellation),
        Err(SourceWriterError::StaleFence)
    ));
    reopened.close().expect("close rebound source writer");
}

#[test]
fn source_writer_manifest_snapshot_rejects_cancellation_and_oversized_row() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("source-manifest-snapshot-budget"),
        1,
    )
    .expect("source writer");
    let expected = writer.current_revision().expect("source revision");
    let cancellation = Arc::new(AtomicBool::new(true));
    assert!(matches!(
        writer.read_manifest_snapshot_page(&expected, None, 64, &cancellation),
        Err(SourceWriterError::Database(SourceDbError::Canceled))
    ));
    cancellation.store(false, Ordering::Release);
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE wav_files SET content_hash = ?1 WHERE path = 'one.wav'",
            ["x".repeat(16 * 1024 * 1024)],
        )
        .expect("seed oversized persisted row");
    assert!(matches!(
        writer.read_manifest_snapshot_page(&expected, None, 64, &cancellation),
        Err(SourceWriterError::SnapshotBudgetExceeded("decoded bytes"))
    ));
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE wav_files SET content_hash = NULL, file_identity = x'FF'
             WHERE path = 'one.wav'",
            [],
        )
        .expect("seed malformed persisted identity");
    assert!(matches!(
        writer.read_manifest_snapshot_page(&expected, None, 64, &cancellation),
        Err(SourceWriterError::SnapshotMalformed(_))
    ));
    writer.close().expect("close source writer");
}

#[test]
fn source_writer_manifest_snapshot_caps_page_at_64_and_continues_exactly() {
    let fixture = CurationFixture::new_profile_bound_with_extra_files(63);
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("source-manifest-snapshot-page-bound"),
        1,
    )
    .expect("source writer");
    let expected = writer.current_revision().expect("source revision");
    let cancellation = Arc::new(AtomicBool::new(false));
    let first = writer
        .read_manifest_snapshot_page(&expected, None, 64, &cancellation)
        .expect("bounded first page");
    assert_eq!(first.entries.len(), 64);
    assert_eq!(
        first.entries[0].relative_path,
        PathBuf::from("extra-000.wav")
    );
    assert_eq!(first.entries[63].relative_path, PathBuf::from("one.wav"));
    let second = writer
        .read_manifest_snapshot_page(&expected, first.next_cursor.as_ref(), 64, &cancellation)
        .expect("exact continuation");
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].relative_path, PathBuf::from("two.wav"));
    assert!(second.next_cursor.is_none());
    assert!(matches!(
        writer.read_manifest_snapshot_page(&expected, None, 65, &cancellation),
        Err(SourceWriterError::InvalidRequest(_))
    ));
    writer.close().expect("close source writer");
}

fn continuity_state(database: &SourceDatabase) -> (u64, SourceDirectoryTruthState) {
    (
        database
            .last_authoritative_audit_metadata()
            .expect("audit metadata")
            .expect("audit metadata present")
            .source_revision,
        database
            .source_directory_truth_state()
            .expect("directory truth state"),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceWriterStateSnapshot {
    revision: u64,
    continuity: (u64, SourceDirectoryTruthState),
    paths_revision: u64,
    identities_revision: u64,
    index_revision: u64,
    entry: (i64, i64, Option<i64>, Option<i64>),
    receipt_count: i64,
}

fn source_writer_state_snapshot(database: &SourceDatabase) -> SourceWriterStateSnapshot {
    SourceWriterStateSnapshot {
        revision: database.get_revision().expect("source revision"),
        continuity: continuity_state(database),
        paths_revision: metadata_number(database, "wav_paths_revision_v1"),
        identities_revision: metadata_number(database, "wav_identities_revision_v1"),
        index_revision: metadata_number(database, "source_index_revision_v1"),
        entry: database
            .connection
            .query_row(
                "SELECT tag, locked, last_played_at, last_curated_at
                 FROM wav_files WHERE path = 'one.wav'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .expect("curation state row"),
        receipt_count: database
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("receipt count"),
    }
}

#[test]
fn playback_history_commits_preserve_curation_and_reject_stale_writers() {
    let mut fixture = CurationFixture::new();
    let initial = fixture
        .commit(&[
            SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            },
            SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            },
            SourceCurationUpdate::AddCollection {
                path: CurationFixture::path("one.wav"),
                collection: SampleCollection::new(2).unwrap(),
            },
        ])
        .unwrap();
    fixture.revision = initial.source_revision();
    let mut expected = initial.files()[0].clone();
    let update = SourceCurationUpdate::SetLastPlayedAt {
        path: CurationFixture::path("one.wav"),
        played_at: Some(1_800_000_000),
    };
    let receipt = fixture.commit(&[update]).unwrap();
    expected.last_played_at = Some(1_800_000_000);
    assert_eq!(receipt.files(), &[expected.clone()]);
    assert_eq!(receipt.source_revision(), fixture.revision + 1);
    assert_eq!(
        receipt.directory_generation(),
        initial.directory_generation()
    );
    assert_eq!(receipt.root_identity(), initial.root_identity());
    let continuity = continuity_state(&fixture.database);
    assert_eq!(continuity.0, receipt.source_revision());

    let clear = SourceCurationUpdate::SetLastPlayedAt {
        path: CurationFixture::path("one.wav"),
        played_at: None,
    };
    assert!(fixture.commit(std::slice::from_ref(&clear)).is_err());
    assert_eq!(continuity_state(&fixture.database), continuity);
    let snapshot = fixture.database.browser_metadata_snapshot().unwrap();
    assert_eq!(snapshot.revision, receipt.source_revision());
    assert_eq!(snapshot.files[0], expected);

    fixture.revision = receipt.source_revision();
    let cleared = fixture.commit(&[clear]).unwrap();
    expected.last_played_at = None;
    assert_eq!(cleared.files(), &[expected]);
    assert_eq!(cleared.source_revision(), fixture.revision + 1);
    assert_eq!(
        continuity_state(&fixture.database).0,
        cleared.source_revision()
    );
}

#[test]
fn valid_curation_commit_preserves_continuity_and_returns_bounded_rows() {
    let fixture = CurationFixture::new();
    let paths_revision = metadata_number(&fixture.database, "wav_paths_revision_v1");
    let identities_revision = metadata_number(&fixture.database, "wav_identities_revision_v1");
    let index_revision = metadata_number(&fixture.database, "source_index_revision_v1");
    let unrelated_before = fixture
        .database
        .entry_for_path(Path::new("two.wav"))
        .expect("unrelated row")
        .expect("unrelated row present");

    let first = SampleCollection::new(0).expect("collection slot");
    let second = SampleCollection::new(1).expect("collection slot");
    let updates = [
        SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        },
        SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        },
        SourceCurationUpdate::AddCollection {
            path: CurationFixture::path("one.wav"),
            collection: first,
        },
        SourceCurationUpdate::AddCollection {
            path: CurationFixture::path("one.wav"),
            collection: second,
        },
    ];
    let receipt = fixture.commit(&updates).expect("curation commit");

    assert_eq!(receipt.source_revision(), fixture.revision + 1);
    assert_eq!(receipt.root_identity(), &fixture.root_identity);
    assert_eq!(receipt.directory_generation(), 1);
    assert_eq!(receipt.files().len(), 1);
    assert_eq!(receipt.files()[0].relative_path, PathBuf::from("one.wav"));
    assert_eq!(receipt.files()[0].rating, Rating::KEEP_3);
    assert!(receipt.files()[0].locked);
    assert_eq!(receipt.files()[0].collections, vec![first, second]);
    assert!(receipt.files()[0].last_curated_at.is_some());

    assert_eq!(
        fixture.database.get_revision().unwrap(),
        fixture.revision + 1
    );
    assert_eq!(
        metadata_number(&fixture.database, "wav_paths_revision_v1"),
        paths_revision
    );
    assert_eq!(
        metadata_number(&fixture.database, "wav_identities_revision_v1"),
        identities_revision
    );
    assert_eq!(
        metadata_number(&fixture.database, "source_index_revision_v1"),
        index_revision
    );
    assert_eq!(
        continuity_state(&fixture.database),
        (
            fixture.revision + 1,
            SourceDirectoryTruthState::Active {
                generation: 1,
                published_source_revision: fixture.revision + 1,
            }
        )
    );
    assert_eq!(
        fixture
            .database
            .last_authoritative_audit_metadata()
            .unwrap()
            .unwrap()
            .root_identity,
        fixture.root_identity
    );
    let unrelated_after = fixture
        .database
        .entry_for_path(Path::new("two.wav"))
        .unwrap()
        .unwrap();
    assert_eq!(unrelated_after.tag, unrelated_before.tag);
    assert_eq!(unrelated_after.file_size, unrelated_before.file_size);
    assert_eq!(unrelated_after.modified_ns, unrelated_before.modified_ns);
}

#[test]
fn curation_receipt_matches_browser_playback_type_projection() {
    let fixture = CurationFixture::new();
    let path = Path::new("one.wav");
    let mut tag_batch = fixture.database.write_batch().expect("tag batch");
    tag_batch
        .assign_tag_to_path(path, " One\t-\tShot ")
        .expect("canonical playback tag");
    tag_batch
        .commit_auxiliary_state()
        .expect("auxiliary tag commit");

    let revision = fixture.revision;
    let snapshot = fixture
        .database
        .browser_metadata_snapshot()
        .expect("browser snapshot");
    let page = fixture
        .database
        .browser_metadata_page(revision, None, 64)
        .expect("browser page");
    let exact = fixture
        .database
        .browser_metadata_for_paths_at_revision(revision, &[path.to_path_buf()])
        .expect("exact browser metadata");
    let receipt = fixture
        .commit(&[SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_1,
        }])
        .expect("curation receipt");

    let expected = Some(SourcePlaybackType::OneShot);
    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == path)
            .expect("snapshot row")
            .playback_type,
        expected
    );
    assert_eq!(
        page.files
            .iter()
            .find(|file| file.relative_path == path)
            .expect("page row")
            .playback_type,
        expected
    );
    assert_eq!(exact.files[0].playback_type, expected);
    assert_eq!(receipt.files()[0].playback_type, expected);
}

#[test]
fn curation_tag_mutations_replace_assigned_playback_types_and_preserve_locked_metadata() {
    let mut fixture = CurationFixture::new();
    let one = Path::new("one.wav");
    let two = Path::new("two.wav");
    let mut seed = fixture.database.write_batch().expect("tag batch");
    for label in [
        "loop",
        "loop-",
        "oneshot",
        "one-shot",
        "unrelated",
        "embedded\0tag",
    ] {
        seed.assign_tag_to_path(one, label).expect("seed tag");
    }
    seed.commit_auxiliary_state().expect("seed tags");

    let locked = fixture
        .commit(&[
            SourceCurationUpdate::SetRating {
                path: CurationFixture::path("two.wav"),
                rating: Rating::KEEP_3,
            },
            SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("two.wav"),
                locked: true,
            },
        ])
        .expect("lock second file");
    fixture.revision = locked.source_revision();

    let receipt = fixture
        .commit(&[
            SourceCurationUpdate::AddTag {
                path: CurationFixture::path("one.wav"),
                label: String::from(" One\t-\tShot "),
            },
            SourceCurationUpdate::AddTag {
                path: CurationFixture::path("two.wav"),
                label: String::from(" LOOP "),
            },
        ])
        .expect("tag curation");
    assert_eq!(receipt.source_revision(), fixture.revision + 1);
    assert_eq!(receipt.files().len(), 2);
    assert_eq!(receipt.files()[0].relative_path, one);
    assert_eq!(
        receipt.files()[0].playback_type,
        Some(SourcePlaybackType::OneShot)
    );
    assert!(receipt.files()[0].last_curated_at.is_some());
    assert_eq!(receipt.files()[1].relative_path, two);
    assert_eq!(
        receipt.files()[1].playback_type,
        Some(SourcePlaybackType::Loop)
    );
    assert_eq!(receipt.files()[1].rating, Rating::KEEP_3);
    assert!(receipt.files()[1].locked);
    assert!(receipt.files()[1].last_curated_at.is_some());

    let normalized_tags = fixture
        .database
        .tags_for_path(one)
        .expect("one tags")
        .into_iter()
        .map(|tag| tag.normalized_text)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        normalized_tags,
        BTreeSet::from([
            String::from("embedded\0tag"),
            String::from("one - shot"),
            String::from("unrelated"),
        ])
    );
    assert_eq!(
        fixture.database.tags_for_path(two).unwrap().len(),
        1,
        "the locked file receives its playback tag without inheriting another file's tags"
    );

    fixture.revision = receipt.source_revision();
    let removed = fixture
        .commit(&[SourceCurationUpdate::RemoveTag {
            path: CurationFixture::path("one.wav"),
            label: String::from(" ONE\t-\tSHOT "),
        }])
        .expect("remove normalized tag");
    assert_eq!(removed.files()[0].playback_type, None);
    assert_eq!(
        fixture
            .database
            .tags_for_path(one)
            .unwrap()
            .into_iter()
            .map(|tag| tag.normalized_text)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([String::from("embedded\0tag"), String::from("unrelated")])
    );
}

#[test]
fn curation_tag_labels_are_bounded_before_any_transaction_mutation() {
    let fixture = CurationFixture::new();
    let valid = SourceCurationUpdate::AddTag {
        path: CurationFixture::path("one.wav"),
        label: String::from("valid"),
    };
    let oversized = SourceCurationUpdate::AddTag {
        path: CurationFixture::path("one.wav"),
        label: "x".repeat(SOURCE_CURATION_MAX_TAG_LABEL_BYTES + 1),
    };
    assert!(matches!(
        fixture.commit(std::slice::from_ref(&oversized)),
        Err(SourceDbError::TagLabelTooLong { .. })
    ));
    assert!(
        fixture
            .database
            .tags_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );

    let nul = SourceCurationUpdate::RemoveTag {
        path: CurationFixture::path("one.wav"),
        label: String::from("bad\0tag"),
    };
    assert!(matches!(
        fixture.commit(&[valid.clone(), nul]),
        Err(SourceDbError::TagLabelContainsNul)
    ));
    assert!(
        fixture
            .database
            .tags_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );

    let whitespace = SourceCurationUpdate::AddTag {
        path: CurationFixture::path("one.wav"),
        label: String::from(" \t\n"),
    };
    assert!(matches!(
        fixture.commit(std::slice::from_ref(&whitespace)),
        Err(SourceDbError::EmptyTagLabel)
    ));
    assert!(
        fixture
            .database
            .tags_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
}

#[test]
fn curation_tag_timestamp_semantics_match_existing_tag_primitives() {
    let mut fixture = CurationFixture::new();
    let path = Path::new("one.wav");
    let mut seed = fixture.database.write_batch().expect("tag batch");
    seed.assign_tag_to_path(path, "Kick").expect("seed tag");
    seed.set_last_curated_at(path, 1).expect("seed timestamp");
    seed.commit_auxiliary_state().expect("seed tag timestamp");

    let duplicate = fixture
        .commit(&[SourceCurationUpdate::AddTag {
            path: CurationFixture::path("one.wav"),
            label: String::from(" kICK "),
        }])
        .expect("duplicate curation tag");
    assert!(duplicate.files()[0].last_curated_at.unwrap() > 1);
    fixture.revision = duplicate.source_revision();

    let mut reset = fixture.database.write_batch().expect("timestamp batch");
    reset.set_last_curated_at(path, 2).expect("reset timestamp");
    reset.commit_auxiliary_state().expect("reset timestamp");

    let absent = fixture
        .commit(&[SourceCurationUpdate::RemoveTag {
            path: CurationFixture::path("one.wav"),
            label: String::from("missing"),
        }])
        .expect("absent curation tag");
    assert_eq!(absent.files()[0].last_curated_at, Some(2));
    assert_eq!(
        fixture.database.last_curated_at_for_path(path).unwrap(),
        Some(2)
    );
}

#[test]
fn stale_revision_root_missing_path_and_invalid_directory_reject_atomically() {
    let fixture = CurationFixture::new();
    let update = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("one.wav"),
        rating: Rating::KEEP_1,
    }];
    let initial_row = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    let initial_continuity = continuity_state(&fixture.database);

    assert!(
        fixture
            .database
            .commit_curation_with_profile_guard(
                &fixture.profile_guard,
                fixture.revision - 1,
                &fixture.root_identity,
                &update,
            )
            .is_err()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        initial_row.tag
    );
    assert_eq!(continuity_state(&fixture.database), initial_continuity);

    let wrong_root = RootIdentity::from_bytes(b"other-root".to_vec());
    assert!(
        fixture
            .database
            .commit_curation_with_profile_guard(
                &fixture.profile_guard,
                fixture.revision,
                &wrong_root,
                &update,
            )
            .is_err()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);

    let missing_path = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("missing.wav"),
        rating: Rating::TRASH_1,
    }];
    assert!(fixture.commit(&missing_path).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(continuity_state(&fixture.database), initial_continuity);

    fixture
        .database
        .connection
        .execute(
            "INSERT INTO source_manifest_audit_seen (path) VALUES ('pending.wav')",
            [],
        )
        .expect("pending audit checkpoint");
    assert!(fixture.commit(&update).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    fixture
        .database
        .connection
        .execute(
            "DELETE FROM source_manifest_audit_seen WHERE path = 'pending.wav'",
            [],
        )
        .expect("clear pending audit checkpoint");

    fixture
        .database
        .connection
        .execute(
            "UPDATE source_directory_generations
             SET published_source_revision = ?1
             WHERE status = 'active'",
            [fixture.revision as i64 - 1],
        )
        .expect("corrupt directory generation");
    assert!(fixture.commit(&update).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        initial_row.tag
    );
}

#[test]
fn curation_admission_rejects_batches_above_bound_without_mutation() {
    let fixture = CurationFixture::new();
    let updates = (0..=SOURCE_CURATION_BATCH_LIMIT)
        .map(|_| SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_1,
        })
        .collect::<Vec<_>>();

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        Rating::NEUTRAL
    );
}

#[test]
fn curation_admission_rejects_invalid_deserialized_rating_without_mutation() {
    let fixture = CurationFixture::new();
    let invalid_rating: Rating = serde_json::from_str("127").expect("deserialized rating");
    let updates = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("one.wav"),
        rating: invalid_rating,
    }];

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        Rating::NEUTRAL
    );
}

#[test]
fn curation_admission_rejects_invalid_deserialized_collection_without_mutation() {
    let fixture = CurationFixture::new();
    let invalid_collection: SampleCollection =
        serde_json::from_str("255").expect("deserialized collection");
    let updates = [SourceCurationUpdate::AddCollection {
        path: CurationFixture::path("one.wav"),
        collection: invalid_collection,
    }];

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert!(
        fixture
            .database
            .collections_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn curation_admission_rejects_mixed_valid_and_invalid_values_atomically() {
    let fixture = CurationFixture::new();
    let invalid_collection: SampleCollection =
        serde_json::from_str("255").expect("deserialized collection");
    let updates = [
        SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        },
        SourceCurationUpdate::AddCollection {
            path: CurationFixture::path("one.wav"),
            collection: invalid_collection,
        },
    ];

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    let row = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    assert_eq!(row.tag, Rating::NEUTRAL);
    assert!(
        fixture
            .database
            .collections_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn curation_commit_helper_rejects_manifest_dirty_batches() {
    let fixture = CurationFixture::new();
    let mut batch = fixture.database.write_batch().expect("write batch");
    batch
        .upsert_file(Path::new("new.wav"), 30, 3)
        .expect("manifest mutation");

    assert!(batch.commit_curation_prepared().is_err());
    assert!(
        fixture
            .database
            .entry_for_path(Path::new("new.wav"))
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
}

#[cfg(unix)]
#[test]
fn profile_bound_curation_rejects_replaced_database_child_atomically() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_child = fixture.profile_child.as_ref().expect("profile-bound child");
    let child_path = fixture.profile_guard.profile_root().join(profile_child);
    let moved_path = fixture
        .profile_guard
        .profile_root()
        .join("curation-profile-bound-moved");
    std::fs::rename(&child_path, &moved_path).expect("orphan retained database child");
    std::fs::create_dir(&child_path).expect("replacement database child");
    std::fs::write(child_path.join("sentinel"), b"untouched").expect("replacement sentinel");

    let initial_row = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    let initial_continuity = continuity_state(&fixture.database);
    let update = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("one.wav"),
        rating: Rating::KEEP_1,
    }];

    assert!(fixture.commit(&update).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        initial_row.tag
    );
    assert_eq!(continuity_state(&fixture.database), initial_continuity);
    assert!(child_path.join("sentinel").is_file());
}

#[cfg(unix)]
#[test]
fn profile_mutation_guard_rejects_replaced_profile_root_before_batch() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_root = fixture.profile_guard.profile_root().to_path_buf();
    let displaced = profile_root.with_file_name("curation-profile-root-moved");
    std::fs::rename(&profile_root, &displaced).expect("orphan retained profile root");
    std::fs::create_dir(&profile_root).expect("replacement profile root");

    let result = fixture.database.write_batch();
    assert!(matches!(result, Err(SourceDbError::ProfileOwnership(_))));
    assert_eq!(revision_value(&fixture.database), fixture.revision as i64);
}

#[test]
fn profile_mutation_guard_enforces_begin_and_commit_on_all_platforms() {
    let mut fixture = CurationFixture::new_profile_bound();
    let valid = Arc::new(AtomicBool::new(true));
    fixture.database =
        fixture
            .database
            .retain_database_root_binding(Arc::new(ControllableDatabaseRootBinding {
                valid: Arc::clone(&valid),
            }));

    let before_revision = revision_value(&fixture.database);
    let before_tag = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap()
        .tag;
    valid.store(false, Ordering::SeqCst);
    let result = fixture.database.write_batch();
    assert!(matches!(result, Err(SourceDbError::ProfileOwnership(_))));
    assert_eq!(revision_value(&fixture.database), before_revision);

    valid.store(true, Ordering::SeqCst);
    let mut batch = fixture.database.write_batch().expect("profile-owned batch");
    batch
        .set_tag(Path::new("one.wav"), Rating::KEEP_1)
        .expect("metadata mutation");
    valid.store(false, Ordering::SeqCst);
    let result = batch.commit();
    assert!(matches!(result, Err(SourceDbError::ProfileOwnership(_))));
    assert_eq!(revision_value(&fixture.database), before_revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        before_tag
    );

    valid.store(true, Ordering::SeqCst);
    fixture
        .database
        .set_tag(Path::new("one.wav"), Rating::KEEP_1)
        .expect("valid profile-owned write");
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        Rating::KEEP_1
    );
}

#[cfg(unix)]
#[test]
fn profile_mutation_guard_rolls_back_batch_after_database_child_replacement() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_child = fixture.profile_child.as_ref().expect("profile-bound child");
    let child_path = fixture.profile_guard.profile_root().join(profile_child);
    let moved_path = fixture
        .profile_guard
        .profile_root()
        .join("curation-profile-mid-batch-moved");
    let before_revision = revision_value(&fixture.database);
    let before_tag = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap()
        .tag;

    let mut batch = fixture.database.write_batch().expect("profile-owned batch");
    batch
        .set_tag(Path::new("one.wav"), Rating::KEEP_1)
        .expect("metadata mutation");
    std::fs::rename(&child_path, &moved_path).expect("orphan retained database child");
    std::fs::create_dir(&child_path).expect("replacement database child");

    let result = batch.commit();
    assert!(matches!(
        result,
        Err(SourceDbError::SourceLease(
            SourceLeaseError::DatabaseRootReplaced { .. }
        ))
    ));
    assert_eq!(revision_value(&fixture.database), before_revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        before_tag
    );
}

#[cfg(unix)]
#[test]
fn profile_mutation_guard_rejects_direct_journal_insert_after_child_replacement() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_child = fixture.profile_child.as_ref().expect("profile-bound child");
    let child_path = fixture.profile_guard.profile_root().join(profile_child);
    let moved_path = fixture
        .profile_guard
        .profile_root()
        .join("curation-profile-journal-moved");
    std::fs::rename(&child_path, &moved_path).expect("orphan retained database child");
    std::fs::create_dir(&child_path).expect("replacement database child");

    let entry = FileOpJournalEntry::new_copy(
        String::from("profile-guard-journal"),
        CopyJournalEntryInit {
            target_relative: PathBuf::from("target.wav"),
            staged_relative: PathBuf::from(".wavecrate-staged-target.wav"),
            tag: Rating::KEEP_1,
            looped: false,
            locked: false,
            last_played_at: None,
            last_curated_at: None,
        },
    )
    .expect("journal entry");

    let result =
        crate::sample_sources::db::file_ops_journal::insert_entry(&fixture.database, &entry);
    assert!(matches!(
        result,
        Err(SourceDbError::SourceLease(
            SourceLeaseError::DatabaseRootReplaced { .. }
        ))
    ));
    assert!(
        crate::sample_sources::db::file_ops_journal::list_entries(&fixture.database)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn curation_tag_assignment_resolves_identity_without_decoding_legacy_display_text() {
    let fixture = CurationFixture::new();
    // A legacy row can contain non-text bytes despite the TEXT column affinity. The bounded
    // curation path only needs its numeric identity and must not decode this unused payload.
    let legacy_display = vec![0xff_u8; 1024 * 1024];
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES ('loop', ?1)",
            [&legacy_display],
        )
        .expect("legacy tag");
    let receipt = fixture
        .commit(&[SourceCurationUpdate::AddTag {
            path: CurationFixture::path("one.wav"),
            label: String::from("loop"),
        }])
        .expect("assign existing identity without decoding its display label");
    assert_eq!(
        receipt.files()[0].playback_type,
        Some(SourcePlaybackType::Loop)
    );
    let (stored_length, assignments): (usize, usize) = fixture
        .database
        .connection
        .query_row(
            "SELECT length(display_label),
            (SELECT count(*) FROM wav_file_tags WHERE tag_id = source_tags.id)
         FROM source_tags WHERE normalized_text = 'loop'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored legacy identity");
    assert_eq!(stored_length, legacy_display.len());
    assert_eq!(assignments, 1);
}

#[test]
fn curation_existing_tag_ids_preserve_legacy_and_unrelated_assignments() {
    let mut fixture = CurationFixture::new();
    let one = Path::new("one.wav");
    let two = Path::new("two.wav");
    let legacy_display = vec![0xff_u8; 1024 * 1024];
    let legacy_normalized = String::from("legacy\0tag");
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            rusqlite::params![&legacy_normalized, &legacy_display],
        )
        .expect("legacy tag");
    let legacy_id: i64 = fixture
        .database
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = ?1",
            [legacy_normalized.as_str()],
            |row| row.get(0),
        )
        .expect("legacy tag id");
    let loop_id = insert_tag(&fixture.database, "loop", "loop");
    let one_shot_id = insert_tag(&fixture.database, "oneshot", "one-shot");
    let unrelated_id = insert_tag(&fixture.database, "unrelated", "Unrelated");
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO wav_file_tags (path, tag_id) VALUES
                (?1, ?3), (?1, ?4), (?2, ?4)",
            rusqlite::params![
                one.to_string_lossy(),
                two.to_string_lossy(),
                loop_id,
                unrelated_id
            ],
        )
        .expect("seed assignments");
    let mut seed = fixture.database.write_batch().expect("timestamp batch");
    seed.set_last_curated_at(one, 1).expect("seed timestamp");
    seed.commit_auxiliary_state().expect("seed timestamp");

    let assigned = fixture
        .commit(&[
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: legacy_id,
            },
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: one_shot_id,
            },
        ])
        .expect("assign existing source-local tag");
    assert_eq!(
        assigned.files()[0].playback_type,
        Some(SourcePlaybackType::OneShot)
    );
    assert!(assigned.files()[0].last_curated_at.unwrap() > 1);
    let assigned_ids = tag_ids_for_path(&fixture.database, one);
    assert_eq!(
        assigned_ids,
        BTreeSet::from([legacy_id, one_shot_id, unrelated_id])
    );
    assert_eq!(
        tag_ids_for_path(&fixture.database, two),
        BTreeSet::from([unrelated_id])
    );
    let (legacy_type, legacy_length): (String, usize) = fixture
        .database
        .connection
        .query_row(
            "SELECT typeof(display_label), length(CAST(display_label AS BLOB))
             FROM source_tags WHERE id = ?1",
            [legacy_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("legacy display payload");
    assert_eq!(legacy_type, "blob");
    assert_eq!(legacy_length, legacy_display.len());

    fixture.revision = assigned.source_revision();
    let removed = fixture
        .commit(&[SourceCurationUpdate::RemoveExistingTag {
            path: CurationFixture::path("one.wav"),
            tag_id: legacy_id,
        }])
        .expect("remove existing legacy source-local tag");
    assert_eq!(
        removed.files()[0].playback_type,
        Some(SourcePlaybackType::OneShot)
    );
    assert_eq!(
        tag_ids_for_path(&fixture.database, one),
        BTreeSet::from([one_shot_id, unrelated_id])
    );
    assert_eq!(
        tag_ids_for_path(&fixture.database, two),
        BTreeSet::from([unrelated_id])
    );
    fixture.revision = removed.source_revision();
    let removed_playback = fixture
        .commit(&[SourceCurationUpdate::RemoveExistingTag {
            path: CurationFixture::path("one.wav"),
            tag_id: one_shot_id,
        }])
        .expect("remove existing playback source-local tag");
    assert_eq!(removed_playback.files()[0].playback_type, None);
    assert_eq!(
        tag_ids_for_path(&fixture.database, one),
        BTreeSet::from([unrelated_id])
    );
    assert_eq!(
        fixture
            .database
            .connection
            .query_row("SELECT count(*) FROM source_tags", [], |row| row
                .get::<_, usize>(0))
            .expect("tag dictionary count"),
        4
    );
}

#[test]
fn curation_existing_tag_ids_reject_invalid_missing_and_stale_without_writes() {
    let fixture = CurationFixture::new();
    let path = Path::new("one.wav");
    let tag_id = insert_tag(&fixture.database, "keep", "Keep");
    let update = |tag_id| SourceCurationUpdate::AssignExistingTag {
        path: CurationFixture::path("one.wav"),
        tag_id,
    };

    for tag_id in [0, -1] {
        assert!(matches!(
            fixture.commit(&[update(tag_id)]),
            Err(SourceDbError::InvalidTagId { tag_id: actual }) if actual == tag_id
        ));
    }
    assert!(matches!(
        fixture.commit(&[update(tag_id + 1)]),
        Err(SourceDbError::TagIdNotFound { tag_id: actual }) if actual == tag_id + 1
    ));

    let initial_revision = fixture.database.get_revision().expect("revision");
    let initial_ids = tag_ids_for_path(&fixture.database, path);
    assert!(matches!(
        fixture.database.commit_curation_with_profile_guard(
            &fixture.profile_guard,
            fixture.revision - 1,
            &fixture.root_identity,
            &[update(tag_id)],
        ),
        Err(SourceDbError::DirectoryTruth(
            crate::sample_sources::db::SourceDirectoryTruthError::StaleRevision { .. }
        ))
    ));
    assert_eq!(
        fixture.database.get_revision().expect("revision"),
        initial_revision
    );
    assert_eq!(tag_ids_for_path(&fixture.database, path), initial_ids);

    let mut batch = fixture.database.write_batch().expect("invalid id batch");
    assert!(matches!(
        batch.assign_existing_tag_for_curation(path, 0),
        Err(SourceDbError::InvalidTagId { tag_id: 0 })
    ));
    drop(batch);
}

#[test]
fn source_writer_replays_durable_receipts_and_reopens_as_observation_only() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-test");
    let mut writer = SourceWriter::new(fixture.database, source_id.clone(), u64::MAX)
        .expect("source writer authority");
    let initial = writer.current_revision().expect("initial revision");
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(11)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(12)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(13)),
        initial.clone(),
        vec![SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        }],
    )
    .expect("request");
    let stale_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(41)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(42)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(43)),
        initial.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("stale request");
    let conflicting_request = SourceWriterRequest::new(
        request.identity().operation_id(),
        request.identity().participant_id(),
        request.identity().command_id(),
        initial.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("conflicting request");
    let first = writer
        .commit_curation(request.clone())
        .expect("first source command");
    assert_eq!(first.source_revision().revision(), initial.revision() + 1);
    let replay = writer
        .commit_curation(request.clone())
        .expect("exact duplicate replay");
    assert_eq!(first, replay);
    let state_after_first = source_writer_state_snapshot(writer.database_for_tests());
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        first.source_revision().revision()
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        state_after_first
    );
    let conflicting_before = source_writer_state_snapshot(writer.database_for_tests());
    assert!(matches!(
        writer.commit_curation(conflicting_request),
        Err(SourceWriterError::ConflictingCommand { .. })
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        conflicting_before
    );
    let stale_before = source_writer_state_snapshot(writer.database_for_tests());
    assert!(matches!(
        writer.commit_curation(stale_request),
        Err(SourceWriterError::StaleRevision {
            expected: 3,
            actual: 4
        })
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        stale_before
    );

    writer.inject_fault_for_tests("before_commit");
    let rollback_expected = writer.current_revision().unwrap();
    let rollback_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(21)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(22)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(23)),
        rollback_expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("rollback request");
    let rollback_before = source_writer_state_snapshot(writer.database_for_tests());
    assert!(matches!(
        writer.commit_curation(rollback_request.clone()),
        Err(SourceWriterError::Database(SourceDbError::Canceled))
    ));
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        rollback_expected.revision()
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        rollback_before
    );
    let rollback_receipt = writer
        .commit_curation(rollback_request)
        .expect("retry after confirmed rollback");
    assert_eq!(
        rollback_receipt.source_revision().revision(),
        rollback_expected.revision() + 1
    );

    let rollback_uncertain_expected = writer.current_revision().unwrap();
    let rollback_uncertain_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(24)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(25)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(26)),
        rollback_uncertain_expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: false,
        }],
    )
    .expect("rollback uncertainty request");
    let rollback_uncertain_before = source_writer_state_snapshot(writer.database_for_tests());
    writer.inject_fault_for_tests("rollback_uncertain");
    assert!(matches!(
        writer.commit_curation(rollback_uncertain_request.clone()),
        Err(SourceWriterError::Uncertain { .. })
    ));
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        rollback_uncertain_expected.revision()
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        rollback_uncertain_before
    );
    let rollback_uncertain_replay = writer
        .commit_curation(rollback_uncertain_request)
        .expect("retry after rollback uncertainty");
    assert_eq!(
        rollback_uncertain_replay.source_revision().revision(),
        rollback_uncertain_expected.revision() + 1
    );

    writer.inject_fault_for_tests("after_commit");
    let uncertain_expected = writer.current_revision().unwrap();
    let uncertain_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(31)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(32)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(33)),
        uncertain_expected,
        vec![SourceCurationUpdate::SetLastPlayedAt {
            path: CurationFixture::path("one.wav"),
            played_at: Some(9),
        }],
    )
    .expect("uncertain request");
    let uncertain_result = writer.commit_curation(uncertain_request.clone());
    match uncertain_result {
        Err(SourceWriterError::Uncertain {
            identity,
            canonical_request,
        }) => {
            assert_eq!(identity, uncertain_request.identity());
            assert_eq!(canonical_request, uncertain_request.canonical_bytes());
        }
        other => panic!("expected uncertain source writer result, got {other:?}"),
    }
    let uncertain_replay = writer
        .commit_curation(uncertain_request.clone())
        .expect("exact retry after uncertain acknowledgement");
    assert_eq!(uncertain_replay.identity(), uncertain_request.identity());
    assert_eq!(
        uncertain_replay.curation().files()[0].relative_path,
        PathBuf::from("one.wav")
    );
    assert_eq!(
        uncertain_replay.curation().files()[0].last_played_at,
        Some(9)
    );

    writer.close().expect("close source writer");
    let reopened_database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen source database");
    assert_eq!(
        reopened_database
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        4
    );
    let mut reopened = SourceWriter::new(reopened_database, source_id, u64::MAX - 1)
        .expect("reopened source writer");
    let historical = reopened
        .read_receipt_observation(request.identity())
        .expect("read historical source receipt")
        .expect("historical source receipt exists");
    assert_eq!(historical.identity(), request.identity());
    assert_eq!(
        historical.source_revision().lifecycle_generation(),
        u64::MAX
    );
    assert_eq!(
        historical.source_revision().revision(),
        first.source_revision().revision()
    );
    assert_eq!(
        historical.source_revision().root_identity(),
        first.source_revision().root_identity()
    );
    assert_eq!(
        historical.source_revision().root_binding_identity(),
        first.source_revision().root_binding_identity()
    );
    assert_eq!(
        historical.source_revision().database_identity(),
        first.source_revision().database_identity()
    );
    assert_eq!(
        historical.source_revision().lease_epoch(),
        first.source_revision().lease_epoch()
    );
    assert!(matches!(
        reopened.commit_curation(request.clone()),
        Err(SourceWriterError::SupersededLifecycle { .. })
    ));
    let stale_generation_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(51)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(52)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(53)),
        first.source_revision().clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: false,
        }],
    )
    .expect("stale generation request");
    assert!(matches!(
        reopened.commit_curation(stale_generation_request),
        Err(SourceWriterError::StaleFence)
    ));
    reopened.close().expect("close reopened source writer");
}

#[test]
fn source_writer_replays_at_full_retained_history_limit() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-history-limit");
    let mut writer =
        SourceWriter::new(fixture.database, source_id.clone(), 1).expect("source writer authority");
    let first_expected = writer.current_revision().expect("initial revision");
    let first_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(1_000)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(2_000)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(3_000)),
        first_expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("first history request");
    let first = writer
        .commit_curation(first_request.clone())
        .expect("first history commit");

    for index in 1..256usize {
        let expected = writer.current_revision().expect("history revision");
        let request = SourceWriterRequest::new(
            crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(
                1_000 + index as u128,
            )),
            crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(
                2_000 + index as u128,
            )),
            SourceCommandId::from_uuid(uuid::Uuid::from_u128(3_000 + index as u128)),
            expected.clone(),
            vec![SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: index % 2 == 0,
            }],
        )
        .expect("history request");
        let receipt = writer.commit_curation(request).expect("history commit");
        assert_eq!(
            receipt.source_revision().revision(),
            expected.revision() + 1
        );
    }

    let at_capacity = writer.current_revision().expect("full history revision");
    let at_capacity_state = source_writer_state_snapshot(writer.database_for_tests());
    let over_limit_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(9_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(9_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(9_003)),
        at_capacity.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("over-limit history request");
    assert!(matches!(
        writer.commit_curation(over_limit_request),
        Err(SourceWriterError::CapacityExceeded)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        at_capacity_state
    );
    let replay = writer
        .commit_curation(first_request)
        .expect("replay at retained history limit");
    assert_eq!(replay, first);
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        at_capacity.revision()
    );
    writer.close().expect("close history writer");

    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen history source database");
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        256
    );
}

#[test]
fn source_writer_rolls_back_oversized_receipt_after_curation_mutation() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-oversized-receipt");
    let mut writer =
        SourceWriter::new(fixture.database, source_id, 1).expect("source writer authority");
    let expected = writer.current_revision().expect("initial revision");
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(5_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(5_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(5_003)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("oversized receipt request");

    writer.inject_fault_for_tests("oversized_receipt");
    assert!(matches!(
        writer.commit_curation(request.clone()),
        Err(SourceWriterError::CapacityExceeded)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );

    let receipt = writer
        .commit_curation(request)
        .expect("retry after oversized receipt rollback");
    assert_eq!(
        receipt.source_revision().revision(),
        expected.revision() + 1
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()).receipt_count,
        before.receipt_count + 1
    );
}

#[test]
fn source_writer_rejects_combined_receipt_budget_without_state_change() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-receipt-budget");
    let mut writer =
        SourceWriter::new(fixture.database, source_id, 1).expect("source writer authority");

    for index in 0..15u128 {
        let expected = writer.current_revision().expect("budget revision");
        let request = SourceWriterRequest::new(
            crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(10_000 + index)),
            crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(
                20_000 + index,
            )),
            SourceCommandId::from_uuid(uuid::Uuid::from_u128(30_000 + index)),
            expected,
            vec![SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: index % 2 == 0,
            }],
        )
        .expect("budget request");
        writer.inject_fault_for_tests("receipt_at_record_limit");
        writer
            .commit_curation(request)
            .expect("padded receipt within combined budget");
    }

    let expected = writer.current_revision().expect("budget boundary revision");
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(40_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(40_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(40_003)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("budget boundary request");
    writer.inject_fault_for_tests("receipt_at_record_limit");
    assert!(matches!(
        writer.commit_curation(request),
        Err(SourceWriterError::CapacityExceeded)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
}

#[test]
fn source_writer_rejects_oversized_request_before_transaction_mutation() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-oversized");
    let writer =
        SourceWriter::new(fixture.database, source_id, 1).expect("source writer authority");
    let expected = writer.current_revision().expect("initial revision");
    let long_path = CurationFixture::path(&format!("{}.wav", "x".repeat(10_000)));
    let updates = (0..SOURCE_CURATION_BATCH_LIMIT)
        .map(|_| SourceCurationUpdate::SetLocked {
            path: long_path.clone(),
            locked: true,
        })
        .collect();
    let oversized = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(4_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(4_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(4_003)),
        expected.clone(),
        updates,
    );
    assert!(matches!(
        oversized,
        Err(SourceWriterError::InvalidRequest(_))
    ));
    assert_eq!(writer.current_revision().unwrap(), expected);
    writer.close().expect("close oversized writer");
    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen oversized source database");
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn source_writer_rejects_revision_beyond_sqlite_integer_before_mutation() {
    let fixture = CurationFixture::new_profile_bound();
    let max_revision = i64::MAX.to_string();
    fixture
        .database
        .connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = 'revision'",
            [&max_revision],
        )
        .expect("set max source revision");
    fixture
        .database
        .connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = ?2",
            rusqlite::params![
                &max_revision,
                crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_REVISION
            ],
        )
        .expect("set max audit revision");
    fixture
        .database
        .connection
        .execute(
            "UPDATE source_directory_generations
             SET published_source_revision = ?1
             WHERE status = 'active'",
            [i64::MAX],
        )
        .expect("set max directory revision");

    let mut writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("source-writer-max-revision"),
        1,
    )
    .expect("source writer authority");
    let expected = writer.current_revision().expect("max revision");
    assert_eq!(expected.revision(), i64::MAX as u64);
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(5_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(5_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(5_003)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("max revision request");
    assert!(matches!(
        writer.commit_curation(request),
        Err(SourceWriterError::InvalidRequest(_))
    ));
    assert_eq!(writer.current_revision().unwrap(), expected);
    writer.close().expect("close max revision writer");
}

#[test]
fn source_writer_rejects_audit_root_mismatch_before_issuing_a_revision() {
    let fixture = CurationFixture::new_profile_bound();
    fixture
        .database
        .connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = ?2",
            rusqlite::params![
                "[99]",
                crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY
            ],
        )
        .expect("tamper audit root");
    assert!(matches!(
        SourceWriter::new(
            fixture.database,
            SourceId::from_string("source-writer-root-mismatch"),
            1,
        ),
        Err(SourceWriterError::StaleFence)
    ));
}

#[test]
fn source_writer_rejects_tampered_receipt_lengths_before_loading_payloads() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-malformed");
    let mut writer =
        SourceWriter::new(fixture.database, source_id.clone(), 1).expect("source writer authority");
    let expected = writer.current_revision().expect("revision");
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(61)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(62)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(63)),
        expected,
        vec![SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_1,
        }],
    )
    .expect("request");
    writer.commit_curation(request.clone()).expect("commit");
    writer.close().expect("close source writer");

    let reopened_database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen source database");
    reopened_database
        .connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .expect("allow malformed fixture row");
    reopened_database
        .connection
        .execute(
            "UPDATE source_participant_curation_receipts
             SET request_len = request_len + 1",
            [],
        )
        .expect("tamper request length");
    let mut reopened =
        SourceWriter::new(reopened_database, source_id, 1).expect("reopen source writer");
    assert!(matches!(
        reopened.commit_curation(request),
        Err(SourceWriterError::MalformedReceipt)
    ));
    let expected = reopened.current_revision().expect("revision after tamper");
    let new_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(64)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(65)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(66)),
        expected,
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("new request after tamper");
    assert!(matches!(
        reopened.commit_curation(new_request),
        Err(SourceWriterError::MalformedReceipt)
    ));
    reopened.close().expect("close reopened source writer");
}

fn insert_tag(database: &SourceDatabase, normalized_text: &str, display_label: &str) -> i64 {
    database
        .connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            rusqlite::params![normalized_text, display_label],
        )
        .expect("tag");
    database
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = ?1",
            [normalized_text],
            |row| row.get(0),
        )
        .expect("tag id")
}

fn tag_ids_for_path(database: &SourceDatabase, path: &Path) -> BTreeSet<i64> {
    let raw_path = path.to_string_lossy();
    let mut statement = database
        .connection
        .prepare("SELECT tag_id FROM wav_file_tags WHERE path = ?1 ORDER BY tag_id")
        .expect("tag ids");
    statement
        .query_map([raw_path.as_ref()], |row| row.get(0))
        .expect("tag ids query")
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("tag ids rows")
}
