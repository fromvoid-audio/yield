//! These regressions intentionally use the production finalizer, not the test substitute.

use super::*;

struct PreparedMoveRecovery {
    fixture: MoveRecoveryFixture,
    journal_before_retry: FileOpJournalEntry,
    staged_path: PathBuf,
    staged_bytes: Vec<u8>,
    staged_identity: String,
}

fn prepare_staged_move(target_already_claimed: bool) -> PreparedMoveRecovery {
    let fixture = MoveRecoveryFixture::new();
    std::fs::rename(fixture.source_absolute(), fixture.staged_absolute()).unwrap();
    if target_already_claimed {
        // Simulate an already-published object with the same retained staged identity.
        std::fs::hard_link(fixture.staged_absolute(), fixture.target_absolute()).unwrap();
    }
    let (size, modified) = file_identity(&fixture.staged_absolute());
    update_stage(
        &fixture.target_db,
        &fixture.entry.id,
        FileOpStage::Staged,
        Some(size),
        Some(modified),
    )
    .unwrap();
    let journal_before_retry = list_entries(&fixture.target_db)
        .unwrap()
        .entries
        .into_iter()
        .next()
        .unwrap();
    let staged_path = fixture.staged_absolute();
    let staged_bytes = std::fs::read(&staged_path).unwrap();
    let staged_identity = crate::filesystem_identity::stable_filesystem_identity_from_open_file(
        &std::fs::File::open(&staged_path).unwrap(),
    )
    .unwrap();

    PreparedMoveRecovery {
        fixture,
        journal_before_retry,
        staged_path,
        staged_bytes,
        staged_identity,
    }
}

fn assert_repeated_production_retry(
    prepared: &PreparedMoveRecovery,
    expected_error: &str,
    assert_target: impl Fn(&MoveRecoveryFixture),
) {
    for _ in 0..2 {
        let summary = super::super::reconcile_pending_ops(&prepared.fixture.target_db).unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.errors.len(), 1);
        assert!(
            summary.errors[0].contains(expected_error),
            "{:?}",
            summary.errors
        );

        assert_eq!(
            std::fs::read(&prepared.staged_path).unwrap(),
            prepared.staged_bytes
        );
        assert_eq!(
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(
                &std::fs::File::open(&prepared.staged_path).unwrap(),
            )
            .unwrap(),
            prepared.staged_identity
        );
        assert_target(&prepared.fixture);

        // A failed replay must not advance to source-side catalogue cleanup.
        let source = prepared
            .fixture
            .source_db
            .entry_for_path(&prepared.fixture.source_relative)
            .unwrap()
            .unwrap();
        assert_eq!(source.tag, Rating::KEEP_1);
        assert!(source.locked);

        let listed = list_entries(&prepared.fixture.target_db).unwrap();
        assert!(listed.malformed.is_empty());
        assert_eq!(listed.entries.len(), 1);
        let retained = &listed.entries[0];
        assert_eq!(retained.id, prepared.journal_before_retry.id);
        assert_eq!(retained.stage, prepared.journal_before_retry.stage);
        assert_eq!(retained.file_size, prepared.journal_before_retry.file_size);
        assert_eq!(
            retained.modified_ns,
            prepared.journal_before_retry.modified_ns
        );
        assert_eq!(
            retained.file_identity,
            prepared.journal_before_retry.file_identity
        );
        assert_eq!(
            retained.target_root_identity,
            prepared.journal_before_retry.target_root_identity
        );
        assert_eq!(
            retained.source_root_identity,
            prepared.journal_before_retry.source_root_identity
        );
        assert_eq!(
            retained.staged_relative,
            prepared.journal_before_retry.staged_relative
        );
    }
}

#[test]
fn production_publication_failure_retains_staging_and_journal_across_retries() {
    let prepared = prepare_staged_move(false);
    assert_repeated_production_retry(
        &prepared,
        "descriptor-bound staged finalization is unsupported",
        |fixture| {
            assert!(!fixture.target_absolute().exists());
            assert!(
                fixture
                    .target_db
                    .entry_for_path(&fixture.target_relative)
                    .unwrap()
                    .is_none()
            );
        },
    );
}

#[test]
fn production_cleanup_failure_retains_both_paths_and_source_metadata_across_retries() {
    let prepared = prepare_staged_move(true);
    let staged_bytes = prepared.staged_bytes.clone();
    assert_repeated_production_retry(
        &prepared,
        "descriptor-bound staged cleanup is unsupported",
        |fixture| {
            assert!(fixture.target_absolute().exists());
            assert_eq!(
                std::fs::read(fixture.target_absolute()).unwrap(),
                staged_bytes
            );
            assert_target_metadata_preserved(fixture);
        },
    );
}

#[test]
fn production_replay_retains_evidence_when_target_root_is_unavailable() {
    let prepared = prepare_staged_move(false);
    let unavailable_root = prepared.fixture.target_root.with_file_name("target-away");
    let staged_path = unavailable_root.join(&prepared.fixture.staged_relative);
    std::fs::rename(&prepared.fixture.target_root, &unavailable_root).unwrap();

    let prepared = PreparedMoveRecovery {
        staged_path,
        ..prepared
    };
    assert_repeated_production_retry(&prepared, "failed to open recovery root", |fixture| {
        assert!(!fixture.target_root.exists());
        assert!(!fixture.target_absolute().exists());
    });
}

#[test]
fn production_replay_retains_evidence_when_target_root_is_replaced() {
    let prepared = prepare_staged_move(false);
    let replaced_root = prepared
        .fixture
        .target_root
        .with_file_name("target-replaced");
    let staged_path = replaced_root.join(&prepared.fixture.staged_relative);
    std::fs::rename(&prepared.fixture.target_root, &replaced_root).unwrap();
    std::fs::create_dir_all(&prepared.fixture.target_root).unwrap();

    let prepared = PreparedMoveRecovery {
        staged_path,
        ..prepared
    };
    assert_repeated_production_retry(&prepared, "root identity changed", |fixture| {
        assert!(fixture.target_root.exists());
        assert!(!fixture.target_absolute().exists());
    });
}

#[test]
fn production_replay_retains_evidence_when_source_root_is_unavailable() {
    let prepared = prepare_staged_move(false);
    let unavailable_root = prepared.fixture.source_root.with_file_name("source-away");
    std::fs::rename(&prepared.fixture.source_root, &unavailable_root).unwrap();

    assert_repeated_production_retry(&prepared, "failed to open recovery root", |fixture| {
        assert!(fixture.target_root.exists());
        assert!(fixture.staged_absolute().exists());
        assert!(!fixture.target_absolute().exists());
    });
}

#[test]
fn production_replay_retains_evidence_when_source_root_is_replaced() {
    let prepared = prepare_staged_move(false);
    let replaced_root = prepared
        .fixture
        .source_root
        .with_file_name("source-replaced");
    std::fs::rename(&prepared.fixture.source_root, &replaced_root).unwrap();
    std::fs::create_dir_all(&prepared.fixture.source_root).unwrap();

    assert_repeated_production_retry(&prepared, "root identity changed", |fixture| {
        assert!(fixture.source_root.exists());
        assert!(fixture.target_root.exists());
        assert!(fixture.staged_absolute().exists());
        assert!(!fixture.target_absolute().exists());
    });
}

#[test]
fn production_replay_retains_staging_after_a_late_unrelated_target_collision() {
    let prepared = prepare_staged_move(false);
    let target_bytes = vec![7u8; 16];
    std::fs::write(prepared.fixture.target_absolute(), &target_bytes).unwrap();
    let (target_size, target_modified) = file_identity(&prepared.fixture.target_absolute());
    let target_identity = crate::filesystem_identity::stable_filesystem_identity_from_open_file(
        &std::fs::File::open(prepared.fixture.target_absolute()).unwrap(),
    )
    .unwrap();
    let mut batch = prepared.fixture.target_db.write_batch().unwrap();
    batch
        .upsert_file_with_hash_and_tag(
            &prepared.fixture.target_relative,
            target_size,
            target_modified,
            "unrelated-target",
            Rating::TRASH_3,
            false,
        )
        .unwrap();
    batch.commit().unwrap();
    prepared
        .fixture
        .target_db
        .set_locked(&prepared.fixture.target_relative, false)
        .unwrap();
    prepared
        .fixture
        .target_db
        .set_last_played_at(&prepared.fixture.target_relative, 77)
        .unwrap();

    assert_repeated_production_retry(
        &prepared,
        "target path was reused before recovery replay",
        |fixture| {
            assert_eq!(
                std::fs::read(fixture.target_absolute()).unwrap(),
                target_bytes
            );
            assert_eq!(
                crate::filesystem_identity::stable_filesystem_identity_from_open_file(
                    &std::fs::File::open(fixture.target_absolute()).unwrap(),
                )
                .unwrap(),
                target_identity
            );
            assert_eq!(
                fixture
                    .target_db
                    .tag_for_path(&fixture.target_relative)
                    .unwrap(),
                Some(Rating::TRASH_3)
            );
            assert_eq!(
                fixture
                    .target_db
                    .last_played_at_for_path(&fixture.target_relative)
                    .unwrap(),
                Some(77)
            );
        },
    );
}
