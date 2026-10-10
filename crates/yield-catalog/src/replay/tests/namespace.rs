use super::*;

#[test]
fn known_nested_parent_survives_cold_restart_but_new_parent_requires_audit() {
    let f = Fixture::new(true);
    fs::create_dir(f.base.root.path().join("known")).unwrap();
    f.put("known/one.wav", b"audited nested file");
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    f.put("known/one.wav", b"first nested change");
    let request = service
        .submit_replay(f.batch(10, &[(Some(11), "known/one.wav")]), 7, before)
        .unwrap();
    let CatalogReplayOutcome::Committed { delta, .. } = Fixture::receive(&request) else {
        panic!("known nested parent must be supported");
    };
    drop(request);
    service.shutdown();
    let service = f.start();
    f.put("known/one.wav", b"nested change after restart");
    let request = service
        .submit_replay(
            f.batch(11, &[(Some(12), "known/one.wav")]),
            7,
            delta.revision,
        )
        .unwrap();
    let CatalogReplayOutcome::Committed { delta, .. } = Fixture::receive(&request) else {
        panic!("bounded DB parent read supports cold replay restart");
    };
    fs::create_dir(f.base.root.path().join("unknown")).unwrap();
    f.put("unknown/one.wav", b"unobserved directory");
    let request = service
        .submit_replay(
            f.batch(12, &[(Some(13), "unknown/one.wav")]),
            7,
            delta.revision,
        )
        .unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::AuditRequired {
            reason: CatalogReplayError::InvalidTarget
        }
    ));
    assert_eq!(f.read().get_revision().unwrap(), delta.revision);
    service.shutdown();
}

#[test]
fn substituted_parent_with_same_file_object_cannot_checkpoint_stale_directory_truth() {
    let f = Fixture::new(true);
    let parent = f.base.root.path().join("known");
    fs::create_dir(&parent).unwrap();
    f.put("known/one.wav", b"same held file object");
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    let moved = f.base.root.path().join("old-parent");
    fs::rename(&parent, &moved).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::rename(moved.join("one.wav"), parent.join("one.wav")).unwrap();
    let request = service
        .submit_replay(f.batch(10, &[(Some(11), "known/one.wav")]), 7, before)
        .unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::AuditRequired { .. }
    ));
    assert_eq!(f.read().get_revision().unwrap(), before);
    service.shutdown();
}

#[cfg(unix)]
#[test]
fn raw_filename_remains_exact_index_evidence_in_the_actual_replay_transaction() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let f = Fixture::new(true);
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    let invalid = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xff.wav".to_vec()));
    #[cfg(target_os = "macos")]
    let path = {
        assert_eq!(
            RootRelativePath::try_from_path(invalid.clone())
                .unwrap()
                .as_path(),
            invalid.as_path()
        );
        let error = fs::write(f.base.root.path().join(&invalid), b"raw filename").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(92), "APFS EILSEQ: {error}");
        assert_eq!(f.read().get_revision().unwrap(), before);
        // A supported non-audio path preserves the original index-only transaction.
        PathBuf::from("raw-é.txt")
    };
    #[cfg(not(target_os = "macos"))]
    let path = invalid;
    let absolute = f.base.root.path().join(&path);
    fs::write(&absolute, b"raw filename").unwrap();
    let ingress = f.ingress(10);
    ingress.record_with_kind(
        Some(11),
        &absolute,
        ReplayEventCondition::Changed,
        ReplayPathKind::File,
    );
    ingress.mark_history_complete();
    let batch = ingress
        .into_candidate()
        .into_observation_batch(7, WatcherGeneration::new(7))
        .unwrap();
    let request = service.submit_replay(batch, 7, before).unwrap();
    let CatalogReplayOutcome::Committed { delta, .. } = Fixture::receive(&request) else {
        panic!("raw index transaction must commit");
    };
    assert!(delta.touched_path_changes.is_empty());
    assert_eq!(
        delta.source_index_commit.unwrap().upserted_entries[0]
            .relative_path
            .as_os_str()
            .as_bytes(),
        path.as_os_str().as_bytes()
    );
    service.shutdown();
}
