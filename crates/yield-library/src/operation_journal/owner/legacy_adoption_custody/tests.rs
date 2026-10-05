//! Actual readonly source/journal/queue ownership. Capacity bootstrap is deliberately absent.
use super::*;
use crate::{
    app_dirs::AppRootGuard,
    sample_sources::{DB_FILE_NAME, SourceDatabase},
    test_runtime::TestRuntimeGuard,
};
use std::{fs, path::PathBuf, time::Instant};
use tempfile::TempDir;

struct Fixture {
    baseline: InputBudget,
    owner: Option<OperationJournalOwner>,
    source: Option<LegacySourceReadOwner>,
    guard: WritableProfileGuard,
    _root: AppRootGuard,
    profile: TempDir,
    media: TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let profile = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_owned()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let mut f = Self {
            baseline: InputBudget::new(),
            owner: None,
            source: None,
            guard,
            _root: root,
            profile,
            media,
            _runtime: runtime,
        };
        f.source = Some(f.open_source("legacy"));
        let store = OperationJournalStore::open(&f.guard).unwrap();
        f.owner = Some(OperationJournalOwner::start_without_capacity_for_test(store).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while f.owner.as_ref().unwrap().status() != OperationJournalOwnerStatus::Available {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        f.baseline = *f.client().shared.input_bytes.lock().unwrap();
        f
    }
    fn open_source(&self, child: &str) -> LegacySourceReadOwner {
        let dir = self.profile.path().join(child);
        fs::create_dir(&dir).unwrap();
        rusqlite::Connection::open(dir.join(DB_FILE_NAME))
            .unwrap()
            .execute_batch(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/profile-v11/source.sql"
            )))
            .unwrap();
        SourceDatabase::open_legacy_read_owner(self.media.path(), &self.guard, child).unwrap()
    }
    fn client(&self) -> OperationJournalClient {
        self.owner.as_ref().unwrap().client()
    }
    fn path(&self) -> PathBuf {
        self.profile.path().join("legacy").join(DB_FILE_NAME)
    }
    fn prepare(&mut self) -> Result<LegacyAdoptionCustody, OperationJournalOwnerError> {
        self.client()
            .prepare_legacy_adoption_custody(
                self.source.take().unwrap(),
                ParticipantId::from_uuid(Uuid::from_u128(11)),
                SourceCommandId::from_uuid(Uuid::from_u128(12)),
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
    }
    fn close(&mut self) {
        if let Some(mut owner) = self.owner.take() {
            let clean = owner.shutdown();
            assert!(clean || thread::panicking());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.close();
    }
}
fn budget(client: &OperationJournalClient) -> InputBudget {
    *client.shared.input_bytes.lock().unwrap()
}
fn validate(custody: &LegacyAdoptionCustody) -> Result<(), OperationJournalOwnerError> {
    custody
        .revalidate()
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
}
fn journal_bytes(f: &Fixture) -> Vec<u8> {
    fs::read(
        f.profile
            .path()
            .join(super::super::super::store::JOURNAL_DATABASE_ROOT_NAME)
            .join(super::super::super::store::JOURNAL_DATABASE_FILE_NAME),
    )
    .unwrap()
}

#[test]
fn legacy_custody_actual_worker_captures_and_revalidates_without_writes_or_lease() {
    let mut f = Fixture::new();
    let source_bytes = fs::read(f.path()).unwrap();
    let journal = journal_bytes(&f);
    let custody = f.prepare().unwrap();
    assert_eq!(
        custody.input().profile_owner_identity(),
        f.guard.owner_identity()
    );
    assert_eq!(custody.input().original_revision(), 7);
    for _ in 0..3 {
        validate(&custody).unwrap();
    }
    assert_eq!(fs::read(f.path()).unwrap(), source_bytes);
    assert_eq!(journal_bytes(&f), journal);
    let connection = rusqlite::Connection::open(f.path()).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='source_lease'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        budget(&f.client()).remaining[0],
        f.baseline.remaining[0] - INPUT_BYTES
    );
    f.close();
    assert!(custody.revalidate().is_err());
}

#[test]
fn legacy_custody_reply_loss_retains_original_lock_and_metadata_credit_until_shutdown() {
    let mut f = Fixture::new();
    let client = f.client();
    let request = client
        .prepare_legacy_adoption_custody(
            f.source.take().unwrap(),
            ParticipantId::from_uuid(Uuid::from_u128(11)),
            SourceCommandId::from_uuid(Uuid::from_u128(12)),
        )
        .unwrap();
    drop(request);
    let other = f.open_source("other");
    let failure = client
        .prepare_legacy_adoption_custody(
            other,
            ParticipantId::from_uuid(Uuid::from_u128(13)),
            SourceCommandId::from_uuid(Uuid::from_u128(14)),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(matches!(
        failure,
        Err(OperationJournalOwnerError::LegacyAdoptionCustody(
            LegacyAdoptionCustodyError::Busy
        ))
    ));
    assert_eq!(
        budget(&client).remaining[0],
        f.baseline.remaining[0] - INPUT_BYTES
    );
    let sidecar = f
        .profile
        .path()
        .join("legacy")
        .join(".yield-source-lease.lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&sidecar)
        .unwrap();
    assert!(file.try_lock().is_err());
    f.close();
    file.try_lock().unwrap();
    assert!(sidecar.exists());
    assert_eq!(budget(&client), InputBudget::new());
}

#[test]
fn legacy_custody_changed_source_rows_refuse_and_retain_slot_without_repair() {
    let mut f = Fixture::new();
    let custody = f.prepare().unwrap();
    let journal = journal_bytes(&f);
    let connection = rusqlite::Connection::open(f.path()).unwrap();
    connection
        .execute("UPDATE metadata SET value='8' WHERE key='revision'", [])
        .unwrap();
    // Fixture key is checked below rather than assuming this external change took effect.
    assert_eq!(
        connection
            .query_row("SELECT value FROM metadata WHERE key='revision'", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "8"
    );
    drop(connection);
    let changed = fs::read(f.path()).unwrap();
    assert!(matches!(
        validate(&custody),
        Err(OperationJournalOwnerError::LegacyAdoptionCustody(
            LegacyAdoptionCustodyError::Changed
        ))
    ));
    assert_eq!(fs::read(f.path()).unwrap(), changed);
    assert_eq!(journal_bytes(&f), journal);
    assert_eq!(
        budget(&f.client()).remaining[0],
        f.baseline.remaining[0] - INPUT_BYTES
    );
}

#[cfg(unix)]
#[test]
fn legacy_custody_original_directory_replacement_refuses_without_touching_substitute() {
    let mut f = Fixture::new();
    let custody = f.prepare().unwrap();
    let original = f.profile.path().join("legacy");
    fs::rename(&original, f.profile.path().join("retained-original")).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(
        original.join(DB_FILE_NAME),
        b"substitute must remain untouched",
    )
    .unwrap();
    assert!(validate(&custody).is_err());
    assert_eq!(
        fs::read(original.join(DB_FILE_NAME)).unwrap(),
        b"substitute must remain untouched"
    );
}

#[test]
fn legacy_custody_foreign_profile_rejection_refunds_and_preserves_both_originals() {
    let mut f = Fixture::new();
    let first_bytes = fs::read(f.path()).unwrap();
    {
        let mut foreign = Fixture::new();
        let foreign_bytes = fs::read(foreign.path()).unwrap();
        let result = f
            .client()
            .prepare_legacy_adoption_custody(
                foreign.source.take().unwrap(),
                ParticipantId::from_uuid(Uuid::from_u128(13)),
                SourceCommandId::from_uuid(Uuid::from_u128(14)),
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            result,
            Err(OperationJournalOwnerError::LegacyAdoptionCustody(
                LegacyAdoptionCustodyError::Preparation(_)
            ))
        ));
        assert_eq!(fs::read(foreign.path()).unwrap(), foreign_bytes);
    }
    assert_eq!(fs::read(f.path()).unwrap(), first_bytes);
    assert_eq!(budget(&f.client()), f.baseline);
    validate(&f.prepare().unwrap()).unwrap();
}

#[test]
fn legacy_custody_queue_credit_refusal_preserves_source_and_does_not_install_slot() {
    let mut f = Fixture::new();
    let client = f.client();
    let source_bytes = fs::read(f.path()).unwrap();
    let debit = client
        .shared
        .reserve_input(InputClass::Ordinary, f.baseline.remaining[0])
        .unwrap();
    let result = client.prepare_legacy_adoption_custody(
        f.source.take().unwrap(),
        ParticipantId::from_uuid(Uuid::from_u128(11)),
        SourceCommandId::from_uuid(Uuid::from_u128(12)),
    );
    assert!(matches!(
        result,
        Err(OperationJournalOwnerQueueError::InputBytesFull { .. })
    ));
    assert_eq!(fs::read(f.path()).unwrap(), source_bytes);
    drop(debit);
    assert_eq!(budget(&client), f.baseline);
    let other = f.open_source("other");
    let custody = client
        .prepare_legacy_adoption_custody(
            other,
            ParticipantId::from_uuid(Uuid::from_u128(13)),
            SourceCommandId::from_uuid(Uuid::from_u128(14)),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    validate(&custody).unwrap();
}
