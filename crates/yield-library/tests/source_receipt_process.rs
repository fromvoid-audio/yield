//! Actual source receipt persistence after owned process death (OPT-2176).
#![cfg(not(feature = "test-support"))]
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
use yield_library::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    operation_journal::{OperationId, ParticipantId},
    sample_sources::{
        Rating, SourceCommandId, SourceCurationUpdate, SourceDatabase, SourceDbError, SourceId,
        SourceLeaseError, SourceRevision, SourceSnapshotOwner, SourceSnapshotPage, SourceWriter,
        SourceWriterCommandIdentity, SourceWriterError, SourceWriterReceipt, SourceWriterRequest,
        reconciliation::RootRelativePath,
    },
};
const PROCESS_WAIT: Duration = Duration::from_secs(30);
const PROFILE_ENV: &str = "YIELD_SOURCE_PROCESS_PROFILE";
const MEDIA_ENV: &str = "YIELD_SOURCE_PROCESS_MEDIA";
const READY_ENV: &str = "YIELD_SOURCE_PROCESS_READY";
const EVIDENCE_LIMIT: u64 = 8192;
// All receipt observations, encoded as values only. Never deserialize an authority.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Evidence {
    operation: OperationId,
    participant: ParticipantId,
    command: SourceCommandId,
    source: String,
    root: Vec<u8>,
    binding: String,
    database: String,
    lifecycle: u64,
    epoch: u64,
    revision: u64,
    curation_revision: u64,
    curation_root: Vec<u8>,
    directory_generation: u64,
    // Debug covers every field of the bounded public metadata rows; it is observation only.
    metadata: String,
}
impl Evidence {
    fn observed(receipt: &SourceWriterReceipt) -> Self {
        let id = receipt.identity();
        let r = receipt.source_revision();
        let c = receipt.curation();
        Self {
            operation: id.operation_id(),
            participant: id.participant_id(),
            command: id.command_id(),
            source: r.source_id().as_str().into(),
            root: r.root_identity().as_bytes().into(),
            binding: r.root_binding_identity().into(),
            database: r.database_identity().into(),
            lifecycle: r.lifecycle_generation(),
            epoch: r.lease_epoch(),
            revision: r.revision(),
            curation_revision: c.source_revision(),
            curation_root: c.root_identity().as_bytes().into(),
            directory_generation: c.directory_generation(),
            metadata: format!("{:?}", c.files()),
        }
    }
    fn identity(&self) -> SourceWriterCommandIdentity {
        SourceWriterCommandIdentity::new(self.operation, self.participant, self.command)
    }
}
fn open(media: &Path, profile: &WritableProfileGuard) -> Result<SourceDatabase, SourceDbError> {
    SourceDatabase::open_for_catalog_with_profile_guard(media, profile, Path::new("source-process"))
}
fn request(revision: SourceRevision, identity: SourceWriterCommandIdentity) -> SourceWriterRequest {
    SourceWriterRequest::new(
        identity.operation_id(),
        identity.participant_id(),
        identity.command_id(),
        revision,
        vec![SourceCurationUpdate::SetRating {
            path: RootRelativePath::try_from_path("fixture.wav".into()).unwrap(),
            rating: Rating::KEEP_3,
        }],
    )
    .unwrap()
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
fn committed_source_receipt_survives_process_death_and_real_lease_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let profile_path = temp.path().join("profile");
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let ready = temp.path().join("receipt.json");
    let _root = AppRootGuard::set(profile_path.clone()).unwrap();
    let mut child = OwnedChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "source_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env(PROFILE_ENV, &profile_path)
            .env(MEDIA_ENV, &media)
            .env(READY_ENV, &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
        reaped: false,
    };
    let deadline = Instant::now() + PROCESS_WAIT;
    while !ready.exists() {
        assert!(
            child.child.try_wait().unwrap().is_none(),
            "child failed before committed replay"
        );
        assert!(Instant::now() < deadline, "readiness timeout");
        thread::sleep(Duration::from_millis(5));
    }
    let mut bytes = Vec::new();
    fs::File::open(&ready)
        .unwrap()
        .take(EVIDENCE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() as u64 <= EVIDENCE_LIMIT);
    let evidence: Evidence = serde_json::from_slice(&bytes).unwrap();
    assert!(matches!(
        WritableProfileGuard::acquire_current(),
        Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
    ));
    assert!(!child.kill_and_reap().success());
    let profile = WritableProfileGuard::acquire_current().unwrap();
    assert!(
        matches!(
            open(&media, &profile),
            Err(SourceDbError::SourceLease(
                SourceLeaseError::LeaseNotExpired { .. }
            ))
        ),
        "production live lease must reject early reopen"
    );
    let deadline = Instant::now() + Duration::from_secs(360);
    let database = loop {
        assert!(
            Instant::now() < deadline,
            "real lease reacquisition exceeded 360 seconds"
        );
        match open(&media, &profile) {
            Ok(db) => break db,
            Err(SourceDbError::SourceLease(SourceLeaseError::LeaseNotExpired { .. })) => {
                thread::sleep(Duration::from_secs(1))
            }
            Err(error) => panic!("unexpected reopen error: {error}"),
        }
    };
    let mut writer =
        SourceWriter::new(database, SourceId::from_string("source-process"), 2).unwrap();
    let current = writer.current_revision().unwrap();
    assert_eq!(current.database_identity(), evidence.database);
    assert_eq!(current.root_identity().as_bytes(), evidence.root);
    assert_eq!(current.root_binding_identity(), evidence.binding);
    assert_eq!(current.source_id().as_str(), evidence.source);
    assert_eq!(current.revision(), evidence.revision);
    assert_ne!(current.lease_epoch(), evidence.epoch);
    assert_ne!(current.lifecycle_generation(), evidence.lifecycle);
    assert_eq!(
        Evidence::observed(
            &writer
                .read_receipt_observation(evidence.identity())
                .unwrap()
                .unwrap()
        ),
        evidence
    );
    // Fresh authority with old identity has different canonical request bytes: reject, never rewrite.
    assert!(matches!(
        writer.commit_curation(request(current.clone(), evidence.identity())),
        Err(SourceWriterError::ConflictingCommand { .. })
    ));
    assert_eq!(writer.current_revision().unwrap(), current);
    assert_eq!(
        Evidence::observed(
            &writer
                .read_receipt_observation(evidence.identity())
                .unwrap()
                .unwrap()
        ),
        evidence
    );
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let SourceSnapshotPage::FreshAsOf { revision, page } =
        owner.read_browser_metadata_page(&current, None, 2).unwrap()
    else {
        panic!("current snapshot stale")
    };
    assert_eq!(revision, current);
    assert_eq!(page.revision, current.revision());
    assert_eq!(page.files.len(), 1);
    assert!(page.next_cursor.is_none());
    assert_eq!(format!("{:?}", page.files), evidence.metadata);
    assert_eq!(page.files[0].relative_path, Path::new("fixture.wav"));
    assert_eq!(page.files[0].rating, Rating::KEEP_3);
    assert!(!page.files[0].locked);
    assert!(!page.files[0].missing);
    let retirement = owner.begin_retirement();
    let deadline = Instant::now() + PROCESS_WAIT;
    while !retirement.is_finished() {
        assert!(Instant::now() < deadline, "snapshot retirement timed out");
        thread::sleep(Duration::from_millis(5));
    }
    retirement
        .join()
        .expect("snapshot worker retired")
        .close()
        .unwrap();
}
#[test]
#[ignore = "owned subprocess helper, invoked by parent"]
fn source_process_child() {
    let profile_path = std::env::var_os(PROFILE_ENV).unwrap();
    let media = std::path::PathBuf::from(std::env::var_os(MEDIA_ENV).unwrap());
    let ready = std::path::PathBuf::from(std::env::var_os(READY_ENV).unwrap());
    let _root = AppRootGuard::set(profile_path.into()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let writer = SourceWriter::new_for_scan(
        open(&media, &profile).unwrap(),
        SourceId::from_string("source-process"),
        1,
    )
    .unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let mut batch = writer.catalog_database().write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new("fixture.wav"), 44, 1000, "fixture-content")
        .unwrap();
    writer.commit_scan_manifest_batch(&fence, batch).unwrap();
    let generation = writer.begin_scan_directory_truth_generation(0).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let batch = writer.catalog_database().write_batch().unwrap();
    writer
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            2000,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    let mut writer = writer;
    let initial = writer.current_revision().unwrap();
    let id = SourceWriterCommandIdentity::new(
        OperationId::from_uuid(Uuid::from_u128(2176)),
        ParticipantId::from_uuid(Uuid::from_u128(2176)),
        SourceCommandId::from_uuid(Uuid::from_u128(2176)),
    );
    let request = request(initial.clone(), id);
    let receipt = writer.commit_curation(request.clone()).unwrap();
    let committed = writer.current_revision().unwrap();
    assert_eq!(committed.revision(), initial.revision() + 1);
    assert_eq!(receipt.source_revision(), &committed);
    assert_eq!(writer.commit_curation(request).unwrap(), receipt);
    assert_eq!(writer.current_revision().unwrap(), committed);
    let bytes = serde_json::to_vec(&Evidence::observed(&receipt)).unwrap();
    assert!(bytes.len() as u64 <= EVIDENCE_LIMIT);
    let pending = ready.with_extension("pending");
    fs::write(&pending, bytes).unwrap();
    fs::rename(pending, ready).unwrap();
    thread::sleep(PROCESS_WAIT);
    std::process::exit(2); // No owner destructor if parent fails.
}
