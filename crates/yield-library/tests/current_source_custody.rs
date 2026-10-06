//! Normal-build original Current producer/queue; physical native execution is separately scoped.
#![cfg(not(feature = "test-support"))]
use std::{fs, path::Path, time::Duration};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
    filesystem_identity::stable_filesystem_identity_from_open_file,
    operation_journal::{OperationId, OperationJournalOwner, OperationJournalStore},
    sample_sources::{SourceDatabase, db::SourceAuthorityRegistration},
};
const WAIT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
use yield_library::durable_capacity::descriptor_capacity_facts;
#[cfg(target_os = "macos")]
const EXTERNAL: &str = "YIELD_CURRENT_CUSTODY_EXTERNAL_ROOT";

fn source_paths(root: &Path) {
    fs::create_dir(root.join("media")).unwrap();
    fs::create_dir(root.join("database")).unwrap();
}
fn open_source(profile: &WritableProfileGuard, root: &Path) -> SourceDatabase {
    SourceDatabase::open_for_source_write_with_profile_guard(
        root.join("media"),
        profile,
        root.join("database"),
    )
    .unwrap()
}
fn retire(
    owner: &OperationJournalOwner,
    authority: &mut SourceAuthorityRegistration,
    source: SourceDatabase,
    receipt: yield_library::sample_sources::db::SourceAuthorityRegistrationReceipt,
) {
    let mut drain = owner.client().begin_source_drain(receipt.clone()).unwrap();
    drain.recv_timeout(WAIT).unwrap().unwrap();
    let acknowledgement = authority.close_database(source).unwrap();
    let mut finalizer = owner
        .client()
        .finalize_source_retirement(receipt, acknowledgement)
        .unwrap();
    finalizer.recv_timeout(WAIT).unwrap().unwrap();
}

// All production APIs and all branches typecheck on Linux/Windows, without fixture constructors.
fn run_original_queue(
    profile: &WritableProfileGuard,
    root: &Path,
    physical: bool,
    lost_reply: bool,
) {
    let source = open_source(profile, root);
    let mut authority = source.prepare_source_authority().unwrap();
    let reserve_path = root.join("media/recovery-reserve.bin");
    let store = OperationJournalStore::open(profile).unwrap();
    let mut owner = match OperationJournalOwner::start(store) {
        Ok(owner) => owner,
        Err(error) => {
            assert!(!physical, "genuine Native bootstrap unavailable: {error}");
            assert!(!reserve_path.exists());
            return;
        }
    };
    assert!(
        physical,
        "unsupported target must not bootstrap physical ownership"
    );
    let client = owner.client();
    let reply = client
        .register_source_authority(authority.journal_request().unwrap())
        .unwrap();
    if lost_reply {
        drop(reply);
        let mut barrier = client.get(OperationId::new()).unwrap();
        assert!(barrier.recv_timeout(WAIT).unwrap().unwrap().is_none());
        let held = fs::File::open(&reserve_path).expect("actual external producer reserve");
        let identity = stable_filesystem_identity_from_open_file(&held).unwrap();
        assert!(held.metadata().unwrap().len() >= PROTECTED_FREE_SPACE_FLOOR);
        // Closing a worker with an unretired registration is not clean settlement. Preserve
        // this dedicated case directory and reserve-object evidence for verification. This
        // public harness does not inspect the private canonical ownership marker.
        assert!(owner.shutdown());
        assert_eq!(
            stable_filesystem_identity_from_open_file(&held).unwrap(),
            identity
        );
        assert_eq!(
            stable_filesystem_identity_from_open_file(&fs::File::open(&reserve_path).unwrap())
                .unwrap(),
            identity
        );
        return;
    }
    let mut reply = reply;
    let receipt = reply.recv_timeout(WAIT).unwrap().unwrap();
    let held = fs::File::open(&reserve_path).unwrap();
    let identity = stable_filesystem_identity_from_open_file(&held).unwrap();
    assert!(held.metadata().unwrap().len() >= PROTECTED_FREE_SPACE_FLOOR);
    retire(&owner, &mut authority, source, receipt);
    drop(authority);
    assert!(owner.shutdown());
    // Actual clean owner restart consumes the existing durable binding and genuine new source
    // seal; source reserve bytes/object remain unchanged. No synthetic generation is supplied.
    let source = open_source(profile, root);
    let mut authority = source.prepare_source_authority().unwrap();
    let store = OperationJournalStore::open(profile).unwrap();
    let mut owner = OperationJournalOwner::start(store).unwrap();
    let mut reply = owner
        .client()
        .register_source_authority(authority.journal_request().unwrap())
        .unwrap();
    let receipt = reply.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(
        stable_filesystem_identity_from_open_file(&fs::File::open(&reserve_path).unwrap()).unwrap(),
        identity
    );
    assert_eq!(held.metadata().unwrap().len(), PROTECTED_FREE_SPACE_FLOOR);
    retire(&owner, &mut authority, source, receipt);
    assert!(owner.shutdown());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn original_normal_current_bootstrap_refuses_before_external_reserve_creation() {
    let app = tempfile::tempdir().unwrap();
    let _app = AppRootGuard::set(app.path().to_owned()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let root = tempfile::tempdir().unwrap();
    source_paths(root.path());
    run_original_queue(&profile, root.path(), false, false);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires isolated native account, genuine distinct mounted APFS volume and physical free space"]
fn original_native_current_external_queue_create_and_clean_rebind() {
    native_case(false);
}
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires separate fresh external APFS volume; preserves reserve object evidence, no cleanup or retry"]
fn original_native_current_external_queue_lost_reply_retains_reserve() {
    native_case(true);
}
#[cfg(target_os = "macos")]
fn native_case(lost_reply: bool) {
    let external = fs::canonicalize(
        std::env::var_os(EXTERNAL).expect("exact dedicated Native external root required"),
    )
    .unwrap();
    let profile_root = tempfile::tempdir().unwrap().keep();
    let _app = AppRootGuard::set(profile_root.clone()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let profile_volume =
        descriptor_capacity_facts(&fs::File::open(profile.profile_root()).unwrap()).unwrap();
    let source_volume = descriptor_capacity_facts(&fs::File::open(&external).unwrap()).unwrap();
    assert_ne!(
        profile_volume.identity, source_volume.identity,
        "same-volume alias cannot exercise external producer"
    );
    assert!(source_volume.free_bytes >= 2 * PROTECTED_FREE_SPACE_FLOOR);
    let root = external.join(format!("current-custody-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    source_paths(&root);
    eprintln!(
        "preserved verification roots: profile={},source={}",
        profile_root.display(),
        root.display()
    );
    run_original_queue(&profile, &root, true, lost_reply);
}
