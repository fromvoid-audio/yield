//! Normal-build original-global historical lookup; native producer execution is separate.
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
    operation_journal::{
        OperationId, ParticipantId,
        work_plan::{WorkUnitKey, WorkUnitRole},
    },
    sample_sources::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus,
        library::{
            HarvestCopyDisposition, HarvestCopyReceiptKey, HarvestCopyReceiptObservation,
            LIBRARY_DB_FILE_NAME,
        },
    },
};
#[path = "harvest_receipt_process/native_producer.rs"]
mod native_producer;
const PROFILE_ENV: &str = "YIELD_HARVEST_RECEIPT_PROCESS_PROFILE";
const READY_ENV: &str = "YIELD_HARVEST_RECEIPT_PROCESS_READY";
const PRODUCER_ENV: &str = "YIELD_HARVEST_RECEIPT_NATIVE_PRODUCER";
const PROCESS_WAIT: Duration = Duration::from_secs(120);
const REPLY_WAIT: Duration = Duration::from_secs(5);
const EVIDENCE_LIMIT: u64 = 4096;

// Encoded scalar observations only. These values can never construct a live request/receipt.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    operation: OperationId,
    work: WorkUnitKey,
    sequence: u64,
    historical_owner: String,
    applied: bool,
    edge: Option<u64>,
    destination_revision: u64,
    proposal_identity: [u8; 32],
}
impl Evidence {
    fn observed(o: HarvestCopyReceiptObservation) -> Self {
        Self {
            operation: o.operation(),
            work: o.work_key(),
            sequence: o.sequence(),
            historical_owner: o.owner_identity().to_string(),
            applied: o.disposition() == HarvestCopyDisposition::Applied,
            edge: o.edge_id(),
            destination_revision: o.destination_revision(),
            proposal_identity: *o.proposal_identity(),
        }
    }
    fn key(&self) -> HarvestCopyReceiptKey {
        HarvestCopyReceiptKey::new(self.operation, self.work).unwrap()
    }
}
fn missing_key() -> HarvestCopyReceiptKey {
    HarvestCopyReceiptKey::new(
        OperationId::from_uuid(Uuid::from_u128(2348)),
        WorkUnitKey::new(
            255,
            1,
            ParticipantId::from_uuid(Uuid::from_u128(2348)),
            WorkUnitRole::Harvest,
        ),
    )
    .unwrap()
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
    key: HarvestCopyReceiptKey,
) -> Option<HarvestCopyReceiptObservation> {
    writer
        .observe_harvest_copy_receipt(key)
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
    let profile = base.path().join("profile");
    let ready = base.path().join("ready.json");
    let _root = AppRootGuard::set(profile.clone()).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "harvest_receipt_process_child",
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
        assert_eq!(before, (2, 1, 1));
    } else {
        assert!(observe(&writer, missing_key()).is_none());
        assert_eq!(database_inventory(&profile), (0, 0, 0));
    }
    writer.shutdown();
}
fn database_inventory(profile: &Path) -> (u64, u64, u64) {
    let c = Connection::open_with_flags(
        profile.join(LIBRARY_DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    c.query_row("SELECT (SELECT count(*) FROM harvest_files),(SELECT count(*) FROM harvest_derivations),(SELECT count(*) FROM harvest_copy_receipts)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap()
}
#[test]
fn normal_build_historical_lookup_missing_survives_owned_process_death() {
    process_case(false);
}
#[cfg(target_os = "macos")]
#[test]
fn native_committed_harvest_receipt_survives_owned_process_death() {
    process_case(true);
}

#[test]
#[ignore = "owned subprocess helper, invoked only by the parent"]
fn harvest_receipt_process_child() {
    let profile = PathBuf::from(std::env::var_os(PROFILE_ENV).unwrap());
    let ready = PathBuf::from(std::env::var_os(READY_ENV).unwrap());
    let producer = std::env::var(PRODUCER_ENV).unwrap() == "1";
    let _root = AppRootGuard::set(profile).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let writer = start(&guard);
    let (evidence, _custody) = if producer {
        let (o, s, j) = native_producer::produce(&guard, &writer);
        (Some(Evidence::observed(o)), Some((s, j)))
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
