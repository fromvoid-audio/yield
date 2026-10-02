//! Normal-build registry receipt persistence across owned process death (OPT-2146).

#![cfg(not(feature = "test-support"))]

use std::{
    fs,
    io::Read,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use yield_library::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    operation_journal::{
        OperationId, ParticipantId,
        work_plan::{WorkUnitKey, WorkUnitRole},
    },
    sample_sources::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus, LibraryError, SampleSource, SourceId,
        SourceRegistrySnapshot,
        library::{
            LIBRARY_DB_FILE_NAME, SourceRegistryParticipantDisposition,
            SourceRegistryParticipantReceipt, SourceRegistryParticipantRequest,
        },
    },
};

const PROFILE_ENV: &str = "YIELD_REGISTRY_PROCESS_FIXTURE_PROFILE";
const READY_ENV: &str = "YIELD_REGISTRY_PROCESS_FIXTURE_READY";
const REPLY_WAIT: Duration = Duration::from_secs(5);
const PROCESS_WAIT: Duration = Duration::from_secs(30);
const EVIDENCE_LIMIT: u64 = 4096;

// Serialized observations of an actual owner reply, never a receipt constructor.
#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptEvidence {
    operation: OperationId,
    work: WorkUnitKey,
    owner: String,
    payload: [u8; 32],
    sequence: u64,
    disposition: String,
}

impl ReceiptEvidence {
    fn observed(receipt: &SourceRegistryParticipantReceipt) -> Self {
        Self {
            operation: receipt.operation_id(),
            work: receipt.key(),
            owner: receipt.owner_identity().to_string(),
            payload: *receipt.payload_identity(),
            sequence: receipt.sequence(),
            disposition: match receipt.disposition() {
                SourceRegistryParticipantDisposition::Applied => "applied",
                SourceRegistryParticipantDisposition::RegistryChanged => "registry_changed",
            }
            .into(),
        }
    }
}

fn work(ordinal: u16) -> WorkUnitKey {
    WorkUnitKey::new(
        u8::MAX,
        ordinal,
        ParticipantId::from_uuid(Uuid::from_u128(2146)),
        WorkUnitRole::GlobalLibrary,
    )
}

fn replacement(profile: &Path, later: bool) -> SourceRegistrySnapshot {
    let id = if later { "later" } else { "original" };
    SourceRegistrySnapshot::new(vec![SampleSource::new_with_id(
        SourceId::from_string(id),
        profile.join(id),
    )])
}

fn request(
    owner: &str,
    profile: &Path,
    later: bool,
    ordinal: u16,
) -> SourceRegistryParticipantRequest {
    SourceRegistryParticipantRequest::new(
        owner,
        OperationId::from_uuid(Uuid::from_u128(2146)),
        work(ordinal),
        &SourceRegistrySnapshot::new(vec![]),
        replacement(profile, later),
    )
    .expect("bounded frozen registry proposal")
}

fn start(guard: &WritableProfileGuard) -> GlobalLibraryWriter {
    let writer = GlobalLibraryWriter::start(guard).expect("actual profile-owned global writer");
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        match writer.status() {
            GlobalLibraryWriterStatus::Available => return writer,
            GlobalLibraryWriterStatus::Initializing => {
                assert!(Instant::now() < deadline, "writer readiness timeout");
                thread::sleep(Duration::from_millis(5));
            }
            status => panic!("writer unavailable: {status:?}"),
        }
    }
}

fn apply(
    writer: &GlobalLibraryWriter,
    request: SourceRegistryParticipantRequest,
) -> Result<SourceRegistryParticipantReceipt, LibraryError> {
    writer
        .apply_source_registry_participant(request)
        .expect("participant queued")
        .recv_timeout(REPLY_WAIT)
        .expect("bounded participant reply")
}

fn assert_registry(writer: &GlobalLibraryWriter, profile: &Path, later: bool) {
    let snapshot = writer
        .load_source_registry_snapshot()
        .expect("registry read queued")
        .recv_timeout(REPLY_WAIT)
        .expect("bounded registry reply")
        .expect("registry read");
    let expected = replacement(profile, later);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot.as_slice()[0].id, expected.as_slice()[0].id);
    assert_eq!(snapshot.as_slice()[0].root, expected.as_slice()[0].root);
}

fn assert_persisted(profile: &Path, evidence: &ReceiptEvidence, later: bool) {
    let connection = Connection::open_with_flags(
        profile.join(LIBRARY_DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only persisted evidence");
    connection.busy_timeout(REPLY_WAIT).unwrap();
    let count: u64 = connection
        .query_row(
            "SELECT count(*) FROM (SELECT 1 FROM source_registry_participant_receipts LIMIT 2)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "one receipt retained, including after exact replay"
    );
    let stored = connection
        .query_row(
            "SELECT operation_id, work_key, owner_identity, payload_identity, sequence, disposition
             FROM source_registry_participant_receipts
             WHERE length(operation_id)=36 AND length(CAST(work_key AS BLOB))<=256
             AND length(owner_identity)=36 AND length(payload_identity)=32
             AND length(disposition)<=16 LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(stored.0, evidence.operation.to_string());
    assert_eq!(
        serde_json::from_str::<WorkUnitKey>(&stored.1).unwrap(),
        evidence.work
    );
    assert_eq!(stored.2, evidence.owner);
    assert_eq!(stored.3, evidence.payload);
    assert_eq!(stored.4, evidence.sequence);
    assert_eq!(stored.5, evidence.disposition);
    let mut statement = connection.prepare(
        "SELECT id, root FROM sources WHERE length(id)<=32 AND length(CAST(root AS BLOB))<=4096 LIMIT 3"
    ).unwrap();
    let sources = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let expected = replacement(profile, later);
    assert_eq!(
        sources,
        vec![(
            expected.as_slice()[0].id.as_str().into(),
            expected.as_slice()[0].root.to_str().unwrap().into()
        )]
    );
}

struct OwnedChild {
    child: Child,
    reaped: bool,
}

impl OwnedChild {
    fn kill_and_reap(&mut self) -> ExitStatus {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "child exited before crash injection"
        );
        self.child
            .kill()
            .expect("kill only the fixture-owned child");
        let deadline = Instant::now() + PROCESS_WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll owned child") {
                self.reaped = true;
                return status;
            }
            assert!(Instant::now() < deadline, "child death timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // Failure/unwind cleanup is bounded and never targets an unrelated PID.
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

#[test]
fn committed_registry_receipt_survives_process_death_and_refenced_replay() {
    let profile = tempfile::tempdir().unwrap();
    let evidence_dir = tempfile::tempdir().unwrap();
    let ready = evidence_dir.path().join("committed-receipt.json");
    let _root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
    let mut child = OwnedChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registry_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env(PROFILE_ENV, profile.path())
            .env(READY_ENV, &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn owned normal-build child"),
        reaped: false,
    };
    let deadline = Instant::now() + PROCESS_WAIT;
    while !ready.exists() {
        assert!(
            child.child.try_wait().unwrap().is_none(),
            "child failed before committed reply"
        );
        assert!(Instant::now() < deadline, "child readiness timed out");
        thread::sleep(Duration::from_millis(5));
    }
    let mut bytes = vec![];
    fs::File::open(&ready)
        .unwrap()
        .take(EVIDENCE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() as u64 <= EVIDENCE_LIMIT);
    let original: ReceiptEvidence = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(original.disposition, "applied");
    assert!(original.sequence > 0);
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    assert!(!child.kill_and_reap().success());

    // Evidence is from the committed WAL/store, not child clean shutdown or fabricated values.
    assert_persisted(profile.path(), &original, false);
    let guard = WritableProfileGuard::acquire_current().expect("reacquire after process death");
    assert_ne!(guard.owner_identity(), original.owner);
    let mut writer = start(&guard);
    assert_registry(&writer, profile.path(), false);
    let stale = request(&original.owner, profile.path(), false, 1);
    assert_eq!(stale.payload_identity(), &original.payload);
    assert!(matches!(
        apply(&writer, stale),
        Err(LibraryError::SourceRegistryParticipantStaleOwner)
    ));
    assert_registry(&writer, profile.path(), false);
    assert_persisted(profile.path(), &original, false);

    let rebound = request(guard.owner_identity(), profile.path(), false, 1);
    let receipt = apply(&writer, rebound.clone()).expect("refenced exact replay");
    assert_eq!(ReceiptEvidence::observed(&receipt), original);
    for changed in [
        request(guard.owner_identity(), profile.path(), true, 1),
        request(guard.owner_identity(), profile.path(), false, 2),
    ] {
        assert!(matches!(
            apply(&writer, changed),
            Err(LibraryError::SourceRegistryParticipantReplayConflict)
        ));
    }
    assert_registry(&writer, profile.path(), false);
    assert_persisted(profile.path(), &original, false);
    writer
        .replace_source_registry(replacement(profile.path(), true))
        .unwrap()
        .recv_timeout(REPLY_WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(apply(&writer, rebound).unwrap(), receipt);
    assert_registry(&writer, profile.path(), true);
    assert_persisted(profile.path(), &original, true);
    assert!(writer.shutdown());
    // Public metadata-only evidence appears only after every actual owner/SQLite
    // assertion and cleanup above. Never emit private receipt or profile data.
    println!(
        "\nYIELD_GLOBAL_RECOVERY_CASE={} ",
        serde_json::json!({
            "schema": "yield.global-recovery-case.v1",
            "case": "committed_registry_receipt_process_death",
            "provenance": "actual_normal_build_owned_process_sqlite",
            "outcome": "observed_metadata_replay",
            "observations": [
                "committed_receipt_survived_owned_process_death",
                "competing_profile_ownership_denied",
                "old_acquisition_rejected",
                "refenced_replay_preserved_historical_receipt",
                "changed_payload_and_work_rejected",
                "later_registry_edit_preserved",
                "one_receipt_retained"
            ]
        })
    );
}

#[test]
#[ignore = "bounded helper dispatched by the process-death parent test"]
fn registry_process_child() {
    let Some(profile) = std::env::var_os(PROFILE_ENV) else {
        return;
    };
    let profile = std::path::PathBuf::from(profile);
    let ready = std::path::PathBuf::from(std::env::var_os(READY_ENV).unwrap());
    let _root = AppRootGuard::set(profile.clone()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let writer = start(&guard);
    let proposal = request(guard.owner_identity(), &profile, false, 1);
    let payload = *proposal.payload_identity();
    let receipt = apply(&writer, proposal).expect("real committed child receipt");
    assert_eq!(receipt.payload_identity(), &payload);
    assert_registry(&writer, &profile, false);
    let bytes = serde_json::to_vec(&ReceiptEvidence::observed(&receipt)).unwrap();
    assert!(bytes.len() as u64 <= EVIDENCE_LIMIT);
    let temporary = ready.with_extension("pending");
    fs::write(&temporary, bytes).unwrap();
    fs::rename(temporary, ready).unwrap();
    // Keep owner and writer alive. Parent kills this process without Drop/shutdown.
    // A parent failure cannot leave a permanently running helper either.
    thread::sleep(PROCESS_WAIT);
    std::process::exit(2);
}
