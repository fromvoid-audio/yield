//! These regressions intentionally use the production finalizer, not the test substitute.

use super::*;

fn assert_unsupported_recovery_preserves_evidence(target_already_claimed: bool) {
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
    let before = list_entries(&fixture.target_db).unwrap().entries.remove(0);
    let staged_bytes = std::fs::read(fixture.staged_absolute()).unwrap();
    let staged_identity = crate::filesystem_identity::stable_filesystem_identity_from_open_file(
        &std::fs::File::open(fixture.staged_absolute()).unwrap(),
    )
    .unwrap();

    for _ in 0..2 {
        let summary = super::super::reconcile_pending_ops(&fixture.target_db).unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.errors.len(), 1);
        let expected = if target_already_claimed {
            "descriptor-bound staged cleanup is unsupported"
        } else {
            "descriptor-bound staged finalization is unsupported"
        };
        assert!(summary.errors[0].contains(expected), "{:?}", summary.errors);
        assert_eq!(
            std::fs::read(fixture.staged_absolute()).unwrap(),
            staged_bytes
        );
        assert_eq!(
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(
                &std::fs::File::open(fixture.staged_absolute()).unwrap(),
            )
            .unwrap(),
            staged_identity
        );
        assert_eq!(fixture.target_absolute().exists(), target_already_claimed);
        if target_already_claimed {
            assert_eq!(
                std::fs::read(fixture.target_absolute()).unwrap(),
                staged_bytes
            );
            assert_target_metadata_preserved(&fixture);
        } else {
            assert!(
                fixture
                    .target_db
                    .entry_for_path(&fixture.target_relative)
                    .unwrap()
                    .is_none()
            );
        }
        // Cleanup failure must not advance to removal of the source-side catalogue row.
        let source = fixture
            .source_db
            .entry_for_path(&fixture.source_relative)
            .unwrap()
            .unwrap();
        assert_eq!(source.tag, Rating::KEEP_1);
        assert!(source.locked);
        let listed = list_entries(&fixture.target_db).unwrap();
        assert!(listed.malformed.is_empty());
        assert_eq!(listed.entries.len(), 1);
        let retained = &listed.entries[0];
        assert_eq!(retained.id, before.id);
        assert_eq!(retained.stage, before.stage);
        assert_eq!(retained.file_identity, before.file_identity);
        assert_eq!(retained.staged_relative, before.staged_relative);
    }
}

#[test]
fn production_publication_failure_retains_staging_and_journal_across_retries() {
    assert_unsupported_recovery_preserves_evidence(false);
}

#[test]
fn production_cleanup_failure_retains_both_paths_and_source_metadata_across_retries() {
    assert_unsupported_recovery_preserves_evidence(true);
}
