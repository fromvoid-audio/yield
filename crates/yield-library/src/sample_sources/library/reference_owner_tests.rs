// Actual global/source owner tests with seeded Harvest metadata. No physical move qualification.
use super::super::super::reference_participant::*;
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{OperationId, ParticipantId, StoredRelativePath};
use crate::sample_sources::{SourceDatabase, SourceWriter};

#[cfg(test)]
#[path = "reference_owner_tests/wal_maintenance.rs"]
mod wal_maintenance;

struct ReferenceFixture {
    global: Option<GlobalLibraryWriter>,
    origin: Option<SourceWriter>,
    destination: Option<SourceWriter>,
    guard: Option<WritableProfileGuard>,
    roots: [tempfile::TempDir; 2],
    _profile: PersistenceProfileGuard,
    _base_guard: ConfigBaseGuard,
    _base: tempfile::TempDir,
    _runtime: TestRuntimeGuard,
}
impl ReferenceFixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempdir().unwrap();
        let (base_guard, profile, guard, global) = start_writer(&base, "reference-owner");
        wait_until_available(&global);
        let roots = [tempdir().unwrap(), tempdir().unwrap()];
        let origin = Self::source(&roots[0], &guard, "origin", 1, true);
        let destination = Self::source(&roots[1], &guard, "destination", 1, true);
        Self {
            global: Some(global),
            origin: Some(origin),
            destination: Some(destination),
            guard: Some(guard),
            roots,
            _profile: profile,
            _base_guard: base_guard,
            _base: base,
            _runtime: runtime,
        }
    }
    fn source(
        root: &tempfile::TempDir,
        guard: &WritableProfileGuard,
        id: &str,
        lifecycle: u64,
        initialize: bool,
    ) -> SourceWriter {
        let db =
            SourceDatabase::open_for_catalog_with_profile_guard(root.path(), guard, Path::new(id))
                .unwrap();
        if initialize {
            db.upsert_file(Path::new("one.wav"), 10, 1).unwrap();
            db.begin_source_directory_truth_generation(1, 0).unwrap();
        }
        let writer = SourceWriter::new_for_scan(db, SourceId::from_string(id), lifecycle).unwrap();
        if initialize {
            let root = writer.current_scan_fence().unwrap().root_identity().clone();
            writer
                .catalog_database()
                .write_batch()
                .unwrap()
                .commit_manifest_audit_with_staged_directory_truth(1, 1, 10, root, None)
                .unwrap();
        }
        writer
    }
    fn global(&self) -> &GlobalLibraryWriter {
        self.global.as_ref().unwrap()
    }
    fn owner(&self) -> &str {
        self.guard.as_ref().unwrap().owner_identity()
    }
    fn db(&self) -> Connection {
        Connection::open(
            self.guard
                .as_ref()
                .unwrap()
                .profile_root()
                .join(super::super::super::LIBRARY_DB_FILE_NAME),
        )
        .unwrap()
    }
    fn keys() -> (ReferenceFileKey, ReferenceFileKey) {
        (
            ReferenceFileKey::new(
                SourceId::from_string("origin"),
                StoredRelativePath::new("one.wav").unwrap(),
            )
            .unwrap(),
            ReferenceFileKey::new(
                SourceId::from_string("destination"),
                StoredRelativePath::new("new.wav").unwrap(),
            )
            .unwrap(),
        )
    }
    fn fences(
        &self,
    ) -> (
        crate::sample_sources::db::SourceReferenceFence,
        crate::sample_sources::db::SourceReferenceFence,
    ) {
        (
            self.origin
                .as_ref()
                .unwrap()
                .reference_read_fence()
                .unwrap(),
            self.destination
                .as_ref()
                .unwrap()
                .reference_read_fence()
                .unwrap(),
        )
    }
    fn snapshot(&self) -> Result<ReferenceSnapshot, LibraryError> {
        let (old, new) = Self::keys();
        let (origin, destination) = self.fences();
        self.global()
            .reference_snapshot(
                ReferenceSnapshotRequest::new(self.owner(), old, new, origin, destination).unwrap(),
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
    }
    fn request(&self, id: u128) -> ReferenceRekeyRequest {
        let snapshot = self.snapshot().unwrap();
        let (origin, destination) = self.fences();
        ReferenceRekeyRequest::new(
            self.owner(),
            OperationId::from_uuid(uuid::Uuid::from_u128(id)),
            WorkUnitKey::new(
                255,
                10,
                ParticipantId::from_uuid(uuid::Uuid::from_u128(2)),
                WorkUnitRole::GlobalLibrary,
            ),
            snapshot,
            origin,
            destination,
        )
        .unwrap()
    }
    fn apply(&self, request: ReferenceRekeyRequest) -> Result<ReferenceRekeyResult, LibraryError> {
        self.global()
            .apply_reference_rekey(request)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
    }
    fn seed(&self) {
        self.db().execute_batch("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at,note) VALUES('origin','one.wav',10,1,'old-hash','done',7,'keep exact state');
    INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,child_source_id,child_relative_path,operation,inherited_tags_json,tool_version,created_at) VALUES('origin','one.wav','origin','one.wav','extract','[\"tag\"]','fixture',8),('other','parent.wav','origin','one.wav','extract','[]','fixture',9),('origin','one.wav','other','child.wav','extract','[]','fixture',10);").unwrap();
    }
    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn restart(&mut self) {
        self.global.as_mut().unwrap().shutdown();
        drop(self.global.take());
        drop(self.origin.take());
        drop(self.destination.take());
        drop(self.guard.take());
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let global = GlobalLibraryWriter::start(&guard).unwrap();
        wait_until_available(&global);
        self.origin = Some(Self::source(&self.roots[0], &guard, "origin", 9, false));
        self.destination = Some(Self::source(
            &self.roots[1],
            &guard,
            "destination",
            9,
            false,
        ));
        self.guard = Some(guard);
        self.global = Some(global);
    }
}
#[test]
fn reference_owner_lost_reply_self_endpoints_old_key_reuse_and_restart() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let bytes = request.proposal_bytes().to_vec();
    let dispatch_revision = f.global().observed_mutation_revision_for_test();
    drop(f.global().apply_reference_rekey(request.clone()).unwrap());
    let replay = f.apply(request.clone()).unwrap();
    assert_eq!(
        f.global().observed_mutation_revision_for_test(),
        dispatch_revision + 2
    );
    assert!(matches!(replay, ReferenceRekeyResult::Replayed(_)));
    assert_eq!(
        replay.receipt().disposition(),
        ReferenceDisposition::Applied
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='origin'"),
        0
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='destination' AND note='keep exact state' AND content_hash='old-hash' AND harvest_state='done'"),1);
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations WHERE parent_source_id='origin' OR child_source_id='origin'"),0);
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations WHERE id=1 AND parent_source_id='destination' AND child_source_id='destination' AND inherited_tags_json='[\"tag\"]'"),1);
    f.db().execute_batch("INSERT INTO harvest_files(source_id,relative_path,content_hash,harvest_state,discovered_at) VALUES('origin','one.wav','later','ignored',99);").unwrap();
    assert_eq!(f.apply(request.clone()).unwrap(), replay);
    f.restart();
    assert!(matches!(
        f.apply(request),
        Err(LibraryError::ReferenceStaleOwner)
    ));
    let (origin, destination) = f.fences();
    let restored = ReferenceRekeyRequest::restore(f.owner(), &bytes, origin, destination).unwrap();
    assert_eq!(f.apply(restored).unwrap(), replay);
    assert_ne!(replay.receipt().owner_identity().to_string(), f.owner());
    assert_eq!(
        f.count(
            "SELECT COUNT(*) FROM harvest_files WHERE source_id='origin' AND content_hash='later'"
        ),
        1
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
}
#[test]
fn reference_owner_changed_missing_and_destination_collision_are_retained() {
    let f = ReferenceFixture::new();
    let missing = f.request(1);
    assert_eq!(
        f.apply(missing.clone()).unwrap().receipt().disposition(),
        ReferenceDisposition::ReferenceMissing
    );
    f.seed();
    assert_eq!(
        f.apply(missing).unwrap().receipt().disposition(),
        ReferenceDisposition::ReferenceMissing
    );
    let changed = f.request(2);
    f.db()
        .execute(
            "UPDATE harvest_derivations SET tool_version='changed' WHERE id=3",
            [],
        )
        .unwrap();
    assert_eq!(
        f.apply(changed.clone()).unwrap().receipt().disposition(),
        ReferenceDisposition::ReferenceChanged
    );
    assert!(matches!(
        f.apply(changed).unwrap(),
        ReferenceRekeyResult::Replayed(_)
    ));
    f.db().execute_batch("INSERT INTO harvest_files(source_id,relative_path,harvest_state,discovered_at) VALUES('destination','new.wav','ignored',5);").unwrap();
    let collision = f.request(3);
    assert_eq!(
        f.apply(collision.clone()).unwrap().receipt().disposition(),
        ReferenceDisposition::DestinationOccupied
    );
    f.db()
        .execute(
            "DELETE FROM harvest_files WHERE source_id='destination'",
            [],
        )
        .unwrap();
    assert_eq!(
        f.apply(collision).unwrap().receipt().disposition(),
        ReferenceDisposition::DestinationOccupied
    );
    // A destination edge alone must also prevent silent merging.
    f.db().execute("UPDATE harvest_derivations SET child_source_id='destination',child_relative_path='new.wav' WHERE id=2",[]).unwrap();
    assert_eq!(
        f.apply(f.request(4)).unwrap().receipt().disposition(),
        ReferenceDisposition::DestinationOccupied
    );
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='origin'"),
        1
    );
}
#[test]
fn reference_owner_snapshot_bounds_complete_graph_and_malformed_types() {
    let f = ReferenceFixture::new();
    f.seed();
    let original = f.snapshot().unwrap();
    f.db()
        .execute("UPDATE harvest_files SET note=?1", ["é".repeat(2049)])
        .unwrap();
    assert!(matches!(
        f.snapshot(),
        Err(LibraryError::ReferenceSnapshotInvalid)
    ));
    f.db()
        .execute("UPDATE harvest_files SET note=x'00'", [])
        .unwrap();
    assert!(matches!(
        f.snapshot(),
        Err(LibraryError::ReferenceSnapshotInvalid)
    ));
    f.db()
        .execute("UPDATE harvest_files SET note='keep exact state'", [])
        .unwrap();
    assert_eq!(f.snapshot().unwrap().identity(), original.identity());
    // 64 distinct rows, including an edge mentioning the key at both ends, pass.
    f.db().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<61) INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,child_source_id,child_relative_path,operation,tool_version,created_at) SELECT 'origin','one.wav','other','x'||x,'extract','fixture',x FROM n;").unwrap();
    f.snapshot().unwrap();
    f.db()
        .execute(
            "UPDATE harvest_derivations SET inherited_tags_json=?1",
            ["x".repeat(1100)],
        )
        .unwrap();
    assert!(matches!(
        f.snapshot(),
        Err(LibraryError::ReferenceSnapshotInvalid)
    ));
    f.db()
        .execute(
            "UPDATE harvest_derivations SET inherited_tags_json='[]'",
            [],
        )
        .unwrap();
    f.db().execute_batch("INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,child_source_id,child_relative_path,operation,tool_version,created_at) VALUES('origin','one.wav','other','65','extract','fixture',65);").unwrap();
    assert!(matches!(
        f.snapshot(),
        Err(LibraryError::ReferenceSnapshotInvalid)
    ));
}
#[test]
fn reference_owner_sql_abort_rolls_back_rekey_and_receipt_then_retry() {
    let f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let snapshot = f.snapshot().unwrap();
    f.db().execute_batch("CREATE TRIGGER abort_reference BEFORE INSERT ON reference_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(matches!(
        f.apply(request.clone()),
        Err(LibraryError::Sql(_))
    ));
    assert_eq!(f.snapshot().unwrap().identity(), snapshot.identity());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 0);
    f.db()
        .execute_batch("DROP TRIGGER abort_reference")
        .unwrap();
    assert_eq!(
        f.apply(request).unwrap().receipt().disposition(),
        ReferenceDisposition::Applied
    );
}
#[test]
fn reference_owner_conflict_corrupt_result_and_ambiguous_evidence_fail_closed() {
    let f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    f.apply(request.clone()).unwrap();
    assert!(matches!(
        f.apply(f.request(1)),
        Err(LibraryError::ReferenceReplayConflict)
    ));
    f.db()
        .execute("UPDATE reference_receipts SET disposition='missing'", [])
        .unwrap();
    assert!(matches!(
        f.apply(request.clone()),
        Err(LibraryError::ReferenceMalformedReceipt)
    ));
    f.db().execute_batch("ALTER TABLE reference_receipts RENAME TO original_receipts; CREATE TABLE reference_receipts AS SELECT * FROM original_receipts; INSERT INTO reference_receipts SELECT * FROM original_receipts;").unwrap();
    assert!(matches!(
        f.apply(request),
        Err(LibraryError::ReferenceMalformedReceipt)
    ));
}
#[test]
fn reference_owner_capacity_preserves_replay_and_rejects_new_effects() {
    let f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let receipt = f.apply(request.clone()).unwrap().receipt().clone();
    let db = f.db();
    db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<256) INSERT INTO reference_receipts(command_key,payload,payload_identity,disposition,owner_identity) SELECT 'budget-'||x,zeroblob(65536),zeroblob(32),'missing','00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    assert!(matches!(
        f.apply(f.request(2)),
        Err(LibraryError::ReferenceReceiptCapacity)
    ));
    assert_eq!(f.apply(request.clone()).unwrap().receipt(), &receipt);
    db.execute(
        "DELETE FROM reference_receipts WHERE command_key LIKE 'budget-%'",
        [],
    )
    .unwrap();
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1) INSERT INTO reference_receipts(command_key,payload,payload_identity,disposition,owner_identity) SELECT 'budget-'||x,x'00',zeroblob(32),'missing','00000000-0000-0000-0000-000000000001' FROM n",[REFERENCE_MAX_RECEIPTS-1]).unwrap();
    assert!(matches!(
        f.apply(f.request(3)),
        Err(LibraryError::ReferenceReceiptCapacity)
    ));
    assert_eq!(f.apply(request).unwrap().receipt(), &receipt);
}
#[test]
fn reference_owner_ended_source_and_changed_revision_fail_before_effect() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    // A source-owner curation commit changes the audited revision, invalidating queued work.
    let writer = f.origin.as_mut().unwrap();
    let update = crate::sample_sources::SourceWriterRequest::new(
        OperationId::new(),
        ParticipantId::new(),
        crate::sample_sources::SourceCommandId::new(),
        writer.current_revision().unwrap(),
        vec![crate::sample_sources::SourceCurationUpdate::SetRating {
            path: crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                "one.wav".into(),
            )
            .unwrap(),
            rating: crate::sample_sources::Rating::KEEP_1,
        }],
    )
    .unwrap();
    writer.commit_curation(update).unwrap();
    let bytes = request.proposal_bytes().to_vec();
    assert!(matches!(
        f.apply(request),
        Err(LibraryError::ReferenceSource(_))
    ));
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 0);
    let (origin, destination) = f.fences();
    let restored = ReferenceRekeyRequest::restore(f.owner(), &bytes, origin, destination).unwrap();
    // Historical input is readable with fresh scope, but absent receipt cannot authorize
    // a new mutation across a changed revision.
    assert!(matches!(
        f.apply(restored),
        Err(LibraryError::ReferenceInput)
    ));

    let request = f.request(2);
    drop(f.origin.take());
    assert!(matches!(
        f.apply(request),
        Err(LibraryError::ReferenceSource(_))
    ));
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='origin'"),
        1
    );
}
#[test]
fn reference_owner_proposal_bounds_canonical_replay_and_endpoint_binding() {
    let f = ReferenceFixture::new();
    let request = f.request(1);
    let (origin, destination) = f.fences();
    assert!(
        ReferenceRekeyRequest::restore(
            f.owner(),
            &vec![0; 65537],
            origin.clone(),
            destination.clone()
        )
        .is_err()
    );
    let mut bytes = request.proposal_bytes().to_vec();
    bytes.push(b' ');
    assert!(
        ReferenceRekeyRequest::restore(f.owner(), &bytes, origin.clone(), destination.clone())
            .is_err()
    );
    assert!(
        ReferenceRekeyRequest::restore(
            f.owner(),
            request.proposal_bytes(),
            destination.clone(),
            origin.clone()
        )
        .is_err()
    );
    let mut value: serde_json::Value = serde_json::from_slice(request.proposal_bytes()).unwrap();
    value["version"] = 2.into();
    assert!(
        ReferenceRekeyRequest::restore(
            f.owner(),
            &serde_json::to_vec(&value).unwrap(),
            origin,
            destination
        )
        .is_err()
    );
    assert!(
        ReferenceFileKey::new(
            SourceId::from_string("x".repeat(257)),
            StoredRelativePath::new("one.wav").unwrap()
        )
        .is_err()
    );
}
#[test]
fn reference_owner_source_change_after_commit_reports_uncertainty_and_recovers() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let bytes = request.proposal_bytes().to_vec();
    f.global.as_mut().unwrap().shutdown();
    drop(f.global.take());
    let (writer, ready, release) =
        GlobalLibraryWriter::start_with_post_command_gate_for_test(f.guard.as_ref().unwrap());
    wait_until_available(&writer);
    f.global = Some(writer);
    let receiver = f.global().apply_reference_rekey(request).unwrap();
    ready.wait();
    let old = f.origin.take().unwrap();
    let database = old.into_catalog_database();
    f.origin = Some(SourceWriter::new(database, SourceId::from_string("origin"), 8).unwrap());
    release.wait();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
        Err(LibraryError::ReferenceCommitUncertain)
    ));
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
    // Remove the fixture's repeating gate before exact live-context readback.
    f.global.as_mut().unwrap().shutdown();
    drop(f.global.take());
    let writer = GlobalLibraryWriter::start(f.guard.as_ref().unwrap()).unwrap();
    wait_until_available(&writer);
    f.global = Some(writer);
    let (origin, destination) = f.fences();
    let restored = ReferenceRekeyRequest::restore(f.owner(), &bytes, origin, destination).unwrap();
    assert!(matches!(
        f.apply(restored).unwrap(),
        ReferenceRekeyResult::Replayed(_)
    ));
}

#[test]
fn reference_owner_edge_only_scope_and_bounded_malformed_receipt() {
    let f = ReferenceFixture::new();
    f.seed();
    f.db().execute("DELETE FROM harvest_files", []).unwrap();
    let request = f.request(1);
    let receipt = f.apply(request.clone()).unwrap();
    assert_eq!(
        receipt.receipt().disposition(),
        ReferenceDisposition::Applied
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations WHERE parent_source_id='origin' OR child_source_id='origin'"), 0);
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_files"), 0);
    // Bypass schema checks to exercise bounded readback against corrupt retained evidence.
    let db = f.db();
    db.execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    let original: Vec<u8> = db
        .query_row("SELECT payload FROM reference_receipts", [], |r| r.get(0))
        .unwrap();
    db.execute("UPDATE reference_receipts SET payload=zeroblob(65537)", [])
        .unwrap();
    assert!(matches!(
        f.apply(request.clone()),
        Err(LibraryError::ReferenceMalformedReceipt)
    ));
    db.execute("UPDATE reference_receipts SET payload=?1", [original])
        .unwrap();
    assert_eq!(
        f.apply(request.clone()).unwrap().receipt(),
        receipt.receipt()
    );
    db.execute("UPDATE reference_receipts SET payload_identity=x'00'", [])
        .unwrap();
    assert!(matches!(
        f.apply(request),
        Err(LibraryError::ReferenceMalformedReceipt)
    ));
}

#[test]
fn reference_owner_reversed_replies_preserve_immutable_operation_results_across_restart() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let first = f.request(183601);
    let second = f.request(183602);
    let first_bytes = first.proposal_bytes().to_vec();
    let second_bytes = second.proposal_bytes().to_vec();
    let first_rx = f.global().apply_reference_rekey(first.clone()).unwrap();
    let second_rx = f.global().apply_reference_rekey(second.clone()).unwrap();
    let rejected = second_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let applied = first_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(
        applied.receipt().operation_id(),
        OperationId::from_uuid(uuid::Uuid::from_u128(183601))
    );
    assert_eq!(
        rejected.receipt().operation_id(),
        OperationId::from_uuid(uuid::Uuid::from_u128(183602))
    );
    assert_eq!(
        applied.receipt().payload_identity(),
        first.payload_identity()
    );
    assert_eq!(
        rejected.receipt().payload_identity(),
        second.payload_identity()
    );
    assert_eq!(
        applied.receipt().disposition(),
        ReferenceDisposition::Applied
    );
    assert_ne!(
        rejected.receipt().disposition(),
        ReferenceDisposition::Applied
    );
    let replay_second = f.apply(second).unwrap();
    let replay_first = f.apply(first).unwrap();
    assert_eq!(replay_second.receipt(), rejected.receipt());
    assert_eq!(replay_first.receipt(), applied.receipt());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 2);
    f.restart();
    let (origin, destination) = f.fences();
    let first = ReferenceRekeyRequest::restore(
        f.owner(),
        &first_bytes,
        origin.clone(),
        destination.clone(),
    )
    .unwrap();
    let second =
        ReferenceRekeyRequest::restore(f.owner(), &second_bytes, origin, destination).unwrap();
    assert_eq!(f.apply(second).unwrap().receipt(), rejected.receipt());
    assert_eq!(f.apply(first).unwrap().receipt(), applied.receipt());
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 2);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM harvest_files WHERE source_id='origin'"),
        0
    );
}
