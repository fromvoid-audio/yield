use super::*;
#[test]
fn harvest_authored_owner_all_states_reset_and_preserve_note_and_discovery() {
    let f = Fixture::new();
    for state in [
        HarvestState::Seen,
        HarvestState::Touched,
        HarvestState::Done,
        HarvestState::Ignored,
    ] {
        let revision = f.global().observed_mutation_revision_for_test();
        let request = f.request(state);
        assert_eq!(f.global().observed_mutation_revision_for_test(), revision);
        let r = f.apply(request).unwrap();
        assert_eq!(
            f.global().observed_mutation_revision_for_test(),
            revision + 1
        );
        assert_eq!(
            r.receipt().disposition(),
            HarvestAuthoredDisposition::Applied
        );
        assert_eq!(f.state(), state.as_str());
    }
    f.db()
        .execute("UPDATE harvest_files SET note='mine'", [])
        .unwrap();
    let discovered: i64 = f
        .db()
        .query_row("SELECT discovered_at FROM harvest_files", [], |r| r.get(0))
        .unwrap();
    f.apply(f.request(HarvestState::New)).unwrap();
    assert_eq!(f.state(), "new");
    let reset:bool=f.db().query_row("SELECT seen_at IS NULL AND touched_at IS NULL AND done_at IS NULL AND ignored_at IS NULL AND note='mine' AND discovered_at=?1 FROM harvest_files",[discovered],|r|r.get(0)).unwrap();
    assert!(reset);
    let edges: i64 = f
        .db()
        .query_row("SELECT COUNT(*) FROM harvest_derivations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(edges, 0);
}
#[test]
fn harvest_authored_owner_lost_reply_restart_preserves_later_user_edit() {
    let mut f = Fixture::new();
    let r = f.request(HarvestState::Done);
    let bytes = r.proposal_bytes().to_vec();
    let lost = f.global().set_harvest_authored_state(r).unwrap();
    drop(lost);
    // The next command is an actual owner queue barrier.
    f.apply(f.request(HarvestState::Ignored)).unwrap();
    f.restart();
    let replay = f.apply(f.restore(&bytes).unwrap()).unwrap();
    assert!(matches!(replay, HarvestAuthoredResult::Replayed(_)));
    assert_eq!(replay.receipt().requested_state(), HarvestState::Done);
    assert_eq!(f.state(), "ignored");
    assert_eq!(f.count(), 2);
}
#[test]
fn harvest_authored_owner_full_profile_handoff_and_later_content_replay() {
    let mut f = Fixture::new();
    let r = f.request(HarvestState::Done);
    let bytes = r.proposal_bytes().to_vec();
    let original = f.apply(r.clone()).unwrap();
    let mut g = f.global.take().unwrap();
    g.shutdown();
    drop(g);
    f.source
        .take()
        .unwrap()
        .into_catalog_database()
        .close()
        .unwrap();
    f.guard.take();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let g = GlobalLibraryWriter::start(&guard).unwrap();
    wait_global(&g);
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        f.root.path(),
        &guard,
        Path::new("origin"),
    )
    .unwrap();
    let s = SourceWriter::new_for_scan(db, SourceId::from_string("origin"), 2).unwrap();
    f.guard = Some(guard);
    f.global = Some(g);
    f.source = Some(s);
    assert!(r.validate_sources(false).is_err());
    f.source_db()
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    let replay = f.apply(f.restore(&bytes).unwrap()).unwrap();
    assert!(matches!(replay, HarvestAuthoredResult::Replayed(_)));
    assert_eq!(
        replay.receipt().owner_identity(),
        original.receipt().owner_identity()
    );
    assert_ne!(
        replay.receipt().owner_identity().to_string(),
        f.guard().owner_identity()
    );
    assert_eq!(replay.receipt().sequence(), original.receipt().sequence());
    assert_eq!(f.count(), 1);
}
#[test]
fn harvest_authored_owner_postcommit_source_change_is_uncertain_then_replayable() {
    let mut f = Fixture::new();
    let r = f.request(HarvestState::Done);
    let bytes = r.proposal_bytes().to_vec();
    let mut g = f.global.take().unwrap();
    g.shutdown();
    drop(g);
    let (g, ready, release) =
        super::super::super::owner::GlobalLibraryWriter::start_with_post_command_gate_for_test(
            f.guard(),
        );
    wait_global(&g);
    f.global = Some(g);
    let reply = f.global().set_harvest_authored_state(r).unwrap();
    ready.wait();
    assert_eq!(f.count(), 1);
    f.source_db()
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    release.wait();
    assert!(matches!(
        reply.recv_timeout(Duration::from_secs(3)).unwrap(),
        Err(LibraryError::HarvestAuthoredCommitUncertain)
    ));
    f.restart();
    assert!(matches!(
        f.apply(f.restore(&bytes).unwrap()).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
}
