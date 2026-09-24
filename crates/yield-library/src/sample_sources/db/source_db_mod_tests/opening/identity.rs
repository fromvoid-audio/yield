use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

use super::*;
use crate::app_dirs::{
    AppRootGuard, ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard,
};
#[cfg(unix)]
use crate::sample_sources::database_path_for;
use crate::sample_sources::db::file_ops_journal::BoundDatabaseRoot;
use crate::sample_sources::db::open::open_source_database_with_profile_guard_for_test;
#[cfg(unix)]
use crate::sample_sources::readiness::{
    ReadinessError, ReadinessStage, ReadinessTarget, ReadinessTargetPublication, SourceAvailability,
};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

const SOURCE_LEASE_HELPER_BASE_ENV: &str = "WAVECRATE_SOURCE_LEASE_HELPER_BASE";
const SOURCE_LEASE_HELPER_PROFILE_ENV: &str = "WAVECRATE_SOURCE_LEASE_HELPER_PROFILE";
const SOURCE_LEASE_HELPER_SOURCE_ROOT_ENV: &str = "WAVECRATE_SOURCE_LEASE_HELPER_SOURCE_ROOT";
const SOURCE_LEASE_HELPER_DATABASE_ROOT_ENV: &str = "WAVECRATE_SOURCE_LEASE_HELPER_DATABASE_ROOT";
const SOURCE_LEASE_HELPER_MODE_ENV: &str = "WAVECRATE_SOURCE_LEASE_HELPER_MODE";

struct SourceLeaseHelperProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    ready_rx: Receiver<Result<(), String>>,
    eof_rx: Receiver<()>,
}

impl Drop for SourceLeaseHelperProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn spawn_source_lease_helper(
    base: &Path,
    profile: &str,
    source_root: &Path,
    database_root: &Path,
    mode: &str,
) -> SourceLeaseHelperProcess {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("sample_sources::db::source_db_mod_tests::opening::identity::source_lease_process_helper")
        .arg("--ignored")
        .arg("--nocapture")
        .env(SOURCE_LEASE_HELPER_BASE_ENV, base)
        .env(SOURCE_LEASE_HELPER_PROFILE_ENV, profile)
        .env(SOURCE_LEASE_HELPER_SOURCE_ROOT_ENV, source_root)
        .env(SOURCE_LEASE_HELPER_DATABASE_ROOT_ENV, database_root)
        .env(SOURCE_LEASE_HELPER_MODE_ENV, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn source lease helper");
    let stdin = child.stdin.take().expect("source lease helper stdin");
    let stdout = child.stdout.take().expect("source lease helper stdout");
    let stderr = child.stderr.take().expect("source lease helper stderr");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (eof_tx, eof_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let ready = loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break Err(String::from("source lease helper exited before handshake")),
                Ok(_) if line.trim_end() == "READY" => break Ok(()),
                Ok(_) => continue,
                Err(error) => break Err(format!("source lease helper handshake failed: {error}")),
            }
        };
        let _ = ready_tx.send(ready);
        let _ = io::copy(&mut reader, &mut io::sink());
        let _ = eof_tx.send(());
    });
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let _ = io::copy(&mut reader, &mut io::sink());
    });
    SourceLeaseHelperProcess {
        child,
        stdin: Some(stdin),
        ready_rx,
        eof_rx,
    }
}

fn wait_for_source_lease_helper_ready(helper: &mut SourceLeaseHelperProcess) {
    match helper.ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("source lease helper handshake failed: {error}"),
        Err(error) => panic!("timed out waiting for source lease helper: {error}"),
    }
}

fn finish_source_lease_helper(
    helper: &mut SourceLeaseHelperProcess,
    command: &str,
    expected_success: bool,
) {
    let mut stdin = helper
        .stdin
        .take()
        .expect("source lease helper command pipe");
    writeln!(stdin, "{command}").expect("send source lease helper command");
    stdin.flush().expect("flush source lease helper command");
    helper
        .eof_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("source lease helper exit");
    let status = helper.child.wait().expect("wait for source lease helper");
    assert_eq!(
        status.success(),
        expected_success,
        "source lease helper status"
    );
}

#[test]
fn source_database_identity_lease_releases_and_advances_epoch() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-source-lease-reopen";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned source database");
    database
        .set_metadata("lease-preserved", "source-data")
        .expect("source data before lease release");
    let database_path = profile_guard
        .profile_root()
        .join(profile_child)
        .join(DB_FILE_NAME);
    let (epoch, state, owner_identity): (i64, String, String) = database
        .connection
        .query_row(
            "SELECT lease_epoch, state, owner_identity FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("active source lease row");
    assert_eq!(epoch, 1);
    assert_eq!(state, "active");
    assert!(Uuid::parse_str(&owner_identity).is_ok());
    drop(database);

    let released = Connection::open(&database_path)
        .expect("reopen source database for lease inspection")
        .query_row(
            "SELECT lease_epoch, state FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("released source lease row");
    assert_eq!(released, (1, String::from("released")));

    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("reopen profile-owned source database");
    let (reopened_epoch, reopened_owner_identity) = reopened
        .connection
        .query_row(
            "SELECT lease_epoch, owner_identity FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("reopened source lease row");
    assert_eq!(reopened_epoch, 2);
    assert!(Uuid::parse_str(&reopened_owner_identity).is_ok());
    assert_ne!(reopened_owner_identity, owner_identity);
    assert_eq!(
        reopened
            .get_metadata("lease-preserved")
            .expect("preserved source data")
            .as_deref(),
        Some("source-data")
    );
}

#[test]
fn source_database_identity_publication_fence_is_read_only_and_weak() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("profile-owned source database");
    let changes_before = database
        .source_lease_control_total_changes_for_tests()
        .expect("physical source lease")
        .expect("control change count");
    let fence = database
        .source_publication_fence()
        .expect("publication fence");
    assert!(fence.is_live());
    fence.validate().expect("live publication fence");
    let changes_after = database
        .source_lease_control_total_changes_for_tests()
        .expect("physical source lease")
        .expect("control change count");
    assert_eq!(changes_after, changes_before);

    drop(database);
    assert!(!fence.is_live());
    assert!(matches!(
        fence.validate(),
        Err(SourcePublicationFenceError::LeaseDropped)
    ));
}

#[test]
fn source_database_identity_publication_fence_rejects_owner_and_epoch_changes() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("profile-owned source database");
    let fence = database
        .source_publication_fence()
        .expect("publication fence");
    let (owner_identity, lease_epoch): (String, i64) = database
        .connection
        .query_row(
            "SELECT owner_identity, lease_epoch FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("active source lease row");

    let replacement_owner = Uuid::new_v4().to_string();
    database
        .connection
        .execute(
            "UPDATE source_lease SET owner_identity = ?1 WHERE singleton = 1",
            [&replacement_owner],
        )
        .expect("replace durable owner identity");
    assert!(matches!(
        fence.validate(),
        Err(SourcePublicationFenceError::OwnerChanged)
    ));
    database
        .connection
        .execute(
            "UPDATE source_lease SET owner_identity = ?1 WHERE singleton = 1",
            [&owner_identity],
        )
        .expect("restore durable owner identity");

    database
        .connection
        .execute(
            "UPDATE source_lease SET lease_epoch = ?1 WHERE singleton = 1",
            [lease_epoch + 1],
        )
        .expect("replace durable lease epoch");
    assert!(matches!(
        fence.validate(),
        Err(SourcePublicationFenceError::EpochChanged)
    ));
    database
        .connection
        .execute(
            "UPDATE source_lease SET lease_epoch = ?1 WHERE singleton = 1",
            [lease_epoch],
        )
        .expect("restore durable lease epoch");
    fence.validate().expect("restored publication fence");
}

#[test]
fn source_database_identity_publication_fence_is_invalidated_by_catalog_handoff() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let replacement_root = tempdir().expect("replacement source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "publication-fence-catalog-handoff",
    )
    .expect("profile-owned catalog database");
    let old_fence = database
        .source_publication_fence()
        .expect("old publication fence");
    assert!(old_fence.is_live());
    let database = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .expect("catalog root handoff");

    assert!(matches!(
        old_fence.validate(),
        Err(SourcePublicationFenceError::LeaseDropped)
    ));
    assert!(!old_fence.is_live());
    let new_fence = database
        .source_publication_fence()
        .expect("new publication fence");
    assert!(new_fence.is_live());
    new_fence.validate().expect("new live publication fence");
}

#[test]
fn source_database_identity_catalog_handoff_advances_epoch_and_preserves_data() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let replacement_root = tempdir().expect("replacement source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-catalog-handoff",
    )
    .expect("profile-owned catalog database");
    database
        .set_metadata("handoff-preserved", "source-data")
        .expect("source data before handoff");
    let (old_epoch, old_owner, old_media_root): (i64, String, String) = database
        .connection
        .query_row(
            "SELECT lease_epoch, owner_identity, media_root_identity
             FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("active lease before handoff");

    let database = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .expect("catalog root handoff");
    let (new_epoch, new_owner, new_media_root): (i64, String, String) = database
        .connection
        .query_row(
            "SELECT lease_epoch, owner_identity, media_root_identity
             FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("active lease after handoff");
    assert_eq!(new_epoch, old_epoch + 1);
    assert_ne!(new_owner, old_owner);
    assert_ne!(new_media_root, old_media_root);
    assert_eq!(
        database
            .get_metadata("handoff-preserved")
            .expect("preserved source data")
            .as_deref(),
        Some("source-data")
    );
    assert_eq!(database.root(), replacement_root.path());
}

#[test]
fn source_database_identity_catalog_handoff_rejects_live_snapshot_fence() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let replacement_root = tempdir().expect("replacement source root");
    let destination_root = tempdir().expect("snapshot destination root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-catalog-handoff-snapshot",
    )
    .expect("profile-owned catalog database");
    let fence = database
        .snapshot_to_path_with_write_fence(&destination_root.path().join("snapshot.db"))
        .expect("snapshot fence");
    let error = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .err()
        .expect("live snapshot must reject catalog handoff");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SnapshotsActive)
    ));
    drop(fence);
}

#[test]
fn source_database_identity_external_handoff_is_unsupported() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let replacement_root = tempdir().expect("replacement source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("profile-owned external source database");
    let error = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .err()
        .expect("external source handoff must be unsupported");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::Unsupported { .. })
    ));
}

#[cfg(unix)]
#[test]
fn source_database_identity_catalog_handoff_rejects_replaced_media_or_database_root() {
    let app_root = tempdir().expect("app root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");

    let source_root = tempdir().expect("source root");
    let replacement_root = tempdir().expect("replacement source root");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-catalog-handoff-media-replacement",
    )
    .expect("profile-owned catalog database");
    let displaced_source_root = source_root.path().with_extension("displaced");
    std::fs::rename(source_root.path(), &displaced_source_root)
        .expect("displace source root before handoff");
    std::fs::create_dir(source_root.path()).expect("replace source root before handoff");
    let error = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .err()
        .expect("replaced media root must reject handoff");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SourceRootReplaced { .. })
    ));
    std::fs::remove_dir(source_root.path()).expect("remove replacement source root");
    std::fs::rename(displaced_source_root, source_root.path()).expect("restore source root");

    let source_root = tempdir().expect("second source root");
    let replacement_root = tempdir().expect("second replacement source root");
    let profile_child = "identity-catalog-handoff-database-replacement";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("second profile-owned catalog database");
    let child_path = profile_guard.profile_root().join(profile_child);
    let displaced_child = profile_guard
        .profile_root()
        .join(format!("{profile_child}.displaced"));
    std::fs::rename(&child_path, &displaced_child).expect("displace database root before handoff");
    std::fs::create_dir(&child_path).expect("replace database root before handoff");
    let error = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .err()
        .expect("replaced database root must reject handoff");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::DatabaseRootReplaced { .. })
    ));
    std::fs::remove_dir(&child_path).expect("remove replacement database root");
    std::fs::rename(displaced_child, child_path).expect("restore database root");
}

#[test]
fn source_database_identity_lease_implicit_drop_closes_connection_before_release() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let database_path = database_root.path().join(DB_FILE_NAME);
    database
        .connection
        .execute_batch(
            "BEGIN EXCLUSIVE;
             UPDATE metadata SET value = 'rolled-back-on-drop' WHERE key = 'drop-order-marker';",
        )
        .expect("hold an ordinary connection write transaction");

    drop(database);

    let state: String = Connection::open(&database_path)
        .expect("reopen source database after implicit drop")
        .query_row(
            "SELECT state FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("released source lease row");
    assert_eq!(state, "released");
}

#[test]
fn source_database_identity_lease_conflicts_on_one_external_source() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("first external source database");

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("the source lease must reject a second writer"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::WritableSourceOwnedByAnotherProcess { .. })
    ));
    drop(database);
}

#[test]
fn source_database_identity_lease_conflicts_across_profile_processes() {
    let base = tempfile::tempdir().expect("profile base");
    let base_path = std::fs::canonicalize(base.path()).expect("canonical profile base");
    let source_root = tempfile::tempdir().expect("source root");
    let database_root = tempfile::tempdir().expect("database root");
    let _base_guard = ConfigBaseGuard::set(base_path.clone());
    let _profile_selection = PersistenceProfileGuard::named("lease-process-parent");
    let profile_guard = WritableProfileGuard::acquire_current().expect("parent profile owner");
    let mut helper = spawn_source_lease_helper(
        &base_path,
        "lease-process-child",
        source_root.path(),
        database_root.path(),
        "release",
    );
    wait_for_source_lease_helper_ready(&mut helper);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("a second profile process must not acquire the source lease"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::WritableSourceOwnedByAnotherProcess { .. })
    ));

    finish_source_lease_helper(&mut helper, "release", true);
    let reopened = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("source lease after the other process released it");
    drop(reopened);
}

#[test]
fn source_database_identity_lease_recovers_same_profile_after_abrupt_expired_exit() {
    let base = tempfile::tempdir().expect("profile base");
    let base_path = std::fs::canonicalize(base.path()).expect("canonical profile base");
    let source_root = tempfile::tempdir().expect("source root");
    let database_root = tempfile::tempdir().expect("database root");
    let _base_guard = ConfigBaseGuard::set(base_path.clone());
    let _profile_selection = PersistenceProfileGuard::named("lease-process-crash");
    let mut helper = spawn_source_lease_helper(
        &base_path,
        "lease-process-crash",
        source_root.path(),
        database_root.path(),
        "crash",
    );
    wait_for_source_lease_helper_ready(&mut helper);
    finish_source_lease_helper(&mut helper, "crash", false);

    let profile_guard = WritableProfileGuard::acquire_current().expect("reacquire profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("same-profile expired lease takeover");
    let epoch = database
        .connection
        .query_row(
            "SELECT lease_epoch FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("recovered lease epoch");
    assert_eq!(epoch, 2);
}

#[test]
fn source_database_identity_lease_rejects_unexpired_reclaim() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("initial external source database");
    let database_path = database_root.path().join(DB_FILE_NAME);
    drop(database);
    let connection = Connection::open(&database_path).expect("lease row database");
    connection
        .execute(
            "UPDATE source_lease
             SET state = 'active', released_at = NULL, expires_at = ?1
             WHERE singleton = 1",
            [i64::MAX],
        )
        .expect("make active lease unexpired");
    drop(connection);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("unexpired source lease must not be reclaimed"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::LeaseNotExpired { .. })
    ));
}

#[test]
fn source_database_identity_lease_rejects_foreign_expired_reclaim() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("initial external source database");
    let database_path = database_root.path().join(DB_FILE_NAME);
    drop(database);
    let connection = Connection::open(&database_path).expect("lease row database");
    let acquired_at = connection
        .query_row(
            "SELECT acquired_at FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("lease acquisition timestamp");
    connection
        .execute(
            "UPDATE source_lease
             SET profile_root_identity = 'foreign-profile', state = 'active',
                 released_at = NULL, expires_at = ?1
             WHERE singleton = 1",
            [acquired_at],
        )
        .expect("make foreign lease expired");
    drop(connection);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("foreign expired source lease must not be reclaimed"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::LivenessUnverified { .. })
    ));
}

#[test]
fn source_database_identity_lease_fence_survives_original_drop() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let destination_root = tempdir().expect("snapshot root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let destination = destination_root.path().join("snapshot.db");
    let fence = database
        .snapshot_to_path_with_write_fence(&destination)
        .expect("snapshot write fence");
    drop(database);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("snapshot fence must retain the source lease"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::WritableSourceOwnedByAnotherProcess { .. })
    ));
    drop(fence);
    let reopened = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("source lease after snapshot fence release");
    drop(reopened);
}

#[test]
fn source_database_identity_close_waits_for_snapshot_fence_before_release() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let destination_root = tempdir().expect("snapshot root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let destination = destination_root.path().join("snapshot.db");
    let fence = database
        .snapshot_to_path_with_write_fence(&destination)
        .expect("snapshot write fence");
    let error = database
        .close()
        .expect_err("close must retain a live snapshot lease");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SnapshotsActive)
    ));
    drop(fence);

    let reopened = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("source lease after fence close");
    reopened.close().expect("explicit source close");
}

#[test]
fn source_database_identity_lease_rejects_changed_epoch_before_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    database
        .connection
        .execute(
            "UPDATE source_lease SET lease_epoch = lease_epoch + 1 WHERE singleton = 1",
            [],
        )
        .expect("advance lease epoch behind the retained owner");

    let error = database
        .set_metadata("lease-stale-epoch", "must-not-write")
        .expect_err("stale source lease epoch must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::Superseded { .. })
    ));
    assert_eq!(
        database
            .get_metadata("lease-stale-epoch")
            .expect("read stale epoch marker"),
        None
    );
}

#[test]
fn source_database_identity_lease_renews_after_idle_before_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    database
        .connection
        .execute(
            "UPDATE source_lease SET expires_at = acquired_at WHERE singleton = 1",
            [],
        )
        .expect("expire source lease while owner remains live");

    database
        .set_metadata("lease-renewed", "after-idle")
        .expect("live owner should renew before mutation");
    let (acquired_at, expires_at) = database
        .connection
        .query_row(
            "SELECT acquired_at, expires_at FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("renewed lease expiry");
    assert!(expires_at > acquired_at);
    assert_eq!(
        database
            .get_metadata("lease-renewed")
            .expect("renewed marker"),
        Some(String::from("after-idle"))
    );
}

#[test]
fn source_database_identity_lease_does_not_renew_healthy_writes() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let control_changes_before = database
        .source_lease_control_total_changes_for_tests()
        .expect("physical source lease")
        .expect("control change count");
    let expiry_before: i64 = database
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("initial lease expiry");

    database
        .set_metadata("healthy-renewal-one", "one")
        .expect("first healthy source write");
    let expiry_after_first: i64 = database
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("lease expiry after first healthy write");
    database
        .set_metadata("healthy-renewal-two", "two")
        .expect("second healthy source write");
    let expiry_after_second: i64 = database
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("lease expiry after second healthy write");

    assert_eq!(expiry_after_first, expiry_before);
    assert_eq!(expiry_after_second, expiry_before);
    assert_eq!(
        database
            .source_lease_control_total_changes_for_tests()
            .expect("physical source lease")
            .expect("control change count"),
        control_changes_before,
        "healthy source writes must not renew the retained lease"
    );
}

#[test]
fn source_database_identity_lease_rejects_unknown_database_layout_unchanged() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database_path = database_root.path().join(DB_FILE_NAME);
    let connection = Connection::open(&database_path).expect("unknown source database");
    connection
        .execute_batch("PRAGMA user_version = 0;")
        .expect("unknown source database version");
    drop(connection);
    let before = std::fs::read(&database_path).expect("unknown database bytes before open");

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("unknown source database layout must not be adopted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::MigrationRequired { .. })
    ));
    assert_eq!(
        std::fs::read(&database_path).expect("unknown database bytes after open"),
        before
    );
    let remaining = Connection::open(&database_path)
        .expect("reopen unknown source database")
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'source_lease'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("inspect unknown source schema");
    assert_eq!(remaining, 0);
}

#[test]
fn source_database_identity_lease_reports_missing_media_root_before_open_without_bootstrap() {
    let app_root = tempdir().expect("app root");
    let source_parent = tempdir().expect("source parent");
    let source_root = source_parent.path().join("missing-media-root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database_path = database_root.path().join(DB_FILE_NAME);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        &source_root,
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("a missing media root must reject before opening the database"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SourceRootUnavailable { .. })
    ));
    assert!(
        !database_path.exists(),
        "a missing media root must not bootstrap a source database"
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_lease_rejects_source_root_replacement() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let displaced = database_root.path().with_extension("displaced");
    std::fs::rename(database_root.path(), &displaced).expect("displace source root");
    std::fs::create_dir(database_root.path()).expect("replacement source root");

    let error = database
        .set_metadata("lease-stale-root", "must-not-write")
        .expect_err("replaced source root must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::DatabaseRootReplaced { .. })
    ));
    assert!(!database_root.path().join(DB_FILE_NAME).exists());
}

#[cfg(unix)]
#[test]
fn source_database_identity_lease_reports_missing_media_root_as_unavailable() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let displaced = source_root.path().with_extension("displaced");
    std::fs::rename(source_root.path(), &displaced).expect("remove media source root");

    let error = database
        .set_metadata("missing-media-root", "must-not-write")
        .expect_err("missing media root must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SourceRootUnavailable { .. })
    ));

    assert_eq!(
        database
            .get_metadata("missing-media-root")
            .expect("read rejected metadata key"),
        None,
        "rejected source-root mutation must not write metadata"
    );
    std::fs::rename(&displaced, source_root.path()).expect("restore media source root");
    database
        .set_metadata("missing-media-root", "restored")
        .expect("same-owner mutation after media root restore");
    assert_eq!(
        database
            .get_metadata("missing-media-root")
            .expect("read restored metadata"),
        Some(String::from("restored"))
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_lease_reports_missing_profile_child_as_database_unavailable() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-missing-database-root";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned catalog database");
    let child_path = profile_guard.profile_root().join(profile_child);
    let displaced = profile_guard
        .profile_root()
        .join("identity-missing-database-root-displaced");
    std::fs::rename(&child_path, &displaced).expect("remove profile child database root");

    let error = database
        .set_metadata("missing-profile-child", "must-not-write")
        .expect_err("missing profile child must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::DatabaseRootUnavailable { .. })
    ));

    assert_eq!(
        database
            .get_metadata("missing-profile-child")
            .expect("read rejected metadata key"),
        None,
        "rejected profile-child mutation must not write metadata"
    );
    std::fs::rename(&displaced, &child_path).expect("restore profile child database root");
    database
        .set_metadata("missing-profile-child", "restored")
        .expect("same-owner mutation after profile child restore");
    assert_eq!(
        database
            .get_metadata("missing-profile-child")
            .expect("read restored metadata"),
        Some(String::from("restored"))
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_lease_retains_media_root_separately_from_database_root() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let (media_root_identity, database_root_identity): (String, String) = database
        .connection
        .query_row(
            "SELECT media_root_identity, database_root_identity
             FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored source-root identities");
    assert!(!media_root_identity.is_empty());
    assert!(!database_root_identity.is_empty());
    assert_ne!(media_root_identity, database_root_identity);

    let displaced = source_root.path().with_extension("displaced");
    std::fs::rename(source_root.path(), &displaced).expect("displace media source root");
    std::fs::create_dir(source_root.path()).expect("replacement media source root");

    let error = database
        .set_metadata("media-root-replacement", "must-not-write")
        .expect_err("replaced media source root must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SourceRootReplaced { .. })
    ));
    assert_eq!(
        database
            .get_metadata("media-root-replacement")
            .expect("read rejected marker"),
        None
    );
    assert!(database_root.path().join(DB_FILE_NAME).is_file());
}

#[cfg(unix)]
#[test]
fn source_database_identity_lease_rejects_sidecar_replacement() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let lock_path = database_root.path().join(".wavecrate-source-lease.lock");
    let displaced = database_root
        .path()
        .join(".wavecrate-source-lease.lock.old");
    std::fs::rename(&lock_path, &displaced).expect("displace source lease sidecar");
    let replacement_bytes = b"replacement source lease sidecar";
    std::fs::write(&lock_path, replacement_bytes).expect("replacement source lease sidecar");

    let error = database
        .set_metadata("lease-stale-sidecar", "must-not-write")
        .expect_err("replaced source lease sidecar must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SidecarReplaced { .. })
    ));
    assert_eq!(
        std::fs::read(&lock_path).expect("replacement sidecar bytes"),
        replacement_bytes
    );
}

#[test]
#[ignore = "spawned by physical-source lease process tests"]
fn source_lease_process_helper() {
    let base = PathBuf::from(
        std::env::var(SOURCE_LEASE_HELPER_BASE_ENV).expect("source lease helper base"),
    );
    let profile =
        std::env::var(SOURCE_LEASE_HELPER_PROFILE_ENV).expect("source lease helper profile");
    let source_root = PathBuf::from(
        std::env::var(SOURCE_LEASE_HELPER_SOURCE_ROOT_ENV)
            .expect("source lease helper source root"),
    );
    let database_root = PathBuf::from(
        std::env::var(SOURCE_LEASE_HELPER_DATABASE_ROOT_ENV)
            .expect("source lease helper database root"),
    );
    let mode = std::env::var(SOURCE_LEASE_HELPER_MODE_ENV).expect("source lease helper mode");
    let _base_guard = ConfigBaseGuard::set(base);
    let _profile_selection = PersistenceProfileGuard::named(profile);
    let profile_guard = WritableProfileGuard::acquire_current().expect("helper profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        &source_root,
        &profile_guard,
        &database_root,
    )
    .expect("helper source database");
    println!("READY");
    io::stdout()
        .flush()
        .expect("flush source lease helper handshake");
    let mut command = String::new();
    io::stdin()
        .read_line(&mut command)
        .expect("read source lease helper command");
    match (mode.as_str(), command.trim()) {
        ("release", "release") => drop(database),
        ("crash", "crash") => {
            database
                .connection
                .execute(
                    "UPDATE source_lease
                     SET state = 'active', released_at = NULL, expires_at = acquired_at
                     WHERE singleton = 1",
                    [],
                )
                .expect("expire source lease before abrupt exit");
            // Exit without Rust destructors so both OS locks are released by the kernel while the
            // active durable row remains available for the same-profile recovery test.
            std::process::exit(73);
        }
        _ => panic!("unexpected source lease helper command"),
    }
}

#[test]
fn source_database_identity_rejects_profile_legacy_layout_without_renaming() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-legacy-layout";
    let binding = BoundDatabaseRoot::for_profile_child(&profile_guard, Path::new(profile_child))
        .expect("profile child");
    let child_path = profile_guard.profile_root().join(profile_child);
    let legacy_path = child_path.join(LEGACY_DB_FILE_NAME);
    let legacy_bytes = b"legacy database bytes retained for migration";
    std::fs::write(&legacy_path, legacy_bytes).expect("legacy bytes");

    let error = match open_source_database_with_profile_guard_for_test(
        source_root.path(),
        Arc::new(binding),
        &profile_guard,
        SourceDatabaseOpenMode::Full,
        None,
        None,
    ) {
        Ok(_) => panic!("profile-owned legacy migration must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::LegacyDatabaseMigrationRequiresBoundRoot { .. }
    ));
    assert_eq!(
        std::fs::read(&legacy_path).expect("retained legacy bytes"),
        legacy_bytes
    );
    assert!(!child_path.join(DB_FILE_NAME).exists());
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_before_sqlite_open() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-before-open";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("initial profile-owned database");
    drop(database);

    let child_path = profile_guard.profile_root().join(profile_child);
    let database_path = child_path.join(DB_FILE_NAME);
    let replacement_path = child_path.join(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement bytes must remain untouched";
    let gate_database_path = database_path.clone();
    let gate_replacement_path = replacement_path.clone();
    let gate = move |_opened_path: &Path| {
        std::fs::rename(&gate_database_path, &gate_replacement_path)
            .expect("displace original database");
        std::fs::write(&gate_database_path, replacement_bytes).expect("write replacement");
    };
    let binding = BoundDatabaseRoot::for_profile_child(&profile_guard, Path::new(profile_child))
        .expect("profile child");

    let error = match open_source_database_with_profile_guard_for_test(
        source_root.path(),
        Arc::new(binding),
        &profile_guard,
        SourceDatabaseOpenMode::Full,
        None,
        Some(&gate),
    ) {
        Ok(_) => panic!("replacement before SQLite open must fail closed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[test]
fn source_database_identity_does_not_recreate_missing_original() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-missing-original",
    )
    .expect("profile-owned database");
    let expected_database = database
        .database_file_binding
        .as_ref()
        .expect("retained database identity")
        .clone();
    let database_path = profile_guard
        .profile_root()
        .join("identity-missing-original")
        .join(DB_FILE_NAME);
    drop(database);
    std::fs::remove_file(&database_path).expect("remove original database");

    let error = match open_source_database_with_profile_guard_for_test(
        source_root.path(),
        Arc::clone(expected_database.root()),
        &profile_guard,
        SourceDatabaseOpenMode::Full,
        Some(&expected_database),
        None,
    ) {
        Ok(_) => panic!("missing original database must not be recreated"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert!(!database_path.exists());
}

#[test]
fn source_database_identity_lease_does_not_recreate_database_with_existing_sidecar() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let lock_path = database_root.path().join(".wavecrate-source-lease.lock");
    let lock_bytes = b"preexisting source lease diagnostic";
    std::fs::write(&lock_path, lock_bytes).expect("preexisting lease sidecar");
    let database_path = database_root.path().join(DB_FILE_NAME);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("missing database with an existing sidecar must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::DatabaseMissingWithExistingLease { .. })
    ));
    assert!(!database_path.exists());
    assert_eq!(
        std::fs::read(&lock_path).expect("retained lease sidecar"),
        lock_bytes
    );
}

#[test]
fn source_database_identity_lease_missing_row_is_retained_without_rebootstrap() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let database_path = database_root.path().join(DB_FILE_NAME);
    database
        .connection
        .execute("DELETE FROM source_lease", [])
        .expect("remove lease row for retention fixture");
    drop(database);
    let before = std::fs::read(&database_path).expect("database bytes before retry");

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("missing lease row must not reset epoch one"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::LeaseStateUnavailable { .. })
    ));
    assert_eq!(
        std::fs::read(&database_path).expect("retained database bytes"),
        before
    );
    let row_count: i64 = rusqlite::Connection::open(&database_path)
        .expect("inspect retained database")
        .query_row("SELECT COUNT(*) FROM source_lease", [], |row| row.get(0))
        .expect("retained lease table");
    assert_eq!(row_count, 0);
}

#[test]
fn source_database_identity_lease_bootstraps_fresh_sidecar_on_current_legacy_schema() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");

    let legacy = SourceDatabase::open_for_source_write(database_root.path())
        .expect("seed current legacy source schema");
    legacy
        .set_metadata("legacy-row", "preserve-me")
        .expect("seed current legacy source row");
    drop(legacy);
    let database_path = database_root.path().join(DB_FILE_NAME);
    let lease_path = database_root.path().join(".wavecrate-source-lease.lock");
    let legacy_row: String = Connection::open(&database_path)
        .expect("inspect current legacy source database")
        .query_row(
            "SELECT value FROM metadata WHERE key = 'legacy-row'",
            [],
            |row| row.get(0),
        )
        .expect("current legacy source row");
    assert_eq!(legacy_row, "preserve-me");

    let diagnostic_sidecar = b"prior source lease sidecar";
    std::fs::write(&lease_path, diagnostic_sidecar).expect("preexisting source sidecar");
    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("preexisting sidecar must not initialize a missing lease table"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::LeaseStateUnavailable { .. })
    ));
    assert_eq!(
        std::fs::read(&lease_path).expect("retained diagnostic sidecar"),
        diagnostic_sidecar
    );
    assert_eq!(
        Connection::open(&database_path)
            .expect("reopen current legacy source database")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'source_lease'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("inspect missing lease table"),
        0
    );

    std::fs::remove_file(&lease_path).expect("remove diagnostic sidecar");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("fresh sidecar may initialize current legacy schema");
    assert_eq!(
        database
            .get_metadata("legacy-row")
            .expect("preserved current legacy source row")
            .as_deref(),
        Some("preserve-me")
    );
    let (epoch, state): (i64, String) = database
        .connection
        .query_row(
            "SELECT lease_epoch, state FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("newly bootstrapped lease row");
    assert_eq!((epoch, state), (1, String::from("active")));
}

#[test]
fn source_database_identity_rejects_wrong_current_base_schema_without_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");

    let fixtures = [
        (
            "metadata",
            "CREATE TABLE metadata (key BLOB PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE wav_files (
                 path TEXT PRIMARY KEY,
                 file_size INTEGER NOT NULL,
                 modified_ns INTEGER NOT NULL
             );",
        ),
        (
            "wav_files",
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE wav_files (
                 path TEXT PRIMARY KEY,
                 file_size TEXT NOT NULL,
                 modified_ns INTEGER NOT NULL
             );",
        ),
    ];

    for (fixture_name, schema_sql) in fixtures {
        let database_root = tempdir().expect("database root");
        let database_path = database_root.path().join(DB_FILE_NAME);
        let connection = Connection::open(&database_path).expect("wrong-shape database");
        connection
            .execute_batch(schema_sql)
            .expect("wrong-shape schema");
        connection
            .pragma_update(None, "user_version", current_schema_version())
            .expect("current schema stamp");
        drop(connection);
        let before = std::fs::read(&database_path).expect("wrong-shape bytes before open");

        let error = match SourceDatabase::open_for_source_write_with_profile_guard(
            source_root.path(),
            &profile_guard,
            database_root.path(),
        ) {
            Ok(_) => panic!("{fixture_name} wrong-shape schema must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                SourceDbError::SourceLease(SourceLeaseError::SchemaUnavailable { .. })
            ),
            "{fixture_name} wrong-shape error: {error:?}"
        );
        assert_eq!(
            std::fs::read(&database_path).expect("wrong-shape bytes after open"),
            before,
            "{fixture_name} database must remain unchanged"
        );
        let lease_table_count: i64 = Connection::open(&database_path)
            .expect("inspect wrong-shape database")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'source_lease'",
                [],
                |row| row.get(0),
            )
            .expect("inspect lease table absence");
        assert_eq!(
            lease_table_count, 0,
            "{fixture_name} lease bootstrap must not mutate schema"
        );
    }
}

#[test]
fn source_database_identity_lease_rejects_lease_trigger_without_mutating_database() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let database_path = database_root.path().join(DB_FILE_NAME);
    database
        .set_metadata("trigger-preservation", "before-trigger")
        .expect("source marker before trigger");
    drop(database);

    let direct = Connection::open(&database_path).expect("open source database for fixture");
    direct
        .execute_batch(
            "CREATE TRIGGER source_lease_audit
             AFTER UPDATE ON source_lease
             BEGIN
                 SELECT 1;
             END;",
        )
        .expect("install source lease trigger fixture");
    drop(direct);
    let sqlite_artifacts = |path: &Path| {
        [
            String::new(),
            String::from("-wal"),
            String::from("-shm"),
            String::from("-journal"),
        ]
        .into_iter()
        .map(|suffix| {
            let artifact = if suffix.is_empty() {
                path.to_path_buf()
            } else {
                let mut name = path.as_os_str().to_os_string();
                name.push(suffix);
                PathBuf::from(name)
            };
            (artifact.clone(), std::fs::read(&artifact).ok())
        })
        .collect::<Vec<_>>()
    };
    let before = sqlite_artifacts(&database_path);

    let error = match SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    ) {
        Ok(_) => panic!("lease schema trigger must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::SchemaUnavailable { .. })
    ));
    assert_eq!(sqlite_artifacts(&database_path), before);

    let retained = Connection::open(&database_path)
        .expect("reopen retained source database")
        .query_row(
            "SELECT value FROM metadata WHERE key = 'trigger-preservation'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("retained source marker");
    assert_eq!(retained, "before-trigger");
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_before_readiness_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-before-readiness",
    )
    .expect("profile-owned database");
    let database_path = profile_guard
        .profile_root()
        .join("identity-before-readiness")
        .join(DB_FILE_NAME);
    let displaced_path = database_path.with_file_name(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement readiness database remains unchanged";
    let mut readiness = database.readiness_store().expect("owned readiness store");
    std::fs::rename(&database_path, &displaced_path).expect("displace open database");
    std::fs::write(&database_path, replacement_bytes).expect("write replacement");

    let error = readiness
        .mark_temporarily_unavailable("identity-before-readiness", 1)
        .expect_err("replaced profile database must reject readiness mutation");
    assert!(matches!(
        error,
        ReadinessError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    drop(readiness);
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_readiness_reports_database_root_replacement() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let mut readiness = database.readiness_store().expect("owned readiness store");
    let displaced = database_root.path().with_extension("displaced");
    std::fs::rename(database_root.path(), &displaced).expect("displace database root");
    std::fs::create_dir(database_root.path()).expect("replacement database root");

    let error = readiness
        .mark_temporarily_unavailable("database-root-replacement", 1)
        .expect_err("replaced database root must reject readiness mutation");
    assert!(matches!(
        error,
        ReadinessError::SourceLease(SourceLeaseError::DatabaseRootReplaced { .. })
    ));
    drop(readiness);
    drop(database);
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_during_readiness_write() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-during-readiness";
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned database");
    let database_path = profile_guard
        .profile_root()
        .join(profile_child)
        .join(DB_FILE_NAME);
    let displaced_path = database_path.with_file_name(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement during readiness write remains unchanged";
    database
        .connection
        .execute_batch(
            "CREATE TABLE source_database_identity_readiness_marker (
                 id INTEGER PRIMARY KEY,
                 writes INTEGER NOT NULL
             );
             INSERT INTO source_database_identity_readiness_marker (id, writes) VALUES (1, 0);
             CREATE TRIGGER source_database_identity_readiness_write
             AFTER INSERT ON source_readiness_sources
             BEGIN
                 UPDATE source_database_identity_readiness_marker
                 SET writes = writes + 1
                 WHERE id = 1;
             END;",
        )
        .expect("install readiness identity test hook");
    let hook_fired = std::sync::Arc::new(AtomicBool::new(false));
    let replacement_attempted = std::sync::Arc::new(AtomicBool::new(false));
    let replacement_written = std::sync::Arc::new(AtomicBool::new(false));
    let hook_fired_by_sqlite = std::sync::Arc::clone(&hook_fired);
    let attempted_by_sqlite = std::sync::Arc::clone(&replacement_attempted);
    let written_by_sqlite = std::sync::Arc::clone(&replacement_written);
    let callback_database_path = database_path.clone();
    let callback_displaced_path = displaced_path.clone();
    database.connection.update_hook(Some(
        move |_action: rusqlite::hooks::Action,
              _database_name: &str,
              table_name: &str,
              _row_id: i64| {
            if table_name == "source_database_identity_readiness_marker" {
                hook_fired_by_sqlite.store(true, Ordering::SeqCst);
                if !attempted_by_sqlite.swap(true, Ordering::SeqCst)
                    && std::fs::rename(&callback_database_path, &callback_displaced_path).is_ok()
                    && std::fs::write(&callback_database_path, replacement_bytes).is_ok()
                {
                    written_by_sqlite.store(true, Ordering::SeqCst);
                }
            }
        },
    ));
    let mut readiness = database.readiness_store().expect("owned readiness store");
    let target = ReadinessTarget::source(
        "identity-during-readiness",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let targets = [target];
    let origin = readiness
        .capture_publication_origin()
        .expect("readiness publication origin");

    let error = readiness
        .publish_targets(
            &origin,
            &ReadinessTargetPublication::new(
                "identity-during-readiness",
                1,
                1,
                SourceAvailability::Active,
                "readiness-contract-v1",
                &targets,
                1,
            ),
        )
        .expect_err("replacement during readiness write must reject before commit");
    assert!(matches!(
        error,
        ReadinessError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert!(hook_fired.load(Ordering::SeqCst));
    assert!(replacement_written.load(Ordering::SeqCst));
    drop(readiness);
    let row_count = database
        .connection
        .query_row("SELECT COUNT(*) FROM source_readiness_sources", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read rolled-back readiness rows");
    assert_eq!(row_count, 0, "rejected readiness write must roll back");
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_unsafe_profile_sidecar() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let profile_child = "identity-unsafe-sidecar";
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    )
    .expect("profile-owned database");
    drop(database);

    let child_path = profile_guard.profile_root().join(profile_child);
    let database_path = child_path.join(DB_FILE_NAME);
    let sidecar_path = child_path.join(format!("{DB_FILE_NAME}-wal"));
    let outside = tempdir().expect("outside sidecar root");
    let outside_path = outside.path().join("outside-sidecar");
    let outside_bytes = b"outside sidecar bytes";
    std::fs::write(&outside_path, outside_bytes).expect("outside sidecar bytes");
    std::os::unix::fs::symlink(&outside_path, &sidecar_path).expect("sidecar symlink");

    let error = match SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        profile_child,
    ) {
        Ok(_) => panic!("unsafe profile sidecar must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::UnsafeSidecar { .. })
    ));
    assert!(database_path.is_file());
    assert_eq!(
        std::fs::read(&outside_path).expect("outside sidecar bytes"),
        outside_bytes
    );
}

#[cfg(unix)]
#[test]
fn source_database_identity_rejects_replacement_before_mutation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "identity-before-mutation",
    )
    .expect("profile-owned database");
    let database_path = profile_guard
        .profile_root()
        .join("identity-before-mutation")
        .join(DB_FILE_NAME);
    let displaced_path = database_path.with_file_name(".wavecrate.db.displaced");
    let replacement_bytes = b"replacement database remains unchanged";
    std::fs::rename(&database_path, &displaced_path).expect("displace open database");
    std::fs::write(&database_path, replacement_bytes).expect("write replacement");

    let error = database
        .set_metadata("identity-replacement", "must-not-write")
        .expect_err("replaced profile database must reject mutation");
    assert!(matches!(
        error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    let destination_root = tempdir().expect("snapshot destination root");
    let destination = database_path_for(destination_root.path());
    let snapshot_error = database
        .snapshot_to_path(&destination)
        .expect_err("replaced profile database must reject snapshot");
    assert!(matches!(
        snapshot_error,
        SourceDbError::DatabaseIdentity(SourceDatabaseIdentityError::Replaced { .. })
    ));
    assert!(
        !destination.exists(),
        "rejected snapshot must not create output"
    );
    assert_eq!(
        std::fs::read(&database_path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[test]
fn source_authority_handoff_uses_one_shot_confirmation_and_durable_retirement_ack() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let database_path = database_root.path().join(DB_FILE_NAME);
    let registration = database
        .prepare_source_authority()
        .expect("validated source authority");
    assert!(matches!(
        database.prepare_source_authority(),
        Err(SourceAuthorityError::AlreadyPrepared)
    ));
    let (request, mut guard) = registration.split();
    assert!(!request.database_file_identity().is_empty());
    assert!(!request.media_root().identity().is_empty());
    assert!(!request.database_root().identity().is_empty());

    let stale_confirmation = guard.confirm().expect("first live confirmation");
    let fresh_confirmation = guard.confirm().expect("new live confirmation");
    assert!(matches!(
        request.accept_confirmation(stale_confirmation),
        Err(SourceAuthorityError::ConfirmationStale)
    ));
    assert!(matches!(
        request.accept_confirmation(fresh_confirmation.clone().with_expired_deadline_for_tests()),
        Err(SourceAuthorityError::ConfirmationStale)
    ));
    let proof = request
        .accept_confirmation(fresh_confirmation)
        .expect("fresh confirmation");
    assert!(matches!(
        guard.confirm(),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));
    request
        .begin_drain(proof.into_drain_marker())
        .expect("pre-release drain");
    assert!(matches!(
        guard.confirm(),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));

    let acknowledgement = database
        .close_with_source_authority(&mut guard)
        .expect("durable source lease retirement");
    assert_eq!(request.phase_for_tests(), 2);
    request
        .finalize_retirement(acknowledgement.clone())
        .expect("consume retirement acknowledgement");
    assert_eq!(request.phase_for_tests(), 3);
    request
        .finalize_retirement(acknowledgement)
        .expect("duplicate retirement acknowledgement is idempotent");
    let state: String = Connection::open(&database_path)
        .expect("reopen retired source database")
        .query_row(
            "SELECT state FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("retired lease row");
    assert_eq!(state, "released");
}

#[cfg(unix)]
#[test]
fn source_authority_handoff_rejects_replaced_root_and_invalidates_pending_confirmation() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database
        .prepare_source_authority()
        .expect("validated source authority");
    let (request, guard) = registration.split();
    let confirmation = guard.confirm().expect("live confirmation");
    std::fs::remove_dir_all(source_root.path()).expect("remove source root");

    let error = guard
        .confirm()
        .expect_err("replaced source root must reject confirmation");
    assert!(matches!(
        error,
        SourceAuthorityError::Lease(
            SourceLeaseError::SourceRootUnavailable { .. }
                | SourceLeaseError::SourceRootReplaced { .. }
                | SourceLeaseError::SourceRootIdentityUnavailable { .. }
        )
    ));
    assert!(matches!(
        request.accept_confirmation(confirmation),
        Err(SourceAuthorityError::ConfirmationStale)
    ));
}

#[test]
fn source_authority_handoff_rejects_stale_owner_and_cross_source_guard() {
    let app_root = tempdir().expect("app root");
    let source_root_a = tempdir().expect("source root a");
    let database_root_a = tempdir().expect("database root a");
    let source_root_b = tempdir().expect("source root b");
    let database_root_b = tempdir().expect("database root b");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database_a = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root_a.path(),
        &profile_guard,
        database_root_a.path(),
    )
    .expect("source database a");
    let database_b = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root_b.path(),
        &profile_guard,
        database_root_b.path(),
    )
    .expect("source database b");
    let (request_a, guard_a) = database_a
        .prepare_source_authority()
        .expect("authority a")
        .split();
    let (request_b, mut guard_b) = database_b
        .prepare_source_authority()
        .expect("authority b")
        .split();
    let confirmation_a = guard_a.confirm().expect("confirmation a");
    let confirmation_b = guard_b.confirm().expect("confirmation b");
    assert!(matches!(
        request_a.accept_confirmation(confirmation_b.clone()),
        Err(SourceAuthorityError::RegistrationMismatch)
    ));
    let proof_a = request_a
        .accept_confirmation(confirmation_a)
        .expect("matching confirmation a");
    assert!(matches!(
        request_b.begin_drain(proof_a.into_drain_marker()),
        Err(SourceAuthorityError::RegistrationMismatch)
    ));

    database_b
        .connection
        .execute(
            "UPDATE source_lease SET owner_identity = ?1 WHERE singleton = 1",
            [Uuid::new_v4().to_string()],
        )
        .expect("replace owner identity b");
    assert!(matches!(
        guard_b.confirm(),
        Err(SourceAuthorityError::Lease(
            SourceLeaseError::Superseded { .. }
        ))
    ));
    assert!(matches!(
        request_b.accept_confirmation(confirmation_b),
        Err(SourceAuthorityError::ConfirmationStale)
    ));
    assert!(matches!(
        database_a.close_with_source_authority(&mut guard_b),
        Err(SourceAuthorityError::GuardMismatch)
    ));
}

#[test]
fn source_authority_retarget_context_cannot_cross_source_generations() {
    let app_root = tempdir().expect("app root");
    let source_root_a = tempdir().expect("source root a");
    let replacement_root_a = tempdir().expect("replacement root a");
    let source_root_b = tempdir().expect("source root b");
    let replacement_root_b = tempdir().expect("replacement root b");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database_a = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root_a.path(),
        &profile_guard,
        "authority-cross-context-a",
    )
    .expect("source database a");
    let registration_a = database_a.prepare_source_authority().expect("authority a");
    let request_a = registration_a
        .journal_request()
        .expect("authority request a");
    let proof_a = request_a
        .request
        .accept_confirmation(request_a.confirmation)
        .expect("authority proof a");
    request_a
        .request
        .begin_drain(proof_a.into_drain_marker())
        .expect("drain authority a");
    let mut context_a = registration_a
        .detach_for_retarget()
        .expect("detach authority a");

    let database_b = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root_b.path(),
        &profile_guard,
        "authority-cross-context-b",
    )
    .expect("source database b");
    let before_b: (i64, String, String) = database_b
        .connection
        .query_row(
            "SELECT lease_epoch, owner_identity, media_root_identity
             FROM source_lease WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("lease b before crossed retarget");
    let error = match database_b.retarget_catalog_root_with_source_authority(
        replacement_root_b.path(),
        &profile_guard,
        &mut context_a,
    ) {
        Ok(_) => panic!("authority from source a must not retarget source b"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SourceDbError::SourceLease(SourceLeaseError::Superseded { .. })
    ));
    let after_b: (i64, String, String) = Connection::open(
        profile_guard
            .profile_root()
            .join("authority-cross-context-b")
            .join(DB_FILE_NAME),
    )
    .expect("reopen lease b after crossed retarget")
    .query_row(
        "SELECT lease_epoch, owner_identity, media_root_identity
             FROM source_lease WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .expect("lease b after crossed retarget");
    assert_eq!(after_b, before_b);
    assert!(matches!(
        context_a.acknowledgement_after_retarget_commit(),
        Err(SourceAuthorityError::InvalidLifecycle)
    ));
    drop(replacement_root_a);
}

#[test]
fn source_authority_handoff_retains_draining_state_when_snapshot_blocks_retirement() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let destination_root = tempdir().expect("snapshot root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database
        .prepare_source_authority()
        .expect("validated source authority");
    let (request, mut guard) = registration.split();
    let proof = request
        .accept_confirmation(guard.confirm().expect("confirmation"))
        .expect("use proof");
    request
        .begin_drain(proof.into_drain_marker())
        .expect("pre-release drain");
    let snapshot = database
        .snapshot_to_path_with_write_fence(&destination_root.path().join("snapshot.db"))
        .expect("snapshot fence");

    let error = database
        .close_with_source_authority(&mut guard)
        .expect_err("live snapshot must block retirement");
    assert!(matches!(
        error,
        SourceAuthorityError::Lease(SourceLeaseError::SnapshotsActive)
    ));
    assert_eq!(request.phase_for_tests(), 2);
    drop(snapshot);
}

#[test]
fn source_authority_guard_drop_invalidates_pending_use_proof() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database
        .prepare_source_authority()
        .expect("validated source authority");
    let (request, guard) = registration.split();
    let confirmation = guard.confirm().expect("live confirmation");
    let proof = request
        .accept_confirmation(confirmation)
        .expect("accepted use proof");
    drop(guard);
    assert!(matches!(
        request.begin_drain(proof.into_drain_marker()),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));
    assert_eq!(request.phase_for_tests(), 4);
}

#[test]
fn source_effect_pin_rejects_drain_and_guard_loss() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let _app_root_guard =
        AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
            .expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database.prepare_source_authority().expect("authority");
    let (request, guard) = registration.split();
    let proof = request
        .accept_confirmation(guard.confirm().expect("confirmation"))
        .expect("registered source");
    let pin = guard.pin_for_effect().expect("live source pin");
    pin.validate_for_issuance(&request, &profile_guard)
        .expect("issue while registered");
    let in_flight = pin.begin_effect().expect("first effect boundary");
    request
        .begin_drain(proof.into_drain_marker())
        .expect("drain source");
    assert!(matches!(
        pin.begin_effect(),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));
    assert!(matches!(
        pin.validate_for_issuance(&request, &profile_guard),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));
    drop(in_flight);
    drop(guard);
    assert!(matches!(
        pin.begin_effect(),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));
}

#[test]
fn source_effect_pin_cannot_join_another_registered_source() {
    let app_root = tempdir().expect("app root");
    let source_root_a = tempdir().expect("source root a");
    let source_root_b = tempdir().expect("source root b");
    let database_root_a = tempdir().expect("database root a");
    let database_root_b = tempdir().expect("database root b");
    let _app_root_guard =
        AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
            .expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database_a = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root_a.path(),
        &profile_guard,
        database_root_a.path(),
    )
    .expect("source a");
    let database_b = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root_b.path(),
        &profile_guard,
        database_root_b.path(),
    )
    .expect("source b");
    let (request_a, guard_a) = database_a
        .prepare_source_authority()
        .expect("authority a")
        .split();
    let (request_b, guard_b) = database_b
        .prepare_source_authority()
        .expect("authority b")
        .split();
    request_a
        .accept_confirmation(guard_a.confirm().expect("confirmation a"))
        .expect("register a");
    request_b
        .accept_confirmation(guard_b.confirm().expect("confirmation b"))
        .expect("register b");
    let pin_a = guard_a.pin_for_effect().expect("pin a");
    pin_a
        .validate_for_issuance(&request_a, &profile_guard)
        .expect("matching registration");
    assert!(matches!(
        pin_a.validate_for_issuance(&request_b, &profile_guard),
        Err(SourceAuthorityError::RegistrationMismatch)
    ));
}

#[cfg(unix)]
#[test]
fn source_effect_pin_rejects_replaced_root_at_effect_boundary() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let _app_root_guard =
        AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
            .expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database.prepare_source_authority().expect("authority");
    let (request, guard) = registration.split();
    let _proof = request
        .accept_confirmation(guard.confirm().expect("confirmation"))
        .expect("registered source");
    let pin = guard.pin_for_effect().expect("live source pin");
    pin.validate_for_issuance(&request, &profile_guard)
        .expect("same live registration");
    std::fs::remove_dir_all(source_root.path()).expect("remove source root");
    assert!(pin.validate_for_issuance(&request, &profile_guard).is_err());
    assert!(matches!(
        pin.begin_effect(),
        Err(SourceAuthorityError::Lease(
            SourceLeaseError::SourceRootUnavailable { .. }
                | SourceLeaseError::SourceRootReplaced { .. }
                | SourceLeaseError::SourceRootIdentityUnavailable { .. }
        ))
    ));
}

#[test]
fn source_release_waits_for_effect_permit_before_releasing_durable_row() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let _app_root_guard =
        AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
            .expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database.prepare_source_authority().expect("authority");
    let (request, guard) = registration.split();
    let _proof = request
        .accept_confirmation(guard.confirm().expect("confirmation"))
        .expect("registered source");
    let pin = guard.pin_for_effect().expect("live source pin");
    let permit = pin.begin_effect().expect("in-flight effect");
    let lease = Arc::clone(database.source_lease.as_ref().expect("retained lease"));
    let gate = lease.effect_gate();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let releaser = thread::spawn(move || {
        result_tx.send(lease.release()).expect("release result");
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !gate.is_closed_for_tests() {
        assert!(
            std::time::Instant::now() < deadline,
            "release did not close admission"
        );
        thread::yield_now();
    }
    assert!(matches!(
        result_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let state: String = database
        .connection
        .query_row(
            "SELECT state FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("durable lease row");
    assert_eq!(state, "active");
    drop(permit);
    result_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("release completion")
        .expect("released source");
    releaser.join().expect("releaser");
    assert!(matches!(
        pin.begin_effect(),
        Err(SourceAuthorityError::ConfirmationClosed)
    ));
}

#[test]
fn source_authority_expired_use_proof_still_allows_restrictive_drain() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database
        .prepare_source_authority()
        .expect("validated source authority");
    let (request, mut guard) = registration.split();
    let stale_confirmation = guard.confirm().expect("confirmation");
    assert!(matches!(
        request.accept_confirmation(stale_confirmation.with_expired_deadline_for_tests()),
        Err(SourceAuthorityError::ConfirmationStale)
    ));
    let proof = request
        .accept_confirmation(guard.confirm().expect("fresh confirmation"))
        .expect("accepted use proof");
    request
        .begin_drain(proof.with_expired_deadline_for_tests().into_drain_marker())
        .expect("expired physical proof still permits restrictive drain");
    assert_eq!(request.phase_for_tests(), 2);
    let acknowledgement = database
        .close_with_source_authority(&mut guard)
        .expect("durable source lease retirement");
    request
        .finalize_retirement(acknowledgement.clone())
        .expect("consume retirement acknowledgement");
    request
        .finalize_retirement(acknowledgement)
        .expect("duplicate retirement acknowledgement is idempotent");
    assert_eq!(request.phase_for_tests(), 3);
}

#[test]
fn source_authority_confirmation_is_consumed_once_under_concurrent_acceptors() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root = tempdir().expect("database root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root.path(),
    )
    .expect("external source database");
    let registration = database
        .prepare_source_authority()
        .expect("validated source authority");
    let (request, guard) = registration.split();
    let confirmation = guard.confirm().expect("live confirmation");
    let request = Arc::new(request);
    let left_confirmation = confirmation.clone();
    let (left, right) = std::thread::scope(|scope| {
        let left_request = Arc::clone(&request);
        let left = scope.spawn(move || left_request.accept_confirmation(left_confirmation));
        let right_request = Arc::clone(&request);
        let right = scope.spawn(move || right_request.accept_confirmation(confirmation));
        (
            left.join().expect("left acceptor"),
            right.join().expect("right acceptor"),
        )
    });
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(left.is_err() || right.is_err());
}
