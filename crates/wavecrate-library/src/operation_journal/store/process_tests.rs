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
use crate::operation_journal::{OperationId, StoredOperationRecord, decode};
use crate::test_runtime::TestRuntimeGuard;

const CONFIG_HOME_ENV: &str = "WAVECRATE_CONFIG_HOME";
const CONFIG_PROFILE_ENV: &str = "WAVECRATE_CONFIG_PROFILE";
const PROFILE_NAME: &str = "operation-journal-restart";
const OPERATION_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

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

fn spawn_journal_owner_helper(fixture: &ProcessFixture) -> JournalOwnerProcess {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("operation_journal::store::process_tests::journal_owner_process_helper")
        .arg("--ignored")
        .arg("--nocapture")
        .env(CONFIG_HOME_ENV, fixture._base.path())
        .env(CONFIG_PROFILE_ENV, &fixture.profile_name)
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

#[test]
fn abrupt_owner_restart_retains_acknowledged_intent() {
    let fixture = ProcessFixture::new();
    let mut helper = spawn_journal_owner_helper(&fixture);
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
#[ignore = "spawned by abrupt_owner_restart_retains_acknowledged_intent"]
fn journal_owner_process_helper() {
    let owner = WritableProfileGuard::acquire_current().expect("journal helper profile owner");
    let store = OperationJournalStore::open(&owner).expect("journal helper store");
    let operation_id = OperationId::parse(OPERATION_ID).expect("fixed operation ID");
    store
        .insert_intent(&intent_record(operation_id))
        .expect("journal helper intent insert");
    println!("READY {operation_id}");
    io::stdout()
        .flush()
        .expect("flush journal helper handshake");
    let mut command = String::new();
    io::stdin()
        .read_line(&mut command)
        .expect("wait for journal helper termination");
}
