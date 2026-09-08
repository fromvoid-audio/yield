use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::db::SourceDirectoryTruthState;
use crate::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use crate::sample_sources::{
    Rating, SOURCE_CURATION_BATCH_LIMIT, SampleCollection, SourceCurationUpdate, SourceDatabase,
};

struct CurationFixture {
    _runtime: crate::test_runtime::TestRuntimeGuard,
    _config_base_dir: TempDir,
    _config_base: ConfigBaseGuard,
    _profile: PersistenceProfileGuard,
    _source: TempDir,
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

    fn new_with_profile_child(profile_child: Option<PathBuf>) -> Self {
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
        database
            .begin_source_directory_truth_generation(1, 0)
            .expect("directory generation");
        let root_identity = RootIdentity::from_bytes(b"curation-root".to_vec());
        database
            .write_batch()
            .expect("audit batch")
            .commit_manifest_audit_with_staged_directory_truth(
                1,
                2,
                10,
                root_identity.clone(),
                None,
            )
            .expect("authoritative audit");
        let revision = database.get_revision().expect("audit revision");
        assert_eq!(revision, 3);

        Self {
            _runtime: runtime,
            _config_base_dir: config_base_dir,
            _config_base: config_base,
            _profile: profile,
            _source: source,
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

fn metadata_number(database: &SourceDatabase, key: &str) -> u64 {
    database
        .get_metadata(key)
        .expect("metadata")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
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
