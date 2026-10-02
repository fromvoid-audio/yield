use super::*;
#[test]
fn harvest_copy_owner_rejects_unbounded_and_ambiguous_existing_child_rows() {
    for ambiguous in [false, true] {
        let f = Fixture::new();
        let c = f.db();
        c.execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at,note) VALUES('destination','new.wav',10,1,?1,'ignored',1,?2)",params!["a".repeat(64),if ambiguous{"authored".into()}else{"x".repeat(1000000)}]).unwrap();
        if ambiguous {
            c.execute_batch("ALTER TABLE harvest_files RENAME TO saved_files;CREATE TABLE harvest_files AS SELECT * FROM saved_files;INSERT INTO harvest_files SELECT * FROM saved_files;").unwrap();
        }
        let request = f.request();
        assert!(matches!(
            f.apply(request),
            Err(LibraryError::HarvestCopyMalformedGraph)
        ));
        assert_eq!(f.count("harvest_derivations"), 0);
        assert_eq!(f.count("harvest_copy_receipts"), 0);
    }
}
#[test]
fn harvest_copy_owner_sql_rollback_and_changed_payload_fail_closed() {
    let f = Fixture::new();
    let r = f.request();
    f.db().execute_batch("CREATE TRIGGER fail_copy BEFORE INSERT ON harvest_copy_receipts BEGIN SELECT RAISE(ABORT,'copy rollback');END;").unwrap();
    assert!(f.apply(r.clone()).is_err());
    assert_eq!(f.count("harvest_files"), 0);
    assert_eq!(f.count("harvest_derivations"), 0);
    assert_eq!(f.count("harvest_copy_receipts"), 0);
    f.db().execute_batch("DROP TRIGGER fail_copy").unwrap();
    assert_eq!(
        f.apply(r.clone()).unwrap().receipt().disposition(),
        HarvestCopyDisposition::Applied
    );
    let mut changed = r;
    changed.payload.tool = "changed-tool".into();
    changed.bytes = changed.payload.encode().unwrap();
    assert!(matches!(
        f.apply(changed),
        Err(LibraryError::HarvestCopyReplayConflict)
    ));
    assert_eq!(f.count("harvest_derivations"), 1);
}
#[test]
fn harvest_copy_owner_stale_sources_and_registry_mismatch_reject_before_effect() {
    for fault in ["source", "registry"] {
        let f = Fixture::new();
        let r = f.request();
        if fault == "source" {
            f.source_connection(0)
                .execute("UPDATE wav_files SET file_identity='changed-object'", [])
                .unwrap();
        } else {
            f.db()
                .execute(
                    "UPDATE source_registry_participant_receipts SET payload_identity=zeroblob(32)",
                    [],
                )
                .unwrap();
        }
        assert!(f.apply(r).is_err());
        assert_eq!(f.count("harvest_copy_receipts"), 0);
        assert_eq!(f.count("harvest_derivations"), 0);
    }
}
#[test]
fn harvest_copy_owner_retains_graph_changed_and_identity_conflict_outcomes() {
    for conflict in [false, true] {
        let f = Fixture::new();
        if conflict {
            f.db().execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at) VALUES('destination','new.wav',10,1,?1,'done',1)",["b".repeat(64)]).unwrap();
        }
        let r = f.request();
        if !conflict {
            f.db().execute("INSERT INTO harvest_files(source_id,relative_path,harvest_state,discovered_at) VALUES('origin','one.wav','done',1)",[]).unwrap();
        }
        let first = f.apply(r.clone()).unwrap();
        let expected = if conflict {
            HarvestCopyDisposition::IdentityConflict
        } else {
            HarvestCopyDisposition::GraphChanged
        };
        assert_eq!(first.receipt().disposition(), expected);
        assert!(first.receipt().edge_id().is_none());
        assert_eq!(f.count("harvest_derivations"), 0);
        assert_eq!(f.apply(r).unwrap().receipt().disposition(), expected);
    }
}
#[test]
fn harvest_copy_owner_bounds_canonical_proposals_and_corrupt_receipts() {
    for fault in ["payload", "result", "identity", "owner", "ambiguous"] {
        let f = Fixture::new();
        let r = f.request();
        let bytes = r.proposal_bytes().to_vec();
        let mut noncanonical = bytes.clone();
        noncanonical.push(b' ');
        assert!(f.restore(&noncanonical).is_err());
        assert!(f.restore(&vec![b' '; 65537]).is_err());
        let mut oversized = r.clone();
        oversized.payload.metadata.tags = vec!["x".repeat(257)];
        assert!(oversized.payload.encode().is_err());
        f.apply(r.clone()).unwrap();
        match fault {
            "payload" => {
                f.db()
                    .execute(
                        "UPDATE harvest_copy_receipts SET payload=zeroblob(65536)",
                        [],
                    )
                    .unwrap();
            }
            "result" => {
                f.db()
                    .execute(
                        "UPDATE harvest_copy_receipts SET result=?1",
                        [b"{}".as_slice()],
                    )
                    .unwrap();
            }
            "identity" => {
                f.db()
                    .execute("UPDATE harvest_copy_receipts SET identity=zeroblob(32)", [])
                    .unwrap();
            }
            "owner" => {
                f.db()
                    .execute(
                        "UPDATE harvest_copy_receipts SET owner=?1",
                        [Uuid::new_v4().to_string()],
                    )
                    .unwrap();
            }
            _ => {
                f.db().execute_batch("ALTER TABLE harvest_copy_receipts RENAME TO saved_copy;CREATE TABLE harvest_copy_receipts AS SELECT * FROM saved_copy;INSERT INTO harvest_copy_receipts SELECT * FROM saved_copy;").unwrap();
            }
        }
        assert!(f.apply(r).is_err());
        assert_eq!(f.count("harvest_derivations"), 1);
    }
}
