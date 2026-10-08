//! Actual catalog worker and owner transaction; producer injection is not FSEvents qualification.
use super::*;
use yield_library::sample_sources::db::SourceDirectoryTruthState;

fn fixture() -> Fixture {
    let f = Fixture::new(true);
    fs::create_dir_all(f.base.root.path().join("known/nested")).unwrap();
    fs::create_dir(f.base.root.path().join("other")).unwrap();
    f.put("known/old.wav", b"old audio");
    f.put("known/old.txt", b"old unsupported");
    f.put("known/nested/child.wav", b"nested audio");
    f.put("other/keep.wav", b"unchanged");
    f
}
fn subtree_batch(f: &Fixture, after: u64, last: u64, path: &Path) -> ReplayObservationBatch {
    let ingress = f.ingress(after);
    ingress.record_with_kind(
        Some(last),
        &f.base.root.path().join(path),
        ReplayEventCondition::Changed,
        ReplayPathKind::Directory,
    );
    ingress.mark_history_complete();
    ingress
        .into_candidate()
        .into_observation_batch(7, WatcherGeneration::new(7))
        .unwrap()
}

#[test]
fn retained_worker_subtree_atomic_delta_truth_checkpoint_duplicate_and_restart() {
    let f = fixture();
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    let prior_audit = f.read().last_authoritative_audit_metadata().unwrap();
    let old_dirs = f.read().source_directory_truth_page(None, 1).unwrap();
    fs::remove_file(f.base.root.path().join("known/old.wav")).unwrap();
    fs::remove_file(f.base.root.path().join("known/old.txt")).unwrap();
    fs::remove_dir_all(f.base.root.path().join("known/nested")).unwrap();
    fs::create_dir(f.base.root.path().join("known/new")).unwrap();
    f.put("known/new/new.wav", b"new audio");
    f.put("known/new.txt", b"new unsupported");
    let batch = subtree_batch(&f, 10, 18, Path::new("known"));
    let request = service.submit_replay(batch.clone(), 7, before).unwrap();
    let CatalogReplayOutcome::Committed { receipt, delta } = Fixture::receive(&request) else {
        panic!("complete bounded subtree must checkpoint atomically");
    };
    assert_eq!(delta.revision, before + 1);
    assert_eq!(receipt.source_revision(), delta.revision);
    assert_eq!(receipt.prior().acknowledged_sequence(), 18);
    assert!(delta.authoritative_snapshot.is_none());
    assert_eq!(delta.touched_path_changes.len(), 3);
    assert_eq!(
        delta.directory_generation,
        old_dirs.state.active_generation()
    );
    assert!(
        delta
            .touched_directory_changes
            .iter()
            .any(|(p, entry)| p == Path::new("known/nested") && entry.is_none())
    );
    assert!(
        delta
            .touched_directory_changes
            .iter()
            .any(|(p, entry)| p == Path::new("known/new")
                && entry.as_ref().is_some_and(|e| &e.relative_path == p))
    );
    assert!(
        !delta
            .touched_directory_changes
            .iter()
            .any(|(p, _)| p == Path::new("other"))
    );
    let read = f.read();
    assert!(
        read.entry_for_path(Path::new("known/old.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    assert!(
        read.entry_for_path(Path::new("known/nested/child.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    assert!(
        !read
            .entry_for_path(Path::new("known/new/new.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    let index = delta.source_index_commit.unwrap();
    assert!(
        index
            .removed_paths
            .contains(&PathBuf::from("known/old.txt"))
    );
    assert!(
        index
            .upserted_entries
            .iter()
            .any(|e| e.relative_path == Path::new("known/new.txt"))
    );
    let dirs = read.source_directory_truth_page(None, 64).unwrap();
    assert!(
        dirs.entries
            .iter()
            .any(|e| e.relative_path == Path::new("other"))
    );
    assert!(
        dirs.entries
            .iter()
            .any(|e| e.relative_path == Path::new("known/new"))
    );
    assert!(
        !dirs
            .entries
            .iter()
            .any(|e| e.relative_path == Path::new("known/nested"))
    );
    assert!(
        matches!(dirs.state,SourceDirectoryTruthState::Active { published_source_revision,.. } if published_source_revision==before+1)
    );
    assert!(
        read.source_directory_truth_page(old_dirs.next_cursor.as_ref(), 1)
            .is_err()
    );
    assert_eq!(
        read.last_authoritative_audit_metadata().unwrap(),
        prior_audit
    );
    drop(read);
    drop(request);
    service.shutdown();
    let restarted = f.start();
    let retry = restarted.submit_replay(batch, 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&retry),
        CatalogReplayOutcome::PreviouslyCommitted { .. }
    ));
    assert_eq!(f.read().get_revision().unwrap(), before + 1);
    drop(retry);
    f.put("known/new/new.wav", b"new exact change");
    let request = restarted
        .submit_replay(
            f.batch(18, &[(Some(19), "known/new/new.wav")]),
            7,
            before + 1,
        )
        .unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::Committed { .. }
    ));
    restarted.shutdown();
}

#[test]
fn actual_subtree_checkpoint_sql_failure_rolls_back_directory_audio_index_and_absence() {
    let mut f = fixture();
    f.fail_replay_update = true;
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    let directories = f
        .read()
        .source_directory_truth_page(None, 64)
        .unwrap()
        .entries;
    fs::remove_dir_all(f.base.root.path().join("known/nested")).unwrap();
    f.put("known/old.wav", b"candidate changed audio");
    fs::create_dir(f.base.root.path().join("known/new")).unwrap();
    f.put("known/new.txt", b"candidate index");
    let batch = subtree_batch(&f, 10, 17, Path::new("known"));
    let request = service.submit_replay(batch, 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::AuditRequired { .. }
    ));
    let read = f.read();
    assert_eq!(read.get_revision().unwrap(), before);
    assert_eq!(
        read.source_directory_truth_page(None, 64).unwrap().entries,
        directories
    );
    assert!(
        !read
            .entry_for_path(Path::new("known/nested/child.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    assert_eq!(
        read.entry_for_path(Path::new("known/old.wav"))
            .unwrap()
            .unwrap()
            .content_hash,
        Some(blake3::hash(b"old audio").to_hex().to_string())
    );
    assert!(
        read.source_index_page(None, 64)
            .unwrap()
            .entries
            .iter()
            .all(|e| e.relative_path != Path::new("known/new.txt"))
    );
    service.shutdown();
}

#[test]
fn budget_missing_target_or_replaced_directory_require_correlated_audit_without_ack() {
    for failure in 0..3 {
        let f = fixture();
        let service = f.start();
        let before = f.read().get_revision().unwrap();
        match failure {
            0 => {
                for i in 0..65 {
                    f.put(&format!("known/extra-{i}.txt"), b"too many");
                }
            }
            1 => {
                fs::remove_dir_all(f.base.root.path().join("known")).unwrap();
            }
            _ => {
                fs::rename(
                    f.base.root.path().join("known"),
                    f.base.root.path().join("old-known"),
                )
                .unwrap();
                fs::create_dir(f.base.root.path().join("known")).unwrap();
            }
        }
        let batch = subtree_batch(&f, 10, 15, Path::new("known"));
        let request = service.submit_replay(batch.clone(), 7, before).unwrap();
        assert!(matches!(
            Fixture::receive(&request),
            CatalogReplayOutcome::AuditRequired { .. }
        ));
        assert_eq!(f.read().get_revision().unwrap(), before);
        let next = service
            .submit_replay(f.batch(15, &[(Some(16), "one.wav")]), 7, before)
            .unwrap();
        assert!(matches!(
            Fixture::receive(&next),
            CatalogReplayOutcome::Retained {
                reason: CatalogReplayError::PendingDifferent,
                ..
            }
        ));
        service.shutdown();
    }
}

#[cfg(unix)]
#[test]
fn raw_directory_and_file_addition_then_absence_stay_lossless_in_worker_transaction() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new(true);
    let raw = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xff-dir".to_vec()));
    fs::create_dir(f.base.root.path().join(&raw)).unwrap();
    let old = raw.join(std::ffi::OsString::from_vec(b"old-\xff.wav".to_vec()));
    fs::write(f.base.root.path().join(&old), b"old raw").unwrap();
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    fs::remove_file(f.base.root.path().join(&old)).unwrap();
    let new = raw.join(std::ffi::OsString::from_vec(b"new-\xfe.wav".to_vec()));
    fs::write(f.base.root.path().join(&new), b"new raw").unwrap();
    let request = service
        .submit_replay(subtree_batch(&f, 10, 14, &raw), 7, before)
        .unwrap();
    let CatalogReplayOutcome::Committed { delta, .. } = Fixture::receive(&request) else {
        panic!("raw subtree must be exact");
    };
    assert!(delta.touched_path_changes.is_empty());
    let index = delta.source_index_commit.unwrap();
    assert!(index.removed_paths.contains(&old));
    assert!(
        index
            .upserted_entries
            .iter()
            .any(|e| e.relative_path == new)
    );
    assert!(
        f.read()
            .source_directory_truth_page(None, 64)
            .unwrap()
            .entries
            .iter()
            .any(|e| e.relative_path == raw)
    );
    service.shutdown();
}
