use std::{io, path::Path, sync::Arc};

use rusqlite::{Connection, OpenFlags};

use crate::app_dirs::WritableProfileGuard;

mod paths;
mod read_only;

use super::{
    BoundDatabaseRoot, SOURCE_DB_READ_ONLY_ENV, SourceDatabase, SourceDatabaseConnectionRole,
    SourceDbError, sqlite_identity, telemetry, util,
};
use read_only::open_read_only_source_database;

const DEFAULT_WRITABLE_BUSY_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceDatabaseOpenMode {
    Fast,
    Full,
}

pub(super) fn open_source_database_for_role(
    root: &Path,
    role: SourceDatabaseConnectionRole,
) -> Result<SourceDatabase, SourceDbError> {
    open_source_database_for_role_with_database_root(root, root, role)
}

pub(super) fn open_source_database_for_role_with_database_root(
    root: &Path,
    database_root: &Path,
    role: SourceDatabaseConnectionRole,
) -> Result<SourceDatabase, SourceDbError> {
    if role.uses_read_only_connection() || should_open_source_db_read_only() {
        return open_read_only_source_database(root, database_root, role);
    }
    require_profile_ownership()?;
    open_source_database_with_flags(
        root,
        database_root,
        role.open_flags(),
        role.open_mode(),
        role.label(),
        role.busy_timeout_ms(),
    )
}

pub(crate) fn open_source_database(
    root: &Path,
    read_only: bool,
    mode: SourceDatabaseOpenMode,
) -> Result<SourceDatabase, SourceDbError> {
    open_source_database_with_database_root(root, root, read_only, mode)
}

pub(crate) fn open_source_database_with_database_root(
    root: &Path,
    database_root: &Path,
    read_only: bool,
    mode: SourceDatabaseOpenMode,
) -> Result<SourceDatabase, SourceDbError> {
    if read_only {
        return open_read_only_source_database(
            root,
            database_root,
            SourceDatabaseConnectionRole::UiRead,
        );
    }
    require_profile_ownership()?;
    open_source_database_with_flags(
        root,
        database_root,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        mode,
        mode.label(),
        DEFAULT_WRITABLE_BUSY_TIMEOUT_MS,
    )
}

/// Open a writable source database through an already-owned profile capability.
///
/// This low-level opener validates the supplied capability; its authorized caller retains clones
/// of the capability and database-root namespace on the returned database. Path-only writable
/// entrypoints must use [`require_profile_ownership`] instead and therefore fail closed in normal
/// builds.
pub(super) fn open_source_database_with_profile_guard(
    root: &Path,
    database_root: Arc<BoundDatabaseRoot>,
    profile_guard: &WritableProfileGuard,
    mode: SourceDatabaseOpenMode,
    expected_database: Option<&sqlite_identity::DatabaseFileBinding>,
) -> Result<SourceDatabase, SourceDbError> {
    open_source_database_with_profile_guard_inner(
        root,
        database_root,
        profile_guard,
        mode,
        expected_database,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
pub(super) fn open_source_database_with_profile_guard_for_test(
    root: &Path,
    database_root: Arc<BoundDatabaseRoot>,
    profile_guard: &WritableProfileGuard,
    mode: SourceDatabaseOpenMode,
    expected_database: Option<&sqlite_identity::DatabaseFileBinding>,
    before_sqlite_open: Option<&dyn Fn(&Path)>,
) -> Result<SourceDatabase, SourceDbError> {
    open_source_database_with_profile_guard_inner(
        root,
        database_root,
        profile_guard,
        mode,
        expected_database,
        before_sqlite_open,
    )
}

fn open_source_database_with_profile_guard_inner(
    root: &Path,
    database_root: Arc<BoundDatabaseRoot>,
    profile_guard: &WritableProfileGuard,
    mode: SourceDatabaseOpenMode,
    expected_database: Option<&sqlite_identity::DatabaseFileBinding>,
    #[cfg(test)] before_sqlite_open: Option<&dyn Fn(&Path)>,
) -> Result<SourceDatabase, SourceDbError> {
    profile_guard
        .validate_current()
        .map_err(SourceDbError::from)?;
    if !root.is_dir() {
        return Err(SourceDbError::InvalidRoot(root.to_path_buf()));
    }

    let database_name = Path::new(super::DB_FILE_NAME);
    let db_path = database_root.path().join(database_name);
    database_root
        .validate_profile_guard(profile_guard)
        .map_err(SourceDbError::from)?;
    if let Some(expected_database) = expected_database {
        expected_database
            .validate_path()
            .map_err(super::map_database_identity_error)?;
    }
    let current_database_exists =
        validate_profile_database_entry(&database_root, database_name, &db_path)?;
    if !current_database_exists {
        reject_legacy_profile_database(&database_root, &db_path)?;
    }
    sqlite_identity::validate_source_sidecars(&database_root, database_name, &db_path)
        .map_err(super::map_database_identity_error)?;

    let opened = match expected_database {
        Some(_) => {
            sqlite_identity::open_existing_database_file(&database_root, database_name, &db_path)
        }
        None => sqlite_identity::open_database_file(&database_root, database_name, &db_path),
    }
    .map_err(super::map_database_identity_error)?;
    if opened.created
        && opened
            .file
            .metadata()
            .map_err(|source| SourceDbError::InspectSourceDatabasePath {
                path: db_path.clone(),
                source,
            })?
            .len()
            != 0
    {
        return Err(SourceDbError::UnsafeSourceDatabasePath {
            path: db_path,
            reason: "newly created source database was populated before SQLite open",
        });
    }
    if let Some(expected_database) = expected_database {
        if opened.identity != expected_database.identity() {
            return Err(SourceDbError::DatabaseIdentity(
                super::SourceDatabaseIdentityError::Replaced { path: db_path },
            ));
        }
    }

    #[cfg(test)]
    if let Some(gate) = before_sqlite_open {
        gate(&db_path);
    }
    sqlite_identity::validate_source_sidecars(&database_root, database_name, &db_path)
        .map_err(super::map_database_identity_error)?;
    sqlite_identity::validate_database_file_identity(
        &database_root,
        database_name,
        &db_path,
        &opened.identity,
    )
    .map_err(super::map_database_identity_error)?;
    database_root
        .validate_profile_guard(profile_guard)
        .map_err(SourceDbError::from)?;
    let connect_started = std::time::Instant::now();
    let connection = match Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ) {
        Ok(connection) => {
            telemetry::record_open_phase(
                root,
                &db_path,
                mode.label(),
                "connect",
                false,
                connect_started.elapsed(),
                Ok(()),
            );
            connection
        }
        Err(err) => {
            let err = SourceDbError::from(err);
            telemetry::record_open_phase(
                root,
                &db_path,
                mode.label(),
                "connect",
                false,
                connect_started.elapsed(),
                Err(&err),
            );
            return Err(err);
        }
    };
    sqlite_identity::validate_connection_identity(&connection, &db_path, &opened.identity)
        .map_err(super::map_database_identity_error)?;
    sqlite_identity::validate_database_file_identity(
        &database_root,
        database_name,
        &db_path,
        &opened.identity,
    )
    .map_err(super::map_database_identity_error)?;
    sqlite_identity::validate_source_sidecars(&database_root, database_name, &db_path)
        .map_err(super::map_database_identity_error)?;
    drop(opened.file);

    let root_binding: Arc<dyn super::DatabaseRootBindingGuard> = database_root.clone();
    let database_file_binding = Arc::new(sqlite_identity::DatabaseFileBinding::new(
        database_root,
        database_name,
        &db_path,
        opened.identity,
    ));
    let db = SourceDatabase {
        connection,
        db_path: db_path.clone(),
        root: root.to_path_buf(),
        telemetry_label: mode.label(),
        database_root_binding: Some(root_binding),
        database_file_binding: Some(database_file_binding),
        profile_guard: Some(profile_guard.try_clone().map_err(SourceDbError::from)?),
    };
    db.validate_profile_owned_open(profile_guard)?;
    let db = initialize_source_database(
        db,
        root,
        mode,
        mode.label(),
        DEFAULT_WRITABLE_BUSY_TIMEOUT_MS,
    )?;
    db.validate_profile_owned_open(profile_guard)?;
    Ok(db)
}

#[cfg(any(test, feature = "test-support"))]
fn require_profile_ownership() -> Result<(), SourceDbError> {
    Ok(())
}

#[cfg(not(any(test, feature = "test-support")))]
fn require_profile_ownership() -> Result<(), SourceDbError> {
    Err(SourceDbError::ProfileOwnershipRequired)
}

fn open_source_database_with_flags(
    root: &Path,
    database_root: &Path,
    open_flags: OpenFlags,
    mode: SourceDatabaseOpenMode,
    telemetry_label: &'static str,
    busy_timeout_ms: u64,
) -> Result<SourceDatabase, SourceDbError> {
    let open_started = std::time::Instant::now();
    if !root.is_dir() {
        return Err(SourceDbError::InvalidRoot(root.to_path_buf()));
    }

    let database_is_embedded = database_root == root;
    let db_path = if database_is_embedded {
        paths::prepare_writable_db_path_in_existing_root(database_root)?
    } else {
        paths::prepare_writable_db_path(database_root)?
    };
    if !database_is_embedded {
        util::create_parent_if_needed(&db_path)?;
    }
    let connect_started = std::time::Instant::now();
    let connection = match Connection::open_with_flags(&db_path, open_flags) {
        Ok(connection) => {
            telemetry::record_open_phase(
                root,
                &db_path,
                telemetry_label,
                "connect",
                false,
                connect_started.elapsed(),
                Ok(()),
            );
            connection
        }
        Err(err) => {
            let err = SourceDbError::from(err);
            telemetry::record_open_phase(
                root,
                &db_path,
                telemetry_label,
                "connect",
                false,
                connect_started.elapsed(),
                Err(&err),
            );
            telemetry::record_open_total(
                root,
                &db_path,
                telemetry_label,
                false,
                open_started.elapsed(),
                Err(&err),
            );
            return Err(err);
        }
    };
    let db = SourceDatabase {
        connection,
        db_path: db_path.clone(),
        root: root.to_path_buf(),
        telemetry_label,
        database_root_binding: None,
        database_file_binding: None,
        profile_guard: None,
    };
    initialize_source_database(db, root, mode, telemetry_label, busy_timeout_ms)
}

fn initialize_source_database(
    db: SourceDatabase,
    root: &Path,
    mode: SourceDatabaseOpenMode,
    telemetry_label: &'static str,
    busy_timeout_ms: u64,
) -> Result<SourceDatabase, SourceDbError> {
    let db_path = db.db_path.clone();
    let open_started = std::time::Instant::now();
    let pragmas_started = std::time::Instant::now();
    if let Err(err) = db.apply_pragmas(busy_timeout_ms) {
        telemetry::record_open_phase(
            root,
            &db_path,
            telemetry_label,
            "pragmas",
            false,
            pragmas_started.elapsed(),
            Err(&err),
        );
        telemetry::record_open_total(
            root,
            &db_path,
            telemetry_label,
            false,
            open_started.elapsed(),
            Err(&err),
        );
        return Err(err);
    }
    telemetry::record_open_phase(
        root,
        &db_path,
        telemetry_label,
        "pragmas",
        false,
        pragmas_started.elapsed(),
        Ok(()),
    );
    if let Some(profile_guard) = db.profile_guard.as_ref() {
        let identity_started = std::time::Instant::now();
        if let Err(err) = db.validate_profile_owned_open(profile_guard) {
            telemetry::record_open_phase(
                root,
                &db_path,
                telemetry_label,
                "identity",
                false,
                identity_started.elapsed(),
                Err(&err),
            );
            telemetry::record_open_total(
                root,
                &db_path,
                telemetry_label,
                false,
                open_started.elapsed(),
                Err(&err),
            );
            return Err(err);
        }
        telemetry::record_open_phase(
            root,
            &db_path,
            telemetry_label,
            "identity",
            false,
            identity_started.elapsed(),
            Ok(()),
        );
    }
    let schema_started = std::time::Instant::now();
    let schema_result = match mode {
        SourceDatabaseOpenMode::Fast => db.apply_schema_fast(),
        SourceDatabaseOpenMode::Full => db.apply_schema(),
    };
    match schema_result {
        Ok(()) => {
            telemetry::record_open_phase(
                root,
                &db_path,
                telemetry_label,
                "schema",
                false,
                schema_started.elapsed(),
                Ok(()),
            );
        }
        Err(err) => {
            telemetry::record_open_phase(
                root,
                &db_path,
                telemetry_label,
                "schema",
                false,
                schema_started.elapsed(),
                Err(&err),
            );
            telemetry::record_open_total(
                root,
                &db_path,
                telemetry_label,
                false,
                open_started.elapsed(),
                Err(&err),
            );
            return Err(err);
        }
    }
    telemetry::record_open_total(
        root,
        &db_path,
        telemetry_label,
        false,
        open_started.elapsed(),
        Ok(()),
    );
    Ok(db)
}

fn validate_profile_database_entry(
    database_root: &BoundDatabaseRoot,
    database_name: &Path,
    database_path: &Path,
) -> Result<bool, SourceDbError> {
    match database_root.symlink_metadata(database_name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(SourceDbError::UnsafeSourceDatabasePath {
                path: database_path.to_path_buf(),
                reason: "source database files must not be symlinks",
            })
        }
        Ok(metadata) if !metadata.is_file() => Err(SourceDbError::UnsafeSourceDatabasePath {
            path: database_path.to_path_buf(),
            reason: "source database path must be a regular file",
        }),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(SourceDbError::InspectSourceDatabasePath {
            path: database_path.to_path_buf(),
            source,
        }),
    }
}

fn reject_legacy_profile_database(
    database_root: &BoundDatabaseRoot,
    current_path: &Path,
) -> Result<(), SourceDbError> {
    let legacy_name = Path::new(super::LEGACY_DB_FILE_NAME);
    let legacy_path = database_root.path().join(legacy_name);
    match database_root.symlink_metadata(legacy_name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(SourceDbError::UnsafeSourceDatabasePath {
                path: legacy_path,
                reason: "source database files must not be symlinks",
            })
        }
        Ok(metadata) if !metadata.is_file() => Err(SourceDbError::UnsafeSourceDatabasePath {
            path: legacy_path,
            reason: "source database path must be a regular file",
        }),
        Ok(_) => Err(SourceDbError::LegacyDatabaseMigrationRequiresBoundRoot {
            from: legacy_path,
            to: current_path.to_path_buf(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SourceDbError::InspectSourceDatabasePath {
            path: legacy_path,
            source,
        }),
    }
}

impl SourceDatabaseOpenMode {
    fn label(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Full => "full",
        }
    }
}

/// Owned scope for one source DB open counter used by regression tests.
#[cfg(debug_assertions)]
pub struct TestSourceDbOpenCountGuard {
    root: std::path::PathBuf,
}

#[cfg(debug_assertions)]
impl Drop for TestSourceDbOpenCountGuard {
    fn drop(&mut self) {
        telemetry::release_open_total_count_scope(&self.root);
    }
}

#[cfg(debug_assertions)]
impl TestSourceDbOpenCountGuard {
    /// Clear observations accumulated so far while retaining scope ownership.
    pub fn reset(&self) {
        telemetry::reset_open_total_count(&self.root);
    }
}

/// Clear one source DB open counter for a scope and clear it again on drop.
#[cfg(debug_assertions)]
pub fn test_scope_source_db_open_total_count(root: &Path) -> TestSourceDbOpenCountGuard {
    telemetry::acquire_open_total_count_scope(root);
    TestSourceDbOpenCountGuard {
        root: root.to_path_buf(),
    }
}

/// Return the debug-only source DB open count used by regression tests.
#[cfg(debug_assertions)]
pub fn test_source_db_open_total_count(root: &Path) -> usize {
    telemetry::open_total_count(root)
}

pub(super) fn should_open_source_db_read_only() -> bool {
    crate::env_flags::env_var_truthy(SOURCE_DB_READ_ONLY_ENV)
}
