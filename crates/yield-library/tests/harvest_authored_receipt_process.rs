//! Genuine normal-build authored metadata producer and historical Global-only process recovery.
#![cfg(not(feature = "test-support"))]
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
use yield_library::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    operation_journal::{OperationId, StoredRelativePath},
    sample_sources::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus, SourceDatabase, SourceId, SourceWriter,
        library::{
            HarvestAuthoredDisposition, HarvestAuthoredReceiptKey,
            HarvestAuthoredReceiptObservation, HarvestAuthoredRequest, HarvestAuthoredResult,
            HarvestAuthoredSnapshotRequest, HarvestState, LIBRARY_DB_FILE_NAME,
        },
    },
};
const PROFILE_ENV: &str = "YIELD_AUTHORED_RECEIPT_PROCESS_PROFILE";
const READY_ENV: &str = "YIELD_AUTHORED_RECEIPT_PROCESS_READY";
const PRODUCER_ENV: &str = "YIELD_AUTHORED_RECEIPT_PRODUCER";
const PROCESS_WAIT: Duration = Duration::from_secs(120);
const REPLY_WAIT: Duration = Duration::from_secs(5);
const EVIDENCE_LIMIT: u64 = 4096;

// Encoded scalar observations only. These values can never construct a live request/receipt.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    operation: OperationId,
    sequence: u64,
    historical_owner: String,
    applied: bool,
    requested_state: String,
    proposal_identity: [u8; 32],
}
impl Evidence {
    fn observed(o: HarvestAuthoredReceiptObservation) -> Self {
        Self {
            operation: o.operation(),
            sequence: o.sequence(),
            historical_owner: o.owner_identity().to_string(),
            applied: o.disposition() == HarvestAuthoredDisposition::Applied,
            requested_state: o.requested_state().as_str().into(),
            proposal_identity: *o.proposal_identity(),
        }
    }
    fn key(&self) -> HarvestAuthoredReceiptKey {
        HarvestAuthoredReceiptKey::new(self.operation).unwrap()
    }
}
fn missing_key() -> HarvestAuthoredReceiptKey {
    HarvestAuthoredReceiptKey::new(OperationId::from_uuid(Uuid::from_u128(2358))).unwrap()
}
fn start(guard: &WritableProfileGuard) -> GlobalLibraryWriter {
    let writer = GlobalLibraryWriter::start(guard).unwrap();
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        match writer.status() {
            GlobalLibraryWriterStatus::Available => return writer,
            GlobalLibraryWriterStatus::Initializing => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            status => panic!("actual global startup failed: {status:?}"),
        }
    }
}
fn observe(
    writer: &GlobalLibraryWriter,
    key: HarvestAuthoredReceiptKey,
) -> Option<HarvestAuthoredReceiptObservation> {
    writer
        .observe_harvest_authored_receipt(key)
        .unwrap()
        .recv_timeout(REPLY_WAIT)
        .unwrap()
        .unwrap()
}
struct OwnedChild {
    child: Child,
    reaped: bool,
}
impl OwnedChild {
    fn kill_and_reap(&mut self) {
        assert!(self.child.try_wait().unwrap().is_none());
        self.child.kill().unwrap();
        let deadline = Instant::now() + PROCESS_WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = true;
                assert!(!status.success());
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let deadline = Instant::now() + PROCESS_WAIT;
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
fn process_case(producer: bool) {
    let base = tempfile::tempdir().unwrap();
    // Native profile ownership refuses symlinked ancestors, including macOS's /var alias.
    let base_path = fs::canonicalize(base.path()).expect("canonical disposable fixture root");
    let profile = base_path.join("profile");
    let ready = base_path.join("ready.json");
    let _root = AppRootGuard::set(profile.clone()).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "harvest_authored_receipt_process_child",
            "--ignored",
            "--nocapture",
        ])
        .env(PROFILE_ENV, &profile)
        .env(READY_ENV, &ready)
        .env(PRODUCER_ENV, if producer { "1" } else { "0" })
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let mut child = OwnedChild {
        child: command.spawn().unwrap(),
        reaped: false,
    };
    let deadline = Instant::now() + PROCESS_WAIT;
    while !ready.exists() {
        assert!(
            child.child.try_wait().unwrap().is_none(),
            "owned child failed before actual lookup"
        );
        assert!(
            Instant::now() < deadline,
            "original producer/reader readiness timeout"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let mut bytes = Vec::new();
    fs::File::open(&ready)
        .unwrap()
        .take(EVIDENCE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() as u64 <= EVIDENCE_LIMIT);
    let evidence: Option<Evidence> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(evidence.is_some(), producer);
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    let source_bytes =
        producer.then(|| fs::read(ready.with_extension("source").join("one.wav")).unwrap());
    child.kill_and_reap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = start(&guard);
    if let Some(e) = evidence {
        assert!(e.applied);
        assert_ne!(e.historical_owner, guard.owner_identity());
        let before = database_inventory(&profile);
        assert_eq!(Evidence::observed(observe(&writer, e.key()).unwrap()), e);
        assert_eq!(Evidence::observed(observe(&writer, e.key()).unwrap()), e);
        assert!(observe(&writer, missing_key()).is_none());
        assert_eq!(
            database_inventory(&profile),
            before,
            "historical read preserves metadata and immutable receipt"
        );
        assert_eq!(before, (1, 0, 2, "done".into()));
    } else {
        assert!(observe(&writer, missing_key()).is_none());
        assert_eq!(database_inventory(&profile), (0, 0, 0, "".into()));
    }
    if let Some(bytes) = source_bytes {
        assert_eq!(bytes, b"abc");
        assert_eq!(
            fs::read(ready.with_extension("source").join("one.wav")).unwrap(),
            bytes
        );
    }
    writer.shutdown();
}
fn database_inventory(profile: &Path) -> (u64, u64, u64, String) {
    let c = Connection::open_with_flags(
        profile.join(LIBRARY_DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    c.query_row("SELECT (SELECT count(*) FROM harvest_files),(SELECT count(*) FROM harvest_derivations),(SELECT count(*) FROM harvest_authored_receipts),COALESCE((SELECT harvest_state FROM harvest_files LIMIT 1),'')",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
}
#[test]
fn normal_build_historical_lookup_missing_survives_owned_process_death() {
    process_case(false);
}
#[test]
fn normal_build_committed_authored_receipt_survives_owned_process_death() {
    process_case(true);
}

#[test]
#[ignore = "owned subprocess helper, invoked only by the parent"]
fn harvest_authored_receipt_process_child() {
    let profile = PathBuf::from(std::env::var_os(PROFILE_ENV).unwrap());
    let ready = PathBuf::from(std::env::var_os(READY_ENV).unwrap());
    let producer = std::env::var(PRODUCER_ENV).unwrap() == "1";
    let _root = AppRootGuard::set(profile).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let writer = start(&guard);
    let (evidence, _custody) = if producer {
        let (o, s) = produce(&guard, &writer, &ready.with_extension("source"));
        (Some(Evidence::observed(o)), Some(s))
    } else {
        assert!(observe(&writer, missing_key()).is_none());
        (None, None)
    };
    let bytes = serde_json::to_vec(&evidence).unwrap();
    assert!(bytes.len() as u64 <= EVIDENCE_LIMIT);
    let preparing = ready.with_extension("tmp");
    fs::write(&preparing, bytes).unwrap();
    fs::rename(preparing, ready).unwrap();
    loop {
        thread::sleep(Duration::from_millis(10));
    }
}

fn produce(
    guard: &WritableProfileGuard,
    global: &GlobalLibraryWriter,
    root: &Path,
) -> (HarvestAuthoredReceiptObservation, SourceWriter) {
    fs::create_dir(root).unwrap();
    let file_path = root.join("one.wav");
    fs::write(&file_path, b"abc").unwrap();
    let mut held = fs::File::open(&file_path).unwrap();
    let identity =
        yield_library::filesystem_identity::stable_filesystem_identity_from_open_file(&held)
            .unwrap();
    let metadata = held.metadata().unwrap();
    let modified: i64 = metadata
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .try_into()
        .unwrap();
    let mut bytes = Vec::new();
    held.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"abc");
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(root, guard, Path::new("origin"))
        .unwrap();
    let source = SourceWriter::new_for_scan(db, SourceId::from_string("origin"), 1).unwrap();
    let fence = source.current_scan_fence().unwrap();
    let mut batch = source.catalog_database().write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new("one.wav"), metadata.len(), modified, &hash)
        .unwrap();
    batch
        .set_file_identity(Path::new("one.wav"), Some(&identity))
        .unwrap();
    source.commit_scan_manifest_batch(&fence, batch).unwrap();
    let generation = source.begin_scan_directory_truth_generation(0).unwrap();
    let fence = source.current_scan_fence().unwrap();
    let batch = source.catalog_database().write_batch().unwrap();
    source
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            1,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    let request = |state| {
        let content = source
            .observe_file_identity(StoredRelativePath::new("one.wav").unwrap())
            .unwrap()
            .into_content_identity()
            .unwrap();
        let snapshot = global
            .harvest_authored_snapshot(
                HarvestAuthoredSnapshotRequest::new(guard, content.clone()).unwrap(),
            )
            .unwrap()
            .recv_timeout(REPLY_WAIT)
            .unwrap()
            .unwrap();
        HarvestAuthoredRequest::new(guard, OperationId::new(), snapshot, content, state).unwrap()
    };
    let original = request(HarvestState::Seen);
    let key = HarvestAuthoredReceiptKey::new(original.operation()).unwrap();
    let digest = *blake3::hash(original.proposal_bytes()).as_bytes();
    drop(global.set_harvest_authored_state(original.clone()).unwrap());
    let observation = observe(global, key).unwrap();
    assert_eq!(
        observation.disposition(),
        HarvestAuthoredDisposition::Applied
    );
    assert_eq!(observation.requested_state(), HarvestState::Seen);
    assert_eq!(observation.proposal_identity(), &digest);
    assert!(matches!(
        global
            .set_harvest_authored_state(request(HarvestState::Done))
            .unwrap()
            .recv_timeout(REPLY_WAIT)
            .unwrap()
            .unwrap(),
        HarvestAuthoredResult::Committed(_)
    ));
    assert!(matches!(
        global
            .set_harvest_authored_state(original)
            .unwrap()
            .recv_timeout(REPLY_WAIT)
            .unwrap()
            .unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert_eq!(observe(global, key), Some(observation));
    assert_eq!(fs::read(&file_path).unwrap(), b"abc");
    (observation, source)
}
