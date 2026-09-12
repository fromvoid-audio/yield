//! Profile-owned SQLite storage for durable operation intents.
//!
//! This store is deliberately smaller than either the source catalog or the global library
//! database. It owns only an operation id, a monotonic insertion sequence, and one bounded
//! encoded record. A successful insert proves that these bytes reached this SQLite database; it
//! does not admit the operation to filesystem execution or grant any other capability. The store
//! uses one exclusive connection, DELETE rollback journaling, and synchronous EXTRA (the
//! strongest rollback-journal mode) and flushes the already-owned profile/child directories on
//! Unix before returning from open. Windows relies on the bundled SQLite VFS for committed file
//! content and deliberately makes no independent directory-entry power-loss guarantee.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;

use crate::app_dirs::{ProfileOwnershipError, WritableProfileGuard};
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;
use crate::sample_sources::db::BoundDatabaseRoot;

use super::codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};
use super::model::{MAX_RECORD_BYTES, OperationDisposition, OperationId, OperationPhase};
use super::validation::{RecordConsistency, classify_record};

/// The filename of the profile-local operation journal database.
pub const JOURNAL_DATABASE_FILE_NAME: &str = "journal.sqlite3";
/// The profile child directory that owns the operation journal database.
pub const JOURNAL_DATABASE_ROOT_NAME: &str = "operation_journal";
/// The SQLite application id reserved for the operation journal.
pub const JOURNAL_APPLICATION_ID: u32 = u32::from_be_bytes(*b"WCJ1");
/// The SQLite user-version written by this journal schema.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

const SQLITE_HEADER_BYTES: usize = 100;
const SQLITE_HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const SQLITE_USER_VERSION_OFFSET: usize = 60;
const SQLITE_APPLICATION_ID_OFFSET: usize = 68;
const SQLITE_JOURNAL_SUFFIX: &str = "-journal";
const SQLITE_WAL_SUFFIX: &str = "-wal";
const SQLITE_SHM_SUFFIX: &str = "-shm";
const JOURNAL_TABLE_NAME: &str = "operation_journal";
const MAX_SCHEMA_OBJECTS: i64 = 8;
const MAX_SCHEMA_COLUMNS: usize = 8;
const MAX_SCHEMA_INDEXES: usize = 8;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const CREATE_JOURNAL_TABLE: &str = "CREATE TABLE operation_journal (
    operation_id TEXT NOT NULL PRIMARY KEY,
    sequence INTEGER NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(length(record) <= 1048576)
)";

/// Errors returned by the profile-owned operation journal store.
#[derive(Debug, Error)]
pub enum OperationJournalStoreError {
    /// The caller did not hold the profile ownership capability, or that capability was replaced.
    #[error(transparent)]
    ProfileOwnership(#[from] ProfileOwnershipError),
    /// SQLite rejected a store operation.
    #[error("operation journal SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A no-follow filesystem operation failed while opening or validating the store.
    #[error("operation journal filesystem operation failed at {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// Existing journal bytes or schema require an audit and were left unchanged.
    #[error("operation journal at {path} requires audit: {reason}")]
    RequiresAudit {
        /// Journal path whose contents were retained.
        path: PathBuf,
        /// Bounded reason for refusing the database.
        reason: String,
    },
    /// The platform cannot provide a verified SQLite connection-file identity check.
    #[error("operation journal cannot verify SQLite connection identity: {reason}")]
    IdentityCheckUnavailable {
        /// Platform or SQLite reason for the fail-closed result.
        reason: String,
    },
    /// The database pathname no longer names the file opened by this store.
    #[error("operation journal database was replaced: {path}")]
    DatabaseReplaced {
        /// Database pathname whose identity changed.
        path: PathBuf,
    },
    /// A SQLite file-control identity check reported an unexpected result.
    #[error("SQLite connection identity check failed for {path}: {reason}")]
    ConnectionIdentityMismatch {
        /// Database path being checked.
        path: PathBuf,
        /// Bounded diagnostic from SQLite or the native identity check.
        reason: String,
    },
    /// The requested operation record was retained but could not be classified as safe data.
    #[error("retained operation journal record {operation_id:?} requires audit: {reason}")]
    RetainedRecord {
        /// Operation id read from the lookup key, when it was parseable.
        operation_id: Option<OperationId>,
        /// Bounded decode or consistency diagnostic.
        reason: String,
    },
    /// A caller attempted to insert an operation that is not at the durable-intent phase.
    #[error("operation journal insert requires IntentDurable phase, found {phase:?}")]
    NotIntentDurable {
        /// Phase supplied by the caller.
        phase: OperationPhase,
    },
    /// A durable intent must carry the capacity plan that was used for admission accounting.
    #[error("operation journal durable intent requires a capacity plan")]
    MissingCapacityPlan,
    /// An intent carried evidence reserved for later execution phases.
    #[error("operation journal durable intent carries execution evidence: {kind}")]
    IntentCarriesExecutionEvidence {
        /// Evidence field that was unexpectedly present.
        kind: &'static str,
    },
    /// A record failed the pure stored-record consistency checks.
    #[error("operation journal record is internally inconsistent: {reason}")]
    InconsistentRecord {
        /// Bounded consistency diagnostic.
        reason: String,
    },
    /// The operation id already exists and was not overwritten.
    #[error("operation journal operation id already exists: {operation_id}")]
    DuplicateOperation {
        /// Colliding operation identity.
        operation_id: OperationId,
    },
    /// A sequence value could not be allocated without exceeding SQLite's integer range.
    #[error("operation journal sequence is exhausted")]
    SequenceExhausted,
}

/// Acknowledgement returned after one intent has committed and passed post-commit validation.
///
/// The receipt proves persistence in this profile-owned journal only. It does not prove
/// filesystem admission, source/global database admission, or publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalInsertReceipt {
    operation_id: OperationId,
    sequence: i64,
    encoded_bytes: usize,
}

impl JournalInsertReceipt {
    /// Return the operation id persisted by the insert.
    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the strictly increasing sequence allocated by the store.
    pub fn sequence(self) -> i64 {
        self.sequence
    }

    /// Return the encoded record length persisted by the store.
    pub fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

/// One profile-owned operation-journal SQLite connection.
///
/// The retained root binding and cloned profile owner are kept until after the SQLite connection
/// is dropped. The store has no raw-connection escape hatch and exposes only bounded point reads.
pub struct OperationJournalStore {
    connection: Connection,
    database_path: PathBuf,
    database_identity: String,
    database_root: BoundDatabaseRoot,
    profile_guard: WritableProfileGuard,
}

impl std::fmt::Debug for OperationJournalStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationJournalStore")
            .field("database_path", &self.database_path)
            .finish_non_exhaustive()
    }
}

impl OperationJournalStore {
    /// Open or create the journal under the retained operation_journal profile child.
    ///
    /// Existing files are accepted only when their SQLite header and private schema match this
    /// store. Missing files are created with no-follow create_new; failures during bootstrap
    /// leave the file in place so a later owner can audit it rather than silently replacing it.
    pub fn open(profile_guard: &WritableProfileGuard) -> Result<Self, OperationJournalStoreError> {
        Self::open_inner(
            profile_guard,
            #[cfg(test)]
            None,
        )
    }

    fn open_inner(
        profile_guard: &WritableProfileGuard,
        #[cfg(test)] before_sqlite_open: Option<&dyn Fn(&Path)>,
    ) -> Result<Self, OperationJournalStoreError> {
        let database_root = BoundDatabaseRoot::for_profile_child(
            profile_guard,
            Path::new(JOURNAL_DATABASE_ROOT_NAME),
        )?;
        database_root.validate_profile_guard(profile_guard)?;
        database_root.sync_namespace(profile_guard)?;

        let database_name = Path::new(JOURNAL_DATABASE_FILE_NAME);
        let database_path = database_root.path().join(database_name);
        reject_unexpected_sidecars(&database_root, &database_path, false)?;

        let (mut file, created) =
            open_database_file(&database_root, database_name, &database_path)?;
        ensure_regular_file(&file, &database_path)?;
        let database_identity =
            stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
                OperationJournalStoreError::RequiresAudit {
                    path: database_path.clone(),
                    reason: String::from("stable database-file identity is unavailable"),
                }
            })?;

        if created {
            if file
                .metadata()
                .map_err(|source| io_error(&database_path, source))?
                .len()
                != 0
            {
                return Err(audit_error(
                    &database_path,
                    "a newly created journal file was populated before SQLite bootstrap",
                ));
            }
        } else {
            validate_sqlite_header(&mut file, &database_path)?;
        }
        #[cfg(test)]
        if let Some(gate) = before_sqlite_open {
            gate(&database_path);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&database_path, flags)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        validate_connection_identity(
            &connection,
            &database_root,
            &database_path,
            &database_identity,
        )?;
        validate_database_file_identity(&database_root, &database_path, &database_identity)?;
        drop(file);

        if created {
            configure_connection(&connection)?;
            bootstrap_schema(&connection)?;
        } else {
            validate_existing_schema(&connection, &database_path)?;
            configure_connection(&connection)?;
        }
        validate_connection_identity(
            &connection,
            &database_root,
            &database_path,
            &database_identity,
        )?;
        verify_connection_configuration(&connection, &database_path)?;
        database_root.sync_namespace(profile_guard)?;
        database_root.validate_profile_guard(profile_guard)?;

        // Clone only after every open, header, schema, pragma, and identity check has succeeded.
        // The clone is the final retained owner moved into the store.
        let retained_profile_guard = profile_guard.try_clone()?;
        Ok(Self {
            connection,
            database_path,
            database_identity,
            database_root,
            profile_guard: retained_profile_guard,
        })
    }

    /// Return one bounded operation record by id.
    pub fn get(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<StoredOperationRecord>, OperationJournalStoreError> {
        self.validate_namespace()?;
        let key = operation_id.to_string();
        let row = self
            .connection
            .query_row(
                "SELECT sequence, length(record) FROM operation_journal WHERE operation_id = ?1",
                params![key],
                |row| {
                    let sequence = row.get::<_, i64>(0)?;
                    let length = row.get::<_, i64>(1)?;
                    Ok((sequence, length))
                },
            )
            .optional()?;
        let Some((sequence, encoded_length)) = row else {
            self.validate_namespace()?;
            return Ok(None);
        };
        if !(0..=MAX_RECORD_BYTES as i64).contains(&encoded_length) {
            return Err(retained_error(
                Some(operation_id),
                format!("stored record length {encoded_length} exceeds the bounded record size"),
            ));
        }
        let encoded = self.connection.query_row(
            "SELECT record FROM operation_journal WHERE operation_id = ?1",
            params![operation_id.to_string()],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        if encoded.len() != encoded_length as usize || encoded.len() > MAX_RECORD_BYTES {
            return Err(retained_error(
                Some(operation_id),
                String::from("stored record length changed or exceeded the bound"),
            ));
        }
        let record =
            decode(&encoded).map_err(|error| retained_decode_error(operation_id, error))?;
        if record.operation_id != operation_id {
            return Err(retained_error(
                Some(operation_id),
                String::from("record identity does not match its SQLite key"),
            ));
        }
        match classify_record(&record) {
            RecordConsistency::Consistent => {
                if sequence <= 0 {
                    return Err(retained_error(
                        Some(operation_id),
                        String::from("stored sequence is not positive"),
                    ));
                }
                self.validate_namespace()?;
                Ok(Some(record))
            }
            RecordConsistency::LegacyMissingPublicationEvidence => Err(retained_error(
                Some(operation_id),
                String::from("legacy record is retained without publication evidence"),
            )),
            RecordConsistency::Invalid(reason) => Err(retained_error(Some(operation_id), reason)),
        }
    }

    /// Insert one internally consistent durable intent without changing any later phase.
    pub fn insert_intent(
        &self,
        record: &StoredOperationRecord,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        validate_intent_record(record)?;
        let encoded = encode(record)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: format!("encoded record exceeds {MAX_RECORD_BYTES} bytes"),
            });
        }

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result = self.insert_inside_transaction(record, &encoded);
        match result {
            Ok(receipt) => {
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let persisted = self.get(record.operation_id)?;
                if persisted.is_none() {
                    return Err(retained_error(
                        Some(record.operation_id),
                        String::from("committed journal row could not be read back"),
                    ));
                }
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn insert_inside_transaction(
        &self,
        record: &StoredOperationRecord,
        encoded: &[u8],
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        let operation_id = record.operation_id;
        let key = operation_id.to_string();
        let already_present: Option<i64> = self
            .connection
            .query_row(
                "SELECT sequence FROM operation_journal WHERE operation_id = ?1",
                params![key.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if already_present.is_some() {
            return Err(OperationJournalStoreError::DuplicateOperation { operation_id });
        }
        let sequence = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM operation_journal",
                [],
                |row| row.get::<_, i64>(0),
            )?
            .checked_add(1)
            .ok_or(OperationJournalStoreError::SequenceExhausted)?;
        self.connection.execute(
            "INSERT INTO operation_journal (operation_id, sequence, record)
             VALUES (?1, ?2, ?3)",
            params![key, sequence, encoded],
        )?;
        Ok(JournalInsertReceipt {
            operation_id,
            sequence,
            encoded_bytes: encoded.len(),
        })
    }

    fn validate_namespace(&self) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        // DELETE mode may retain its ordinary rollback journal while this exclusive connection
        // remains open. WAL/SHM and unsafe/non-regular entries are still rejected.
        reject_unexpected_sidecars(&self.database_root, &self.database_path, true)
    }

    fn validate_namespace_without_sidecars(&self) -> Result<(), OperationJournalStoreError> {
        self.database_root
            .validate_profile_guard(&self.profile_guard)?;
        validate_database_file_identity(
            &self.database_root,
            &self.database_path,
            &self.database_identity,
        )?;
        validate_connection_identity(
            &self.connection,
            &self.database_root,
            &self.database_path,
            &self.database_identity,
        )
    }
}

fn validate_intent_record(
    record: &StoredOperationRecord,
) -> Result<(), OperationJournalStoreError> {
    if record.phase != OperationPhase::IntentDurable {
        return Err(OperationJournalStoreError::NotIntentDurable {
            phase: record.phase,
        });
    }
    if record.capacity_plan.is_none() {
        return Err(OperationJournalStoreError::MissingCapacityPlan);
    }
    if record.disposition != OperationDisposition::None {
        return Err(OperationJournalStoreError::InconsistentRecord {
            reason: String::from("durable intent must have the None disposition"),
        });
    }
    for (present, kind) in [
        (record.prepared.is_some(), "prepared"),
        (record.staged.is_some(), "staged"),
        (record.published().is_some(), "published"),
        (
            record.absent_final_recovery_observation.is_some(),
            "absent-final recovery observation",
        ),
        (
            record.absent_final_transaction_owned_proof.is_some(),
            "absent-final transaction-owned proof",
        ),
        (
            record.absent_final_adoption_evidence.is_some(),
            "absent-final adoption evidence",
        ),
        (
            record.replacement_qualification.is_some(),
            "replacement qualification",
        ),
    ] {
        if present {
            return Err(OperationJournalStoreError::IntentCarriesExecutionEvidence { kind });
        }
    }
    match classify_record(record) {
        RecordConsistency::Consistent => Ok(()),
        RecordConsistency::LegacyMissingPublicationEvidence => {
            Err(OperationJournalStoreError::InconsistentRecord {
                reason: String::from("legacy publication evidence is not an admissible intent"),
            })
        }
        RecordConsistency::Invalid(reason) => {
            Err(OperationJournalStoreError::InconsistentRecord { reason })
        }
    }
}

fn open_database_file(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
) -> Result<(File, bool), OperationJournalStoreError> {
    match root.open_file_nofollow(name, false) {
        Ok(file) => Ok((file, false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match root.open_file_nofollow(name, true) {
                Ok(file) => Ok((file, true)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => root
                    .open_file_nofollow(name, false)
                    .map(|file| (file, false))
                    .map_err(|source| io_error(path, source)),
                Err(source) => Err(io_error(path, source)),
            }
        }
        Err(source) => Err(io_error(path, source)),
    }
}

fn ensure_regular_file(file: &File, path: &Path) -> Result<(), OperationJournalStoreError> {
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.is_file() {
        return Err(audit_error(path, "journal database is not a regular file"));
    }
    Ok(())
}

fn reject_unexpected_sidecars(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    allow_rollback_journal: bool,
) -> Result<(), OperationJournalStoreError> {
    for suffix in [SQLITE_JOURNAL_SUFFIX, SQLITE_WAL_SUFFIX, SQLITE_SHM_SUFFIX] {
        let name = format!("{JOURNAL_DATABASE_FILE_NAME}{suffix}");
        match root.symlink_metadata(Path::new(&name)) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(audit_error(
                        database_path,
                        format!("SQLite sidecar {name} is a symlink"),
                    ));
                }
                if !metadata.is_file() {
                    return Err(audit_error(
                        database_path,
                        format!("SQLite sidecar {name} is not a regular file"),
                    ));
                }
                if suffix == SQLITE_JOURNAL_SUFFIX && allow_rollback_journal {
                    continue;
                }
                return Err(audit_error(
                    database_path,
                    format!("unexpected SQLite sidecar {name} is present"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(database_path, source)),
        }
    }
    Ok(())
}

fn validate_sqlite_header(file: &mut File, path: &Path) -> Result<(), OperationJournalStoreError> {
    let length = file
        .metadata()
        .map_err(|source| io_error(path, source))?
        .len();
    if length < SQLITE_HEADER_BYTES as u64 {
        return Err(audit_error(
            path,
            "existing SQLite file is shorter than its header",
        ));
    }
    let mut header = [0_u8; SQLITE_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut header))
        .map_err(|source| io_error(path, source))?;
    if &header[..SQLITE_HEADER_MAGIC.len()] != SQLITE_HEADER_MAGIC {
        return Err(audit_error(path, "existing file is not SQLite format 3"));
    }
    if header[18] != 1 || header[19] != 1 {
        return Err(audit_error(
            path,
            format!(
                "existing SQLite file uses unsupported format versions write={} read={}",
                header[18], header[19]
            ),
        ));
    }
    let application_id = u32::from_be_bytes(
        header[SQLITE_APPLICATION_ID_OFFSET..SQLITE_APPLICATION_ID_OFFSET + 4]
            .try_into()
            .expect("SQLite application-id field has fixed width"),
    );
    let user_version = u32::from_be_bytes(
        header[SQLITE_USER_VERSION_OFFSET..SQLITE_USER_VERSION_OFFSET + 4]
            .try_into()
            .expect("SQLite user-version field has fixed width"),
    );
    if application_id != JOURNAL_APPLICATION_ID {
        return Err(audit_error(
            path,
            format!("SQLite application id {application_id:#x} is not the operation-journal id"),
        ));
    }
    if user_version != JOURNAL_SCHEMA_VERSION {
        return Err(audit_error(
            path,
            format!(
                "SQLite user version {user_version} is not the supported journal schema {}",
                JOURNAL_SCHEMA_VERSION
            ),
        ));
    }
    Ok(())
}

fn validate_existing_schema(
    connection: &Connection,
    path: &Path,
) -> Result<(), OperationJournalStoreError> {
    let object_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if !(1..=MAX_SCHEMA_OBJECTS).contains(&object_count) {
        return Err(audit_error(
            path,
            format!("unexpected SQLite schema object count {object_count}"),
        ));
    }
    let objects: Vec<(String, String)> = connection
        .prepare(
            "SELECT name, type FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name LIMIT ?1",
        )?
        .query_map(params![MAX_SCHEMA_OBJECTS + 1], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if objects.len() != 1 || objects[0] != (String::from(JOURNAL_TABLE_NAME), String::from("table"))
    {
        return Err(audit_error(
            path,
            "existing SQLite schema has unexpected objects",
        ));
    }

    let table_sql: String = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![JOURNAL_TABLE_NAME],
        |row| row.get(0),
    )?;
    if normalize_sql(&table_sql) != normalize_sql(CREATE_JOURNAL_TABLE) {
        return Err(audit_error(
            path,
            "existing operation-journal table schema differs",
        ));
    }

    let columns: Vec<(String, String, i64, i64)> = connection
        .prepare("PRAGMA table_info(operation_journal)")?
        .query_map([], |row| {
            Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(5)?))
        })?
        .take(MAX_SCHEMA_COLUMNS)
        .collect::<Result<Vec<_>, _>>()?;
    let expected_columns = vec![
        (String::from("operation_id"), String::from("TEXT"), 1, 1),
        (String::from("sequence"), String::from("INTEGER"), 1, 0),
        (String::from("record"), String::from("BLOB"), 1, 0),
    ];
    if columns != expected_columns {
        return Err(audit_error(
            path,
            "existing operation-journal columns differ",
        ));
    }

    let index_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_index_list('operation_journal')",
        [],
        |row| row.get(0),
    )?;
    if !(1..=MAX_SCHEMA_INDEXES as i64).contains(&index_count) {
        return Err(audit_error(
            path,
            "operation-journal unique indexes are unavailable",
        ));
    }
    Ok(())
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}

fn configure_connection(connection: &Connection) -> Result<(), OperationJournalStoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = DELETE;
         PRAGMA synchronous = EXTRA;
         PRAGMA locking_mode = EXCLUSIVE;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}

fn bootstrap_schema(connection: &Connection) -> Result<(), OperationJournalStoreError> {
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = connection
        .execute_batch(&format!(
            "PRAGMA application_id = {JOURNAL_APPLICATION_ID};
             PRAGMA user_version = {JOURNAL_SCHEMA_VERSION};
             {CREATE_JOURNAL_TABLE};"
        ))
        .map(|_| ());
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error.into())
        }
    }
}

fn verify_connection_configuration(
    connection: &Connection,
    path: &Path,
) -> Result<(), OperationJournalStoreError> {
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let locking_mode: String =
        connection.pragma_query_value(None, "locking_mode", |row| row.get(0))?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let user_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete")
        || !locking_mode.eq_ignore_ascii_case("exclusive")
        || synchronous != 3
        || application_id != i64::from(JOURNAL_APPLICATION_ID)
        || user_version != i64::from(JOURNAL_SCHEMA_VERSION)
    {
        return Err(audit_error(
            path,
            format!(
                "SQLite configuration is journal_mode={journal_mode}, locking_mode={locking_mode}, synchronous={synchronous}, application_id={application_id:#x}, user_version={user_version}"
            ),
        ));
    }
    Ok(())
}

fn validate_database_file_identity(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    let file = root
        .open_file_nofollow(Path::new(JOURNAL_DATABASE_FILE_NAME), false)
        .map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                OperationJournalStoreError::DatabaseReplaced {
                    path: database_path.to_path_buf(),
                }
            } else {
                io_error(database_path, source)
            }
        })?;
    ensure_regular_file(&file, database_path)?;
    let actual_identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
        OperationJournalStoreError::IdentityCheckUnavailable {
            reason: String::from("stable identity is unavailable for the open journal file"),
        }
    })?;
    if actual_identity != expected_identity {
        return Err(OperationJournalStoreError::DatabaseReplaced {
            path: database_path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_connection_identity(
    connection: &Connection,
    _root: &BoundDatabaseRoot,
    path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    #[cfg(unix)]
    {
        let mut moved = 0_i32;
        let return_code = unsafe {
            // Connection::handle is valid for the duration of this borrow. SQLite writes only
            // the boolean result into moved; the operation does not retain any pointer.
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                (&mut moved as *mut i32).cast(),
            )
        };
        if return_code != rusqlite::ffi::SQLITE_OK {
            return Err(OperationJournalStoreError::IdentityCheckUnavailable {
                reason: format!("SQLITE_FCNTL_HAS_MOVED returned {return_code}"),
            });
        }
        if moved != 0 {
            return Err(OperationJournalStoreError::DatabaseReplaced {
                path: path.to_path_buf(),
            });
        }
        let _ = expected_identity;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::ptr::null_mut;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        let mut native_handle = HANDLE(null_mut());
        let return_code = unsafe {
            // SQLite returns a borrowed native handle. We inspect it and never close or wrap it
            // in an owning Rust file object.
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                (&mut native_handle as *mut HANDLE).cast(),
            )
        };
        if return_code != rusqlite::ffi::SQLITE_OK {
            return Err(OperationJournalStoreError::IdentityCheckUnavailable {
                reason: format!("SQLITE_FCNTL_WIN32_GET_HANDLE returned {return_code}"),
            });
        }
        if native_handle.is_invalid() {
            return Err(OperationJournalStoreError::IdentityCheckUnavailable {
                reason: String::from("SQLite returned an invalid native file handle"),
            });
        }
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(native_handle, &mut information) }.map_err(
            |error| OperationJournalStoreError::IdentityCheckUnavailable {
                reason: format!("GetFileInformationByHandle failed: {error}"),
            },
        )?;
        let actual_identity = format_windows_identity(&information);
        if actual_identity != expected_identity {
            return Err(OperationJournalStoreError::ConnectionIdentityMismatch {
                path: path.to_path_buf(),
                reason: String::from("SQLite native handle identifies a different file"),
            });
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (connection, path, expected_identity);
        Err(OperationJournalStoreError::IdentityCheckUnavailable {
            reason: String::from("no verified SQLite connection identity control exists"),
        })
    }
}

#[cfg(windows)]
fn format_windows_identity(
    information: &windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
) -> String {
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let creation_time = (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
        | u64::from(information.ftCreationTime.dwLowDateTime);
    format!(
        "windows:{}:{}:{}",
        information.dwVolumeSerialNumber, file_index, creation_time
    )
}

fn io_error(path: &Path, source: io::Error) -> OperationJournalStoreError {
    OperationJournalStoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn audit_error(path: &Path, reason: impl Into<String>) -> OperationJournalStoreError {
    OperationJournalStoreError::RequiresAudit {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn retained_error(
    operation_id: Option<OperationId>,
    reason: impl Into<String>,
) -> OperationJournalStoreError {
    OperationJournalStoreError::RetainedRecord {
        operation_id,
        reason: reason.into(),
    }
}

fn retained_decode_error(
    operation_id: OperationId,
    error: DecodeError,
) -> OperationJournalStoreError {
    retained_error(Some(operation_id), error.to_string())
}

impl From<EncodeError> for OperationJournalStoreError {
    fn from(error: EncodeError) -> Self {
        Self::InconsistentRecord {
            reason: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::test_runtime::TestRuntimeGuard;

    struct Fixture {
        _runtime: TestRuntimeGuard,
        _base: TempDir,
        _config: ConfigBaseGuard,
        _profile: PersistenceProfileGuard,
        owner: WritableProfileGuard,
    }

    fn fixture() -> Fixture {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().expect("temporary config base");
        let base_path = fs::canonicalize(base.path()).expect("canonical temporary config base");
        let config = ConfigBaseGuard::set(base_path);
        let profile =
            PersistenceProfileGuard::named(format!("journal-store-{}", Uuid::new_v4().simple()));
        let owner = WritableProfileGuard::acquire_current().expect("profile owner");
        Fixture {
            _runtime: runtime,
            _base: base,
            _config: config,
            _profile: profile,
            owner,
        }
    }

    fn intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 1,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "store-test"
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

    fn database_path(owner: &WritableProfileGuard) -> PathBuf {
        owner
            .profile_root()
            .join(JOURNAL_DATABASE_ROOT_NAME)
            .join(JOURNAL_DATABASE_FILE_NAME)
    }

    #[test]
    fn open_retain_owner_and_child_lease_until_store_drop() {
        let mut fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        assert!(path.is_file());
        drop(fixture.owner);
        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        drop(store);
        fixture.owner =
            WritableProfileGuard::acquire_current().expect("reacquire after store drop");
    }

    #[test]
    fn intent_insert_get_duplicate_and_restart_preserve_sequence() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = intent_record(OperationId::new());
        let receipt = store.insert_intent(&first).expect("insert first intent");
        assert_eq!(receipt.operation_id(), first.operation_id);
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(
            store.get(first.operation_id).expect("get first").as_ref(),
            Some(&first)
        );
        assert!(matches!(
            store.insert_intent(&first),
            Err(OperationJournalStoreError::DuplicateOperation { operation_id })
                if operation_id == first.operation_id
        ));

        let second = intent_record(OperationId::new());
        let second_receipt = store.insert_intent(&second).expect("insert second intent");
        assert_eq!(second_receipt.sequence(), 2);
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        assert_eq!(
            reopened
                .get(first.operation_id)
                .expect("get persisted first"),
            Some(first)
        );
        assert_eq!(
            reopened
                .get(second.operation_id)
                .expect("get persisted second"),
            Some(second)
        );
    }

    #[test]
    fn insert_rejects_later_phase_disposition_and_missing_capacity() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");

        let mut later_phase = intent_record(OperationId::new());
        later_phase.phase = OperationPhase::Prepared;
        assert!(matches!(
            store.insert_intent(&later_phase),
            Err(OperationJournalStoreError::NotIntentDurable {
                phase: OperationPhase::Prepared
            })
        ));

        let mut later_disposition = intent_record(OperationId::new());
        later_disposition.disposition = OperationDisposition::RetryPending;
        assert!(matches!(
            store.insert_intent(&later_disposition),
            Err(OperationJournalStoreError::InconsistentRecord { reason })
                if reason.contains("None disposition")
        ));

        let mut no_capacity = intent_record(OperationId::new());
        no_capacity.capacity_plan = None;
        assert!(matches!(
            store.insert_intent(&no_capacity),
            Err(OperationJournalStoreError::MissingCapacityPlan)
        ));
    }

    #[test]
    fn unknown_existing_file_is_retained_without_mutation() {
        let fixture = fixture();
        let path = database_path(&fixture.owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let bytes = b"not a sqlite database".to_vec();
        fs::write(&path, &bytes).expect("write unknown file");
        let error = OperationJournalStore::open(&fixture.owner).expect_err("reject unknown file");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert_eq!(fs::read(&path).expect("read retained file"), bytes);
    }

    #[test]
    fn future_sqlite_header_is_retained_without_schema_mutation() {
        let fixture = fixture();
        let path = database_path(&fixture.owner);
        fs::create_dir_all(path.parent().expect("journal directory")).expect("journal directory");
        let mut bytes = vec![0_u8; SQLITE_HEADER_BYTES];
        bytes[..SQLITE_HEADER_MAGIC.len()].copy_from_slice(SQLITE_HEADER_MAGIC);
        bytes[18] = 1;
        bytes[19] = 1;
        bytes[SQLITE_USER_VERSION_OFFSET..SQLITE_USER_VERSION_OFFSET + 4]
            .copy_from_slice(&(JOURNAL_SCHEMA_VERSION + 1).to_be_bytes());
        bytes[SQLITE_APPLICATION_ID_OFFSET..SQLITE_APPLICATION_ID_OFFSET + 4]
            .copy_from_slice(&JOURNAL_APPLICATION_ID.to_be_bytes());
        fs::write(&path, &bytes).expect("write future SQLite header");
        let error = OperationJournalStore::open(&fixture.owner).expect_err("reject future header");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert_eq!(fs::read(&path).expect("read retained header"), bytes);
    }

    #[test]
    fn malformed_row_is_retained_and_reported_without_unbounded_read() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        drop(store);

        let operation_id = OperationId::new();
        let malformed = b"not-json";
        let connection = rusqlite::Connection::open(&path).expect("open journal fixture");
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, 1, ?2)",
                params![operation_id.to_string(), malformed],
            )
            .expect("insert malformed retained row");
        drop(connection);

        let store = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        let error = store
            .get(operation_id)
            .expect_err("report malformed retained row");
        assert!(matches!(
            error,
            OperationJournalStoreError::RetainedRecord {
                operation_id: Some(id), ..
            } if id == operation_id
        ));
        drop(store);

        let connection = rusqlite::Connection::open(&path).expect("reopen retained journal");
        let retained: Vec<u8> = connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read retained malformed row");
        assert_eq!(retained, malformed);
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_symlink_is_rejected_before_database_creation() {
        let fixture = fixture();
        let root = fixture
            .owner
            .profile_root()
            .join(JOURNAL_DATABASE_ROOT_NAME);
        fs::create_dir_all(&root).expect("journal directory");
        let target = root.join("sidecar-target");
        fs::write(&target, b"retained").expect("sidecar target");
        std::os::unix::fs::symlink(&target, root.join("journal.sqlite3-wal"))
            .expect("sidecar symlink");
        let error =
            OperationJournalStore::open(&fixture.owner).expect_err("reject sidecar symlink");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert!(!root.join(JOURNAL_DATABASE_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn database_replacement_is_rejected_before_lookup_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        let displaced = path.with_extension("sqlite3.displaced");
        fs::rename(&path, &displaced).expect("displace database");
        fs::write(&path, b"replacement").expect("write replacement database");
        let error = store
            .get(OperationId::new())
            .expect_err("reject replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(fs::read(&path).expect("read replacement"), b"replacement");
        drop(store);
    }

    #[cfg(unix)]
    #[test]
    fn database_replacement_is_rejected_before_insert_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        let displaced = path.with_extension("sqlite3.displaced");
        fs::rename(&path, &displaced).expect("displace database");
        fs::write(&path, b"replacement").expect("write replacement database");
        let record = intent_record(OperationId::new());
        let error = store
            .insert_intent(&record)
            .expect_err("reject insert into replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(fs::read(&path).expect("read replacement"), b"replacement");
        drop(store);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_between_creation_and_sqlite_open_is_not_bootstrapped() {
        let fixture = fixture();
        let replacement = b"replacement must remain untouched";
        let gate = |path: &Path| {
            fs::rename(path, path.with_extension("displaced")).expect("displace creator file");
            fs::write(path, replacement).expect("install replacement");
        };
        let result = OperationJournalStore::open_inner(&fixture.owner, Some(&gate));
        assert!(matches!(
            result,
            Err(OperationJournalStoreError::DatabaseReplaced { .. })
        ));
        assert_eq!(
            fs::read(database_path(&fixture.owner)).expect("retained bytes"),
            replacement
        );
    }

    #[cfg(windows)]
    #[test]
    fn sqlite_handle_prevents_database_replacement_while_owned() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        assert!(fs::rename(&path, path.with_extension("displaced")).is_err());
        let record = intent_record(OperationId::new());
        store
            .insert_intent(&record)
            .expect("original store remains usable");
        assert_eq!(
            store.get(record.operation_id).expect("read original"),
            Some(record)
        );
    }

    #[test]
    fn child_lease_is_retained_by_store() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let result = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new(JOURNAL_DATABASE_ROOT_NAME),
        );
        assert!(matches!(
            result,
            Err(ProfileOwnershipError::ProfileChildAlreadyLeased { .. })
        ));
        drop(store);
        let reopened = BoundDatabaseRoot::for_profile_child(
            &fixture.owner,
            Path::new(JOURNAL_DATABASE_ROOT_NAME),
        )
        .expect("child lease released after store drop");
        drop(reopened);
    }

    #[test]
    fn post_open_wal_sidecar_is_rejected_before_lookup() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let root = fixture
            .owner
            .profile_root()
            .join(JOURNAL_DATABASE_ROOT_NAME);
        fs::write(root.join("journal.sqlite3-wal"), b"unexpected").expect("write WAL sidecar");
        let error = store
            .get(OperationId::new())
            .expect_err("reject WAL sidecar");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        drop(store);
    }

    #[cfg(unix)]
    #[test]
    fn post_open_rollback_symlink_blocks_insert_without_touching_target() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let path = database_path(&fixture.owner);
        let sidecar = path.with_file_name("journal.sqlite3-journal");
        if sidecar.exists() {
            fs::rename(&sidecar, sidecar.with_extension("retained")).expect("retain own journal");
        }
        let target = path.with_file_name("unrelated-file");
        fs::write(&target, b"untouched").expect("target fixture");
        std::os::unix::fs::symlink(&target, &sidecar).expect("replace sidecar with symlink");
        assert!(matches!(
            store.insert_intent(&intent_record(OperationId::new())),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        assert_eq!(fs::read(&target).expect("target unchanged"), b"untouched");
        fs::remove_file(sidecar).expect("remove only fixture symlink before SQLite closes");
    }

    #[test]
    fn oversized_retained_blob_is_rejected_before_materializing_record() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let id = OperationId::new();
        // Simulate a corrupt/older producer that bypassed the schema's size constraint.
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("fixture bypass");
        store
            .connection
            .execute(
                "INSERT INTO operation_journal VALUES (?1, 1, zeroblob(?2))",
                params![id.to_string(), (MAX_RECORD_BYTES + 1) as i64],
            )
            .expect("oversized fixture");
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("restore constraint");
        assert!(
            matches!(store.get(id), Err(OperationJournalStoreError::RetainedRecord { reason, .. }) if reason.contains("length"))
        );
        let retained: i64 = store
            .connection
            .query_row(
                "SELECT length(record) FROM operation_journal WHERE operation_id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .expect("retained blob length");
        assert_eq!(retained, (MAX_RECORD_BYTES + 1) as i64);
    }

    #[cfg(unix)]
    #[test]
    fn profile_root_replacement_is_rejected_before_lookup_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let root = fixture.owner.profile_root().to_path_buf();
        let displaced = root.with_file_name("journal-store-displaced-root");
        fs::rename(&root, &displaced).expect("displace profile root");
        fs::create_dir(&root).expect("replacement profile root");
        fs::create_dir(root.join(JOURNAL_DATABASE_ROOT_NAME)).expect("replacement journal root");
        let error = store
            .get(OperationId::new())
            .expect_err("reject root replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::ProfileOwnership(
                ProfileOwnershipError::ProfileRootReplaced { .. }
            )
        ));
        drop(store);
        fs::remove_dir_all(&root).expect("remove replacement root");
        fs::rename(displaced, root).expect("restore profile root");
    }
}
