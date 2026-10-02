use super::*;
#[test]
fn harvest_copy_owner_rejects_foreign_acquisition_work_key_and_ended_journal() {
    let mut f = Fixture::new();
    let r = f.request();
    let mut foreign = r.clone();
    foreign.owner = Uuid::new_v4();
    assert!(matches!(
        f.apply(foreign),
        Err(LibraryError::HarvestCopyStaleOwner)
    ));
    let mut wrong = r.clone();
    let k = wrong.payload.work;
    wrong.payload.work = WorkUnitKey::new(
        k.action_ordinal(),
        k.step_ordinal() + 1,
        k.participant_id(),
        k.role(),
    );
    wrong.bytes = wrong.payload.encode().unwrap();
    assert!(f.apply(wrong).is_err());
    let mut journal = f.journal.take().unwrap();
    assert!(journal.shutdown());
    assert!(f.apply(r).is_err());
    assert_eq!(f.count("harvest_derivations"), 0);
    assert_eq!(f.count("harvest_copy_receipts"), 0);
}
#[test]
fn harvest_copy_owner_postcommit_source_change_reports_uncertain_and_replays() {
    let mut f = Fixture::new();
    let r = f.request();
    let bytes = r.proposal_bytes().to_vec();
    let mut old = f.global.take().unwrap();
    old.shutdown();
    drop(old);
    let (global, ready, release) =
        super::super::super::owner::GlobalLibraryWriter::start_with_post_command_gate_for_test(
            f.guard(),
        );
    wait_global(&global);
    f.global = Some(global);
    let reply = f.global().apply_harvest_copy(r).unwrap();
    ready.wait();
    // The actual graph+receipt transaction committed; replace exact source facts before reply.
    assert_eq!(f.count("harvest_derivations"), 1);
    assert_eq!(f.count("harvest_copy_receipts"), 1);
    f.source_connection(0)
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    release.wait();
    assert!(matches!(
        reply.recv_timeout(Duration::from_secs(3)).unwrap(),
        Err(LibraryError::HarvestCopyCommitUncertain)
    ));
    let mut gated = f.global.take().unwrap();
    gated.shutdown();
    drop(gated);
    let global = GlobalLibraryWriter::start(f.guard()).unwrap();
    wait_global(&global);
    f.global = Some(global);
    let replay = f.apply(f.restore(&bytes).unwrap()).unwrap();
    assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
    assert_eq!(f.count("harvest_derivations"), 1);
}
#[test]
fn harvest_copy_owner_full_profile_handoff_uses_fresh_real_owners_and_old_bytes() {
    let mut f = Fixture::new();
    let r = f.request();
    let bytes = r.proposal_bytes().to_vec();
    let original = f.apply(r.clone()).unwrap();
    let operation = r.operation();
    let harvest = r.work_key();
    let enrollment_bytes = f.enrollment.proposal_bytes().to_vec();
    let mut global = f.global.take().unwrap();
    global.shutdown();
    drop(global);
    let mut journal = f.journal.take().unwrap();
    assert!(journal.shutdown());
    drop(journal);
    for writer in f.sources.take().unwrap() {
        writer.into_catalog_database().close().unwrap();
    }
    f.guard.take();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let global = GlobalLibraryWriter::start(&guard).unwrap();
    wait_global(&global);
    let sources = ["origin", "destination"].map(|id| {
        let index = usize::from(id == "destination");
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            f.roots[index].path(),
            &guard,
            Path::new(id),
        )
        .unwrap();
        SourceWriter::new_for_scan(db, SourceId::from_string(id), 2).unwrap()
    });
    let stored = OperationJournalStore::open(&guard).unwrap();
    let journal = OperationJournalOwner::start_without_capacity_for_test(stored).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    while journal.status() != OperationJournalOwnerStatus::Available {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(r.validate_live(false).is_err());
    let family = journal
        .client()
        .observe_copy_intent(operation, harvest)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
        .unwrap();
    let enrollment = ManifestEnrollmentRequest::restore(&enrollment_bytes, &sources[1]).unwrap();
    let enrollment = sources[1]
        .replay_manifest_enrollment(&enrollment)
        .unwrap()
        .unwrap()
        .receipt;
    f.guard = Some(guard);
    f.global = Some(global);
    f.journal = Some(journal);
    f.sources = Some(sources);
    f.family = family;
    f.enrollment = enrollment;
    let replay = f.apply(f.restore(&bytes).unwrap()).unwrap();
    assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
    assert_eq!(
        replay.receipt().owner_identity(),
        original.receipt().owner_identity()
    );
    assert_ne!(
        replay.receipt().owner_identity().to_string(),
        f.guard().owner_identity()
    );
    assert_eq!(replay.receipt().edge_id(), original.receipt().edge_id());
    assert_eq!(
        replay.receipt().destination_revision(),
        original.receipt().destination_revision()
    );
    assert_eq!(f.count("harvest_derivations"), 1);
}
