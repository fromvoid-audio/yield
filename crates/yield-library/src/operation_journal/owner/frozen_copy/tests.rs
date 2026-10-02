//! Real journal owner/SQLite metadata evidence; test capacity/snapshots confer no physical grant.
use super::*;
use crate::{
    app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard},
    operation_journal::{
        codec::StoredOperationRecord,
        full_progress_codec::StoredFullProgressRecord,
        model::{
            OperationActor, OperationIntent, OperationKind, UniversalCommandKind, UniversalIntent,
        },
        work_plan::WorkUnitRole,
    },
    test_runtime::TestRuntimeGuard,
};
use tempfile::TempDir;
struct Fixture {
    owner: Option<OperationJournalOwner>,
    guard: Option<WritableProfileGuard>,
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().unwrap();
        let config = ConfigBaseGuard::set(std::fs::canonicalize(base.path()).unwrap());
        let profile = PersistenceProfileGuard::named("frozen-copy-journal");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        Self {
            owner: None,
            guard: Some(guard),
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
    fn start(&mut self, store: OperationJournalStore) {
        let owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match owner.status() {
                OperationJournalOwnerStatus::Available => break,
                OperationJournalOwnerStatus::Unavailable { reason } => panic!("{reason}"),
                _ => {
                    assert!(std::time::Instant::now() < end);
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        self.owner = Some(owner);
    }
    fn client(&self) -> OperationJournalClient {
        self.owner.as_ref().unwrap().client()
    }
    fn read(
        &self,
        op: OperationId,
        key: WorkUnitKey,
    ) -> Result<Option<FrozenCopyObservation>, OperationJournalOwnerError> {
        self.client()
            .observe_copy_intent(op, key)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
    }
    fn close(&mut self) {
        if let Some(mut owner) = self.owner.take() {
            assert!(owner.shutdown());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.close();
    }
}
fn snapshot() -> crate::operation_journal::work_progress::FullProgressSnapshotObservation {
    crate::operation_journal::work_progress::test_full_progress_store_snapshots().0
}
fn key(
    snapshot: &crate::operation_journal::work_progress::FullProgressSnapshotObservation,
) -> WorkUnitKey {
    snapshot
        .intent()
        .plan()
        .units()
        .iter()
        .find(|u| u.key().role() == WorkUnitRole::Harvest)
        .unwrap()
        .key()
}
fn legacy(
    snapshot: &crate::operation_journal::work_progress::FullProgressSnapshotObservation,
) -> StoredOperationRecord {
    let i = snapshot.intent();
    StoredOperationRecord::new_v16_command(
        i.operation_id(),
        OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::FileMutation,
            label: "copy metadata fixture".into(),
        },
        UniversalIntent::new(
            None,
            UniversalCommandKind::Copy,
            "operation.status.copy",
            i.participants().clone(),
        )
        .unwrap(),
        i.args().clone(),
        i.capacity_plan().clone(),
        1,
    )
}
#[test]
fn frozen_copy_owner_reads_real_full_and_initial_records_and_revalidates() {
    for full in [false, true] {
        let mut f = Fixture::new();
        let s = snapshot();
        let op = s.intent().operation_id();
        let k = key(&s);
        let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
        if full {
            store
                .persist_full(&StoredFullProgressRecord::new(s.clone()).unwrap(), None)
                .unwrap();
        } else {
            store.insert_intent(&legacy(&s)).unwrap();
        }
        f.start(store);
        let observation = f.read(op, k).unwrap().unwrap();
        assert_eq!(observation.operation(), op);
        assert_eq!(observation.command(), s.intent().args());
        assert_eq!(observation.participants(), s.intent().participants());
        assert_eq!(observation.work_plan(), s.intent().plan());
        assert_eq!(observation.harvest_key(), k);
        assert_eq!(
            observation.profile_identity(),
            f.guard.as_ref().unwrap().profile_root_identity()
        );
        assert!(!observation.database_identity().is_empty());
        assert!(observation.canonical_bytes().unwrap().len() <= 65536);
        observation.validate().unwrap();
        let again = f.read(op, k).unwrap().unwrap();
        assert_eq!(
            again.canonical_bytes().unwrap(),
            observation.canonical_bytes().unwrap()
        );
        assert!(f.read(OperationId::new(), k).unwrap().is_none());
        let wrong = WorkUnitKey::new(
            k.action_ordinal(),
            k.step_ordinal() + 1,
            k.participant_id(),
            WorkUnitRole::Harvest,
        );
        assert!(f.read(op, wrong).is_err());
        f.close();
        assert!(matches!(
            observation.validate(),
            Err(FrozenCopyValidationError::OwnerEnded)
        ));
    }
}
#[test]
fn frozen_copy_owner_does_not_retain_profile_lock_and_reopen_requires_fresh_observation() {
    let mut f = Fixture::new();
    let s = snapshot();
    let op = s.intent().operation_id();
    let k = key(&s);
    let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
    store
        .persist_full(&StoredFullProgressRecord::new(s).unwrap(), None)
        .unwrap();
    f.start(store);
    let old = f.read(op, k).unwrap().unwrap();
    let historical = old.canonical_bytes().unwrap();
    f.guard.take();
    f.close();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    f.guard = Some(guard);
    f.start(store);
    assert!(matches!(
        old.validate(),
        Err(FrozenCopyValidationError::OwnerEnded)
    ));
    let fresh = f.read(op, k).unwrap().unwrap();
    fresh.validate().unwrap();
    assert_eq!(fresh.canonical_bytes().unwrap(), historical);
}
#[test]
fn frozen_copy_owner_bounds_malformed_oversized_and_ambiguous_storage() {
    for fault in [
        "oversized",
        "malformed",
        "ambiguous",
        "unsupported",
        "noncanonical",
        "text",
    ] {
        let mut f = Fixture::new();
        let s = snapshot();
        let op = s.intent().operation_id();
        let k = key(&s);
        let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
        store
            .persist_full(&StoredFullProgressRecord::new(s.clone()).unwrap(), None)
            .unwrap();
        // This fixture injects storage damage before handing the sole store to its real owner.
        let path = f
            .guard
            .as_ref()
            .unwrap()
            .profile_root()
            .join(crate::operation_journal::store::JOURNAL_DATABASE_ROOT_NAME)
            .join(crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME);
        drop(store);
        let c = rusqlite::Connection::open(path).unwrap();
        match fault {
            "oversized" => {
                c.execute(
                    "UPDATE operation_journal_full_progress SET record=zeroblob(1000000)",
                    [],
                )
                .unwrap();
            }
            "malformed" => {
                c.execute(
                    "UPDATE operation_journal_full_progress SET record=?1",
                    [b"{}".as_slice()],
                )
                .unwrap();
            }
            "unsupported" => {
                let mut value =
                    serde_json::to_value(StoredFullProgressRecord::new(s.clone()).unwrap())
                        .unwrap();
                value["schema_version"] = serde_json::json!(999);
                let bytes = serde_json::to_vec(&value).unwrap();
                c.execute(
                    "UPDATE operation_journal_full_progress SET record=?1",
                    [bytes],
                )
                .unwrap();
            }
            "noncanonical" => {
                let mut bytes = crate::operation_journal::full_progress_codec::encode(
                    &StoredFullProgressRecord::new(s.clone()).unwrap(),
                )
                .unwrap();
                bytes.push(b' ');
                c.execute(
                    "UPDATE operation_journal_full_progress SET record=?1",
                    [bytes],
                )
                .unwrap();
            }
            "text" => {
                c.execute(
                    "UPDATE operation_journal_full_progress SET record=?1",
                    ["x".repeat(1000000)],
                )
                .unwrap();
            }
            _ => {
                c.execute_batch("DROP INDEX IF EXISTS idx_full_progress_sequence;ALTER TABLE operation_journal_full_progress RENAME TO saved_full;CREATE TABLE operation_journal_full_progress AS SELECT * FROM saved_full;INSERT INTO operation_journal_full_progress SELECT * FROM saved_full;").unwrap();
            }
        }
        drop(c);
        // The operational opener may reject malformed schema itself; that is also fail closed.
        match OperationJournalStore::open(f.guard.as_ref().unwrap()) {
            Ok(store) => {
                f.start(store);
                assert!(f.read(op, k).is_err());
            }
            Err(_) => assert_eq!(fault, "ambiguous"),
        }
    }
}
#[test]
fn frozen_copy_owner_rejects_cross_family_ambiguity_and_wrong_input() {
    let mut f = Fixture::new();
    let s = snapshot();
    let op = s.intent().operation_id();
    let k = key(&s);
    let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
    store
        .persist_full(&StoredFullProgressRecord::new(s.clone()).unwrap(), None)
        .unwrap();
    let path = f
        .guard
        .as_ref()
        .unwrap()
        .profile_root()
        .join(crate::operation_journal::store::JOURNAL_DATABASE_ROOT_NAME)
        .join(crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME);
    drop(store);
    let c = rusqlite::Connection::open(path).unwrap();
    let bytes = crate::operation_journal::codec::encode(&legacy(&s)).unwrap();
    c.execute(
        "INSERT INTO operation_journal(operation_id,sequence,record) VALUES(?1,2,?2)",
        rusqlite::params![op.to_string(), bytes],
    )
    .unwrap();
    drop(c);
    let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
    f.start(store);
    assert!(f.read(op, k).is_err());
    let invalid = WorkUnitKey::new(0, 1, k.participant_id(), WorkUnitRole::GlobalLibrary);
    assert!(f.client().observe_copy_intent(op, invalid).is_err());
}
#[cfg(unix)]
#[test]
fn frozen_copy_owner_revalidates_real_profile_and_journal_database_replacement() {
    for replace_database in [false, true] {
        let mut f = Fixture::new();
        let s = snapshot();
        let op = s.intent().operation_id();
        let k = key(&s);
        let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
        store
            .persist_full(&StoredFullProgressRecord::new(s).unwrap(), None)
            .unwrap();
        let db_path = f
            .guard
            .as_ref()
            .unwrap()
            .profile_root()
            .join(crate::operation_journal::store::JOURNAL_DATABASE_ROOT_NAME)
            .join(crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME);
        f.start(store);
        let observation = f.read(op, k).unwrap().unwrap();
        if replace_database {
            std::fs::rename(&db_path, db_path.with_extension("held")).unwrap();
            std::fs::write(&db_path, b"replacement").unwrap();
        } else {
            let path = f.guard.as_ref().unwrap().profile_root().to_path_buf();
            let held = path.with_extension("held");
            std::fs::rename(&path, held).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        assert!(observation.validate().is_err());
        if let Some(mut owner) = f.owner.take() {
            let _ = owner.shutdown();
        }
    }
}

#[test]
fn frozen_copy_owner_rejects_valid_canceled_terminal_records() {
    let mut rejected = 0;
    for s in crate::operation_journal::work_progress::test_guarded_terminal_store_snapshots() {
        if s.cancellation_request().is_none() {
            continue;
        }
        let mut f = Fixture::new();
        let op = s.intent().operation_id();
        let k = key(&s);
        let store = OperationJournalStore::open(f.guard.as_ref().unwrap()).unwrap();
        store
            .persist_full(&StoredFullProgressRecord::new(s).unwrap(), None)
            .unwrap();
        f.start(store);
        assert!(f.read(op, k).is_err());
        rejected += 1;
    }
    assert!(rejected > 0);
}
