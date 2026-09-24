//! Cross-process restart coverage for the profile-owned operation journal.

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

use super::OperationJournalStore;
use crate::app_dirs::{
    ConfigBaseGuard, PersistenceProfileGuard, ProfileOwnershipError, WritableProfileGuard,
};
use crate::operation_journal::{
    CancellationRequestObservation, OperationDisposition, OperationId, SCHEMA_V4, SCHEMA_V5,
    StoredOperationRecord, decode,
};
use crate::test_runtime::TestRuntimeGuard;

const CONFIG_HOME_ENV: &str = "YIELD_CONFIG_HOME";
const CONFIG_PROFILE_ENV: &str = "YIELD_CONFIG_PROFILE";
const PROFILE_NAME: &str = "operation-journal-restart";
const OPERATION_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
const CANCEL_HELPER_ENV: &str = "YIELD_JOURNAL_HELPER_CANCEL";
const RETRY_HELPER_ENV: &str = "YIELD_JOURNAL_HELPER_RETRY";

#[derive(Clone, Copy)]
enum HelperMode {
    Intent,
    Cancellation,
    Retry,
}

struct ProcessFixture {
    profile_name: String,
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: TempDir,
    _runtime: TestRuntimeGuard,
}

impl ProcessFixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let temporary_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
        let base = tempfile::tempdir_in(temporary_root).expect("temporary config base");
        let base_path = base.path().to_path_buf();
        let config = ConfigBaseGuard::set(base_path);
        let profile = PersistenceProfileGuard::named(PROFILE_NAME);
        Self {
            profile_name: String::from(PROFILE_NAME),
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
}

struct JournalOwnerProcess {
    child: Child,
    _stdin: ChildStdin,
    ready_rx: Receiver<Result<OperationId, String>>,
}

impl Drop for JournalOwnerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn spawn_journal_owner_helper(fixture: &ProcessFixture, mode: HelperMode) -> JournalOwnerProcess {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("operation_journal::store::process_tests::journal_owner_process_helper")
        .arg("--ignored")
        .arg("--nocapture")
        // Exercise libtest's inline test-name prefix regardless of the parent thread setting.
        .arg("--test-threads=1")
        .env(CONFIG_HOME_ENV, fixture._base.path())
        .env(CONFIG_PROFILE_ENV, &fixture.profile_name)
        .env(
            CANCEL_HELPER_ENV,
            if matches!(mode, HelperMode::Cancellation) {
                "1"
            } else {
                "0"
            },
        )
        .env(
            RETRY_HELPER_ENV,
            if matches!(mode, HelperMode::Retry) {
                "1"
            } else {
                "0"
            },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn journal owner helper");
    let stdin = child.stdin.take().expect("journal helper stdin");
    let stdout = child.stdout.take().expect("journal helper stdout");
    let stderr = child.stderr.take().expect("journal helper stderr");
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let ready = loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break Err(String::from("journal helper exited before handshake")),
                Ok(_) if line.trim_start().starts_with("READY ") => {
                    let value = line.trim().strip_prefix("READY ").unwrap_or_default();
                    break OperationId::parse(value)
                        .map_err(|error| format!("journal helper returned invalid ID: {error}"));
                }
                Ok(_) => continue,
                Err(error) => break Err(format!("journal helper handshake read failed: {error}")),
            }
        };
        let _ = ready_tx.send(ready);
        let _ = io::copy(&mut reader, &mut io::sink());
    });
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let _ = io::copy(&mut reader, &mut io::sink());
    });
    JournalOwnerProcess {
        child,
        _stdin: stdin,
        ready_rx,
    }
}

fn wait_for_ready(helper: &mut JournalOwnerProcess) -> OperationId {
    match helper.ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(operation_id)) => operation_id,
        Ok(Err(error)) => panic!("journal helper handshake failed: {error}"),
        Err(error) => panic!("timed out waiting for journal helper: {error}"),
    }
}

fn terminate_abruptly(helper: &mut JournalOwnerProcess) {
    helper.child.kill().expect("kill journal helper");
    let status = helper.child.wait().expect("wait for killed journal helper");
    assert!(!status.success(), "helper must be terminated abruptly");
}

fn intent_record(operation_id: OperationId) -> StoredOperationRecord {
    let value = serde_json::json!({
        "schema_version": 1,
        "operation_id": operation_id.as_uuid(),
        "intent": {
            "actor": "User",
            "kind": "FileMutation",
            "label": "restart-test"
        },
        "phase": "IntentDurable",
        "disposition": "None",
        "payload": {},
        "capacity_plan": {
            "volumes": [{
                "identity": {"device": 1},
                "allocation_unit": 1,
                "allocation_class": "DestinationStaging",
                "logical_bytes": 1,
                "allocated_bytes": 1,
                "protected_free_bytes": 268435456
            }]
        },
        "prepared": null,
        "staged": null,
        "published": null,
        "replacement_qualification": null,
        "created_unix_ms": 1,
        "updated_unix_ms": 1
    });
    decode(&serde_json::to_vec(&value).expect("serialize intent fixture"))
        .expect("decode intent fixture")
}

fn universal_intent_record(operation_id: OperationId) -> StoredOperationRecord {
    let value = serde_json::json!({
        "schema_version": 3,
        "operation_id": operation_id.as_uuid(),
        "intent": {
            "actor": "User",
            "kind": "FileMutation",
            "label": "restart-v3-test"
        },
        "universal_intent": {
            "parent_operation_id": null,
            "command_kind": "CreateFile",
            "user_status_key": "operation.status.restart-v3",
            "participants": {
                "descriptors": [{
                    "id": "fedcba98-7654-3210-fedc-ba9876543210",
                    "kind": "GlobalLibrary",
                    "requirement": "Required",
                    "source_fence": null,
                    "artifact_key": null
                }],
                "checkpoints": [{
                    "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                    "state": "Pending",
                    "committed_revision": null,
                    "diagnostic_code": null
                }]
            }
        },
        "phase": "IntentDurable",
        "disposition": "None",
        "payload": {"restart": "v3"},
        "capacity_plan": {
            "volumes": [{
                "identity": {"device": 1},
                "allocation_unit": 1,
                "allocation_class": "DestinationStaging",
                "logical_bytes": 1,
                "allocated_bytes": 1,
                "protected_free_bytes": 268435456
            }]
        },
        "created_unix_ms": 1,
        "updated_unix_ms": 1
    });
    decode(&serde_json::to_vec(&value).expect("serialize universal intent fixture"))
        .expect("decode universal intent fixture")
}

fn v5_copy_record(operation_id: OperationId) -> StoredOperationRecord {
    let value = serde_json::json!({
        "schema_version": 5,
        "operation_id": operation_id.as_uuid(),
        "intent": {
            "actor": "User",
            "kind": "FileMutation",
            "label": "restart-v5-copy"
        },
        "universal_intent": {
            "parent_operation_id": null,
            "command_kind": "Copy",
            "user_status_key": "operation.status.restart-v5",
            "participants": {
                "descriptors": [
                    {
                        "id": "fedcba98-7654-3210-fedc-ba9876543210",
                        "kind": "SourceDatabase",
                        "requirement": "Required",
                        "source_fence": {
                            "source_id": "source-a",
                            "root_identity": "root-a",
                            "database_identity": "database-a",
                            "lifecycle_generation": 1,
                            "source_revision": 7,
                            "lease_epoch": 1
                        },
                        "artifact_key": null
                    },
                    {
                        "id": "89abcdef-0123-4567-89ab-cdef01234567",
                        "kind": "Filesystem",
                        "requirement": "Required",
                        "source_fence": null,
                        "artifact_key": null
                    }
                ],
                "checkpoints": [
                    {
                        "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                        "state": "Pending",
                        "committed_revision": null,
                        "diagnostic_code": null
                    },
                    {
                        "participant_id": "89abcdef-0123-4567-89ab-cdef01234567",
                        "state": "Pending",
                        "committed_revision": null,
                        "diagnostic_code": null
                    }
                ]
            }
        },
        "phase": "IntentDurable",
        "disposition": "None",
        "command_args": {
            "source": {
                "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                "source_id": "source-a",
                "relative_path": "in.wav"
            },
            "destination": {
                "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                "source_id": "source-a",
                "relative_path": "out.wav"
            },
            "expected_source": {
                "source_revision": 7,
                "file_identity": "file-identity",
                "file_size": 10,
                "modified_ns": 11
            },
            "collision_policy": "NoReplace",
            "inherited_metadata_reference": "metadata/entry"
        },
        "capacity_plan": {
            "volumes": [{
                "identity": {"device": 1},
                "allocation_unit": 1,
                "allocation_class": "DestinationStaging",
                "logical_bytes": 1,
                "allocated_bytes": 1,
                "protected_free_bytes": 268435456
            }]
        },
        "retry_observation": null,
        "cancellation_request": null,
        "created_unix_ms": 1,
        "updated_unix_ms": 1
    });
    decode(&serde_json::to_vec(&value).expect("serialize v5 copy fixture"))
        .expect("decode v5 copy fixture")
}

#[test]
fn abrupt_owner_restart_retains_acknowledged_intent() {
    let fixture = ProcessFixture::new();
    let mut helper = spawn_journal_owner_helper(&fixture, HelperMode::Intent);
    let operation_id = wait_for_ready(&mut helper);
    assert_eq!(
        operation_id,
        OperationId::parse(OPERATION_ID).expect("fixed ID")
    );
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    terminate_abruptly(&mut helper);

    let owner = WritableProfileGuard::acquire_current().expect("reacquire after abrupt exit");
    let store = OperationJournalStore::open(&owner).expect("reopen journal after abrupt exit");
    assert_eq!(
        store.get(operation_id).expect("read acknowledged intent"),
        Some(intent_record(operation_id))
    );
}

#[test]
fn abrupt_owner_restart_retains_acknowledged_cancellation_request() {
    let fixture = ProcessFixture::new();
    let mut helper = spawn_journal_owner_helper(&fixture, HelperMode::Cancellation);
    let operation_id = wait_for_ready(&mut helper);
    terminate_abruptly(&mut helper);

    let owner = WritableProfileGuard::acquire_current().expect("reacquire after abrupt exit");
    let store = OperationJournalStore::open(&owner).expect("reopen journal after abrupt exit");
    let persisted = store
        .get(operation_id)
        .expect("read acknowledged cancellation request")
        .expect("cancellation request row");
    assert_eq!(persisted.schema_version, SCHEMA_V4);
    assert_eq!(
        persisted.disposition,
        crate::operation_journal::OperationDisposition::CancelRequestedBeforePublish
    );
    assert_eq!(
        persisted
            .cancellation_request()
            .expect("cancellation observation")
            .requested_unix_ms(),
        2
    );
}

#[test]
fn abrupt_owner_restart_retains_acknowledged_retry_observation() {
    let fixture = ProcessFixture::new();
    let mut helper = spawn_journal_owner_helper(&fixture, HelperMode::Retry);
    let operation_id = wait_for_ready(&mut helper);
    terminate_abruptly(&mut helper);

    let owner = WritableProfileGuard::acquire_current().expect("reacquire after abrupt exit");
    let store = OperationJournalStore::open(&owner).expect("reopen journal after abrupt exit");
    let persisted = store
        .get(operation_id)
        .expect("read acknowledged retry observation")
        .expect("retry observation row");
    assert_eq!(persisted.schema_version, SCHEMA_V5);
    assert_eq!(persisted.disposition, OperationDisposition::RetryPending);
    assert_eq!(
        persisted
            .command_args()
            .expect("typed copy arguments")
            .source()
            .relative_path()
            .as_str(),
        "in.wav"
    );
    let retry = persisted.retry_observation().expect("retry observation");
    assert_eq!(retry.attempt(), 1);
    assert_eq!(retry.requested_unix_ms(), 2);
    assert_eq!(retry.next_retry_unix_ms(), 3);
    assert_eq!(retry.lease_id().as_str(), "restart-retry-lease");
    assert_eq!(retry.lease_expiry_unix_ms(), 4);
    assert_eq!(retry.diagnostic_code(), "writer_busy");
}

#[test]
#[ignore = "spawned by abrupt_owner_restart_retains_acknowledged_intent"]
fn journal_owner_process_helper() {
    let owner = WritableProfileGuard::acquire_current().expect("journal helper profile owner");
    let store = OperationJournalStore::open(&owner).expect("journal helper store");
    let operation_id = OperationId::parse(OPERATION_ID).expect("fixed operation ID");
    if std::env::var_os(RETRY_HELPER_ENV).is_some_and(|value| value == "1") {
        let record = v5_copy_record(operation_id);
        let inserted = store
            .persist_copy_intent(
                operation_id,
                record.intent.clone(),
                record
                    .universal_intent
                    .clone()
                    .expect("journal helper universal intent"),
                record
                    .command_args()
                    .cloned()
                    .expect("journal helper copy arguments"),
                record
                    .capacity_plan
                    .clone()
                    .expect("journal helper capacity plan"),
                1,
            )
            .expect("journal helper v5 intent insert");
        let observation = serde_json::from_value(serde_json::json!({
            "attempt": 1,
            "requested_unix_ms": 2,
            "next_retry_unix_ms": 3,
            "lease_id": "restart-retry-lease",
            "lease_expiry_unix_ms": 4,
            "error_class": "TransientContention",
            "diagnostic_code": "writer_busy",
            "redacted_context": null
        }))
        .expect("journal helper retry observation");
        store
            .record_retry_pending(operation_id, inserted.sequence(), observation)
            .expect("journal helper retry observation");
    } else if std::env::var_os(CANCEL_HELPER_ENV).is_some_and(|value| value == "1") {
        let record = universal_intent_record(operation_id);
        let inserted = store
            .insert_intent(&record)
            .expect("journal helper intent insert");
        let observation =
            CancellationRequestObservation::new(2).expect("journal helper cancellation time");
        store
            .request_cancel_before_publish(operation_id, inserted.sequence(), observation)
            .expect("journal helper cancellation request");
    } else {
        store
            .insert_intent(&intent_record(operation_id))
            .expect("journal helper intent insert");
    }
    // Libtest may already have printed a test-name prefix without a newline.
    // Keep the handshake on its own line so the parent can parse it strictly.
    println!("\nREADY {operation_id}");
    io::stdout()
        .flush()
        .expect("flush journal helper handshake");
    let mut command = String::new();
    io::stdin()
        .read_line(&mut command)
        .expect("wait for journal helper termination");
}
