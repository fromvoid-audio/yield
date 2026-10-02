//! Actual SQLite boundary tests; synthetic directory labels are retained contract observations,
//! not production filesystem or native qualification (scanner/worker tests hold actual objects).
use super::*;

fn directory(path: &str, identity: &str) -> SourceDirectoryEntry {
    SourceDirectoryEntry {
        relative_path: PathBuf::from(path),
        directory_identity: identity.to_owned(),
    }
}
fn source() -> (tempfile::TempDir, SourceDatabase) {
    let root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_source_write(root.path()).unwrap();
    db.begin_source_directory_truth_generation(1, 3).unwrap();
    db.stage_source_directory_truth_entries(
        1,
        &[
            directory("known", "dir1"),
            directory("known/old", "dir2"),
            directory("other", "dir3"),
        ],
    )
    .unwrap();
    db.finalize_source_directory_truth_generation(1, 0).unwrap();
    (root, db)
}
fn facts(db: &SourceDatabase) -> SubtreeScanDatabaseFacts {
    db.subtree_scan_database_facts_at_revision(
        &[RootRelativePath::try_from_path(PathBuf::from("known")).unwrap()],
        db.get_revision().unwrap(),
    )
    .unwrap()
}

#[test]
fn owner_bound_directory_observation_rejects_foreign_source_stale_revision_and_overlap() {
    let (_a, a) = source();
    let (_b, b) = source();
    let observed = facts(&a);
    let mut foreign = b.write_batch().unwrap();
    assert!(
        foreign
            .apply_prepared_subtree_directory_truth(&observed, &observed.directories)
            .is_err()
    );
    drop(foreign);
    a.set_metadata("unrelated", "advance").unwrap();
    let mut stale = a.write_batch().unwrap();
    assert!(
        stale
            .apply_prepared_subtree_directory_truth(&observed, &observed.directories)
            .is_err()
    );
    drop(stale);
    let scopes = [
        RootRelativePath::try_from_path(PathBuf::from("known")).unwrap(),
        RootRelativePath::try_from_path(PathBuf::from("known/old")).unwrap(),
    ];
    assert!(
        a.subtree_scan_database_facts_at_revision(&scopes, a.get_revision().unwrap())
            .is_err()
    );
}

#[test]
fn directory_delta_cannot_commit_auxiliary_or_full_audit_and_failures_rollback_its_savepoint() {
    let (_root, db) = source();
    let observed = facts(&db);
    let before = db.get_revision().unwrap();
    let old = db.source_directory_truth_page(None, 64).unwrap().entries;
    let new = [directory("known", "dir1"), directory("known/new", "dir4")];
    let mut batch = db.write_batch().unwrap();
    batch
        .apply_prepared_subtree_directory_truth(&observed, &new)
        .unwrap();
    assert!(batch.commit_auxiliary_state().is_err());
    assert_eq!(db.get_revision().unwrap(), before);
    assert_eq!(
        db.source_directory_truth_page(None, 64).unwrap().entries,
        old
    );
    let mut batch = db.write_batch().unwrap();
    batch
        .apply_prepared_subtree_directory_truth(&observed, &new)
        .unwrap();
    batch.complete_manifest_audit(2).unwrap();
    assert!(
        batch
            .commit_with_manifest_snapshot_and_audit(
                crate::sample_sources::reconciliation::RootIdentity::from_bytes(
                    b"synthetic-root".to_vec()
                ),
                None
            )
            .is_err()
    );
    assert_eq!(db.get_revision().unwrap(), before);
    let mut batch = db.write_batch().unwrap();
    // The known subtree cannot steal the stable identity retained by another subtree.
    assert!(
        batch
            .apply_prepared_subtree_directory_truth(&observed, &[directory("known", "dir3")])
            .is_err()
    );
    batch
        .set_metadata("after_rejected_directory_delta", "safe")
        .unwrap();
    batch.commit_auxiliary_state().unwrap();
    assert_eq!(
        db.source_directory_truth_page(None, 64).unwrap().entries,
        old
    );
}

#[test]
fn stored_oversized_locators_invalid_policy_and_retained_count_never_grant_absence() {
    for failure in 0..3 {
        let (_root, db) = source();
        match failure {
            0 => {
                db.connection
                    .execute(
                        "INSERT INTO wav_files(path,file_size,modified_ns,tag) VALUES(?1,1,1,0)",
                        [format!("known/{}", "x".repeat(10000))],
                    )
                    .unwrap();
            }
            1 => {
                db.connection
                    .execute(
                        "UPDATE metadata SET value=?1 WHERE key='source_traversal_policy_v1'",
                        ["unknown"],
                    )
                    .unwrap();
                db.connection.execute("INSERT OR REPLACE INTO metadata(key,value) VALUES('source_traversal_policy_v1','unknown')",[]).unwrap();
            }
            _ => {
                for i in 0..129 {
                    db.connection.execute("INSERT INTO wav_files(path,file_size,modified_ns,tag) VALUES(?1,1,1,0)",[format!("known/{i}.wav")]).unwrap();
                }
            }
        }
        assert!(
            db.subtree_scan_database_facts_at_revision(
                &[RootRelativePath::try_from_path(PathBuf::from("known")).unwrap()],
                db.get_revision().unwrap()
            )
            .is_err()
        );
    }
}
