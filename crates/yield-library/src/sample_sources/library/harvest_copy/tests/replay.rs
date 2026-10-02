use super::*;
#[test]
fn harvest_copy_owner_applies_one_edge_and_replays_without_touching_later_state() {
    let f = Fixture::new();
    let r = f.request();
    let bytes = r.proposal_bytes().to_vec();
    let first = f.apply(r.clone()).unwrap();
    assert!(matches!(first, HarvestCopyResult::Committed(_)));
    let receipt = first.receipt();
    assert_eq!(receipt.disposition(), HarvestCopyDisposition::Applied);
    assert_eq!(receipt.operation(), r.operation());
    assert_eq!(receipt.work_key(), r.work_key());
    assert!(receipt.edge_id().unwrap() > 0);
    assert_eq!(f.count("harvest_files"), 2);
    assert_eq!(f.count("harvest_derivations"), 1);
    f.db().execute("UPDATE harvest_files SET harvest_state='ignored',ignored_at=77,note='later authored state' WHERE source_id='origin'",[]).unwrap();
    let replay = f.apply(f.restore(&bytes).unwrap()).unwrap();
    assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
    assert_eq!(replay.receipt().edge_id(), receipt.edge_id());
    assert_eq!(replay.receipt().sequence(), receipt.sequence());
    assert_eq!(f.count("harvest_derivations"), 1);
    let authored: (String, i64, String) = f
        .db()
        .query_row(
            "SELECT harvest_state,ignored_at,note FROM harvest_files WHERE source_id='origin'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        authored,
        ("ignored".into(), 77, "later authored state".into())
    );
    let (parent, child): (String, String) = f
        .db()
        .query_row(
            "SELECT parent_source_id,child_source_id FROM harvest_derivations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((parent, child), ("origin".into(), "destination".into()));
}
#[test]
fn harvest_copy_owner_preserves_authored_done_and_ignored_rows() {
    let f = Fixture::new();
    let c = f.db();
    for (source, path, state) in [
        ("origin", "one.wav", "done"),
        ("destination", "new.wav", "ignored"),
    ] {
        c.execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at,seen_at,touched_at,done_at,ignored_at,note) VALUES(?1,?2,10,1,?3,?4,11,12,13,14,15,'authored')",params![source,path,"a".repeat(64),state]).unwrap();
    }
    let before:Vec<(String,String,i64,String)>=c.prepare("SELECT source_id,harvest_state,discovered_at,note FROM harvest_files ORDER BY source_id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(
        f.apply(f.request()).unwrap().receipt().disposition(),
        HarvestCopyDisposition::Applied
    );
    let after:Vec<(String,String,i64,String)>=c.prepare("SELECT source_id,harvest_state,discovered_at,note FROM harvest_files ORDER BY source_id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(before, after);
    let timestamps:(i64,i64,i64,i64)=c.query_row("SELECT seen_at,touched_at,done_at,ignored_at FROM harvest_files WHERE source_id='origin'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(timestamps, (12, 13, 14, 15));
}
#[test]
fn harvest_copy_owner_lost_reply_and_global_restart_replay_original_receipt() {
    let mut f = Fixture::new();
    let r = f.request();
    let bytes = r.proposal_bytes().to_vec();
    drop(f.global().apply_harvest_copy(r).unwrap());
    f.global()
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(f.count("harvest_derivations"), 1);
    let first = f.apply(f.restore(&bytes).unwrap()).unwrap();
    let edge = first.receipt().edge_id();
    let mut owner = f.global.take().unwrap();
    owner.shutdown();
    drop(owner);
    let owner = GlobalLibraryWriter::start(f.guard()).unwrap();
    wait_global(&owner);
    f.global = Some(owner);
    let replay = f.apply(f.restore(&bytes).unwrap()).unwrap();
    assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
    assert_eq!(replay.receipt().edge_id(), edge);
    assert_eq!(f.count("harvest_derivations"), 1);
    assert_eq!(f.roots.len(), 2);
}
#[test]
fn harvest_copy_owner_fresh_source_context_replay_keeps_historical_content() {
    let f = Fixture::new();
    let r = f.request();
    let bytes = r.proposal_bytes().to_vec();
    let first = f.apply(r.clone()).unwrap();
    f.source_connection(0)
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    assert!(r.validate_live(false).is_err());
    let fresh = f.restore(&bytes).unwrap();
    assert!(fresh.validate_live(true).is_err());
    let replay = f.apply(fresh).unwrap();
    assert_eq!(replay.receipt().edge_id(), first.receipt().edge_id());
    let hash: String = f
        .db()
        .query_row(
            "SELECT parent_content_hash FROM harvest_derivations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hash, "a".repeat(64));
}
