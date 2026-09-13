//! Profile-owned SQLite storage for durable operation intents.
//!
//! This store is deliberately smaller than either the source catalog or the global library
//! database. It owns only an operation id, a monotonic journal-observation sequence, and one bounded
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

use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;

use crate::app_dirs::{ProfileOwnershipError, WritableProfileGuard};
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;
use crate::sample_sources::db::BoundDatabaseRoot;
use crate::sample_sources::db::sqlite_identity::{self, DatabaseIdentityError};

use super::codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};
use super::coordinator::{CopyIntentArgs, RetryObservation};
use super::model::UniversalIntent;
use super::model::{
    CancellationRequestObservation, MAX_RECORD_BYTES, OperationActor, OperationDisposition,
    OperationId, OperationIntent, OperationKind, OperationPhase, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5,
};
use super::validation::{RecordConsistency, classify_record};

/// Read-only, bounded recovery paging for the profile-owned operation journal.
pub mod recovery;

#[cfg(test)]
mod process_tests;

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
const SQLITE_ROLLBACK_HEADER_BYTES: usize = 28;
const SQLITE_JOURNAL_SUFFIX: &str = "-journal";
const SQLITE_WAL_SUFFIX: &str = "-wal";
const SQLITE_SHM_SUFFIX: &str = "-shm";
const JOURNAL_TABLE_NAME: &str = "operation_journal";
const MAX_SCHEMA_OBJECTS: i64 = 8;
const MAX_SCHEMA_COLUMNS: usize = 8;
const MAX_SCHEMA_INDEXES: usize = 8;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
// Keep SQLite itself from materializing an arbitrarily large key/value before the recovery
// scanner can inspect its SQL type and length. This leaves room for the bounded JSON envelope
// while remaining close to the persisted record limit.
const SQLITE_VALUE_LIMIT: i32 = (MAX_RECORD_BYTES + 64 * 1024) as i32;

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
    /// A cancellation request must carry a positive recovery sequence expectation.
    #[error(
        "operation journal cancellation request requires a positive expected sequence, found {expected}"
    )]
    InvalidExpectedSequence {
        /// Sequence supplied by the caller.
        expected: i64,
    },
    /// The requested operation id is not present in the journal.
    #[error("operation journal operation is not present: {operation_id}")]
    OperationNotFound {
        /// Operation identity that was not found.
        operation_id: OperationId,
    },
    /// The journal row changed after the caller observed its sequence.
    #[error(
        "operation journal operation {operation_id} has sequence {actual}, expected {expected}"
    )]
    StaleSequence {
        /// Operation identity whose sequence changed.
        operation_id: OperationId,
        /// Sequence supplied by the caller.
        expected: i64,
        /// Current durable sequence.
        actual: i64,
    },
    /// A different cancellation request is already durable for this operation.
    #[error(
        "operation journal operation {operation_id} already has a different cancellation request"
    )]
    CancellationConflict {
        /// Operation identity with the conflicting request.
        operation_id: OperationId,
    },
    /// The requested cancellation observation is invalid at the store boundary.
    #[error("operation journal cancellation request is invalid: {reason}")]
    InvalidCancellationRequest {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// This schema cannot be transitioned by the pre-publication cancellation API.
    #[error(
        "operation journal cancellation requires a V3 universal intent, found schema v{schema_version}"
    )]
    UnsupportedCancellationSchema {
        /// Schema version of the retained row.
        schema_version: u32,
    },
    /// Cancellation is only a user operation in this first transition boundary.
    #[error("operation journal cancellation is unsupported for actor {actor:?}")]
    UnsupportedCancellationActor {
        /// Actor carried by the durable intent.
        actor: OperationActor,
    },
    /// The retained row is not an eligible V3 initial intent.
    #[error("operation journal cancellation state is not eligible: {reason}")]
    UnsupportedCancellationState {
        /// Bounded state diagnostic.
        reason: String,
    },
    /// The requested retry observation is invalid at the store boundary.
    #[error("operation journal retry observation is invalid: {reason}")]
    InvalidRetryObservation {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A different retry observation is already durable for this operation.
    #[error("operation journal operation {operation_id} already has a different retry observation")]
    RetryObservationConflict {
        /// Operation identity with the conflicting observation.
        operation_id: OperationId,
    },
    /// This schema cannot be transitioned by the retry API.
    #[error(
        "operation journal retry observation requires a V5 coordinator record, found schema v{schema_version}"
    )]
    UnsupportedRetrySchema {
        /// Schema version of the retained row.
        schema_version: u32,
    },
    /// The retained row is not eligible for a V5 retry observation.
    #[error("operation journal retry state is not eligible: {reason}")]
    UnsupportedRetryState {
        /// Bounded state diagnostic.
        reason: String,
    },
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

/// Acknowledgement returned after a cancellation request has committed and passed readback.
///
/// The receipt proves only that the non-terminal request is durable in this profile-owned
/// journal. It grants no cleanup, capacity-release, filesystem, or recovery execution authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalCancelRequestReceipt {
    operation_id: OperationId,
    sequence: i64,
    encoded_bytes: usize,
}

impl JournalCancelRequestReceipt {
    /// Return the operation id carrying the cancellation request.
    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the new or replayed journal sequence.
    pub fn sequence(self) -> i64 {
        self.sequence
    }

    /// Return the encoded V4 record length.
    pub fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

/// Acknowledgement returned after a typed retry observation has committed and passed readback.
///
/// The receipt proves only that scheduling metadata is durable in this profile-owned journal. It
/// grants no live retry lease, capacity reservation, filesystem capability, or execution
/// authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalRetryReceipt {
    operation_id: OperationId,
    sequence: i64,
    encoded_bytes: usize,
}

impl JournalRetryReceipt {
    /// Return the operation carrying the retry observation.
    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Return the new or replayed journal sequence.
    pub fn sequence(self) -> i64 {
        self.sequence
    }

    /// Return the encoded V5 record length.
    pub fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

struct JournalRow {
    sequence: i64,
    encoded: Vec<u8>,
    record: StoredOperationRecord,
}

enum CancelTransition {
    Replay(JournalCancelRequestReceipt),
    Updated {
        receipt: JournalCancelRequestReceipt,
        record: StoredOperationRecord,
    },
}

enum RetryTransition {
    Replay(JournalRetryReceipt),
    Updated {
        receipt: JournalRetryReceipt,
        record: StoredOperationRecord,
    },
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

    /// Read one bounded, read-only recovery page after an optional journal-observation cursor.
    ///
    /// The page validates the retained namespace before and after its query. Returned records
    /// are untrusted observations and do not authorize filesystem or journal mutations.
    pub fn scan_recovery_page(
        &self,
        cursor: Option<recovery::RecoveryCursor>,
    ) -> Result<recovery::RecoveryPage, recovery::RecoveryScanError> {
        recovery::scan_page(self, cursor)
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
        let rollback_journal = inspect_preexisting_sidecars(&database_root, &database_path)?;

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
        validate_pre_sqlite_sidecars(&database_root, &database_path, rollback_journal.as_ref())?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&database_path, flags)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, SQLITE_VALUE_LIMIT)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        validate_connection_identity(
            &connection,
            &database_root,
            &database_path,
            &database_identity,
        )?;
        validate_database_file_identity(&database_root, &database_path, &database_identity)?;
        validate_pre_sqlite_sidecars(&database_root, &database_path, rollback_journal.as_ref())?;
        drop(file);

        if created {
            configure_connection(&connection)?;
            bootstrap_schema(&connection)?;
        } else {
            validate_existing_schema(&connection, &database_path)?;
            configure_connection(&connection)?;
        }
        if let Some(rollback_journal) = rollback_journal.as_ref() {
            validate_rollback_journal_identity(
                &database_root,
                &database_path,
                &rollback_journal.identity,
            )?;
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
        drop(rollback_journal);

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
        let Some(row) = self.read_row(operation_id)? else {
            self.validate_namespace()?;
            return Ok(None);
        };
        self.validate_namespace()?;
        Ok(Some(row.record))
    }

    /// Persist one validated V5 regular-file copy intent.
    ///
    /// This method records the typed command and its participant observations after an owning
    /// coordinator has completed any queue and capacity gates. It proves only that the V5 intent
    /// reached this journal; it does not admit filesystem execution or acquire a live capability.
    pub fn persist_copy_intent(
        &self,
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: CopyIntentArgs,
        capacity_plan: crate::durable_capacity::DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        if intent.actor != OperationActor::User || intent.kind != OperationKind::FileMutation {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: String::from(
                    "schema-v5 copy intent requires a user file-mutation operation",
                ),
            });
        }
        if universal_intent.command_kind() != super::model::UniversalCommandKind::Copy {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: String::from("schema-v5 copy intent requires the Copy command"),
            });
        }
        if timestamp_unix_ms < 0 {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: String::from("schema-v5 timestamps must be non-negative"),
            });
        }
        command_args
            .validate_against_participants(universal_intent.participants())
            .map_err(|error| OperationJournalStoreError::InconsistentRecord {
                reason: error.to_string(),
            })?;
        let record = StoredOperationRecord::new_v5_copy(
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
            timestamp_unix_ms,
        );
        match classify_record(&record) {
            RecordConsistency::Consistent => {}
            RecordConsistency::LegacyMissingPublicationEvidence => {
                return Err(OperationJournalStoreError::InconsistentRecord {
                    reason: String::from("schema-v5 copy intent has legacy publication evidence"),
                });
            }
            RecordConsistency::Invalid(reason) => {
                return Err(OperationJournalStoreError::InconsistentRecord { reason });
            }
        }
        let encoded = encode(&record)?;
        self.persist_record_with_encoded(&record, &encoded)
    }

    /// Persist a user cancellation request against a current V3 or V5 universal intent.
    ///
    /// The expected sequence is a compare-and-swap guard. A retry carrying the exact observation
    /// already stored for this operation is a read-only idempotent replay and returns the current
    /// receipt without allocating another sequence. All other stale, legacy, malformed, or
    /// unsupported rows remain unchanged. A V3 row becomes V4 and a V5 row remains V5 so its
    /// typed command and retry observations are preserved. The resulting record is intentionally
    /// non-terminal; it grants no cleanup, capacity-release, filesystem, or recovery execution
    /// authority.
    pub fn request_cancel_before_publish(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: CancellationRequestObservation,
    ) -> Result<JournalCancelRequestReceipt, OperationJournalStoreError> {
        if expected_sequence <= 0 {
            return Err(OperationJournalStoreError::InvalidExpectedSequence {
                expected: expected_sequence,
            });
        }
        observation.validate().map_err(|error| {
            OperationJournalStoreError::InvalidCancellationRequest {
                reason: error.to_string(),
            }
        })?;

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result =
            self.request_cancel_inside_transaction(operation_id, expected_sequence, observation);
        match result {
            Ok(CancelTransition::Replay(receipt)) => {
                let rollback = self.connection.execute_batch("ROLLBACK");
                if let Err(error) = rollback {
                    return Err(error.into());
                }
                self.validate_namespace()?;
                Ok(receipt)
            }
            Ok(CancelTransition::Updated { receipt, record }) => {
                if let Err(error) = self.validate_namespace_without_sidecars() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let persisted = self.get(operation_id)?;
                if persisted.as_ref() != Some(&record) {
                    return Err(retained_error(
                        Some(operation_id),
                        String::from("committed cancellation request could not be read back"),
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

    fn request_cancel_inside_transaction(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: CancellationRequestObservation,
    ) -> Result<CancelTransition, OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        let Some(current) = self.read_row(operation_id)? else {
            return Err(OperationJournalStoreError::OperationNotFound { operation_id });
        };

        if matches!(current.record.schema_version, SCHEMA_V4 | SCHEMA_V5)
            && current.record.disposition == OperationDisposition::CancelRequestedBeforePublish
        {
            if current.record.cancellation_request() == Some(&observation) {
                return Ok(CancelTransition::Replay(JournalCancelRequestReceipt {
                    operation_id,
                    sequence: current.sequence,
                    encoded_bytes: current.encoded.len(),
                }));
            }
            return Err(OperationJournalStoreError::CancellationConflict { operation_id });
        }

        if current.sequence != expected_sequence {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        if !matches!(current.record.schema_version, SCHEMA_V3 | SCHEMA_V5) {
            return Err(OperationJournalStoreError::UnsupportedCancellationSchema {
                schema_version: current.record.schema_version,
            });
        }
        if current.record.intent.actor != OperationActor::User {
            return Err(OperationJournalStoreError::UnsupportedCancellationActor {
                actor: current.record.intent.actor,
            });
        }
        let eligible_disposition = match current.record.schema_version {
            SCHEMA_V3 => current.record.disposition == OperationDisposition::None,
            SCHEMA_V5 => matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            ),
            _ => false,
        };
        if current.record.phase != OperationPhase::IntentDurable || !eligible_disposition {
            let expected = match current.record.schema_version {
                SCHEMA_V5 => "V5 IntentDurable/None or RetryPending",
                _ => "V3 IntentDurable/None",
            };
            return Err(OperationJournalStoreError::UnsupportedCancellationState {
                reason: format!(
                    "expected {expected}, found {:?}/{:?}",
                    current.record.phase, current.record.disposition
                ),
            });
        }
        if observation.requested_unix_ms() < current.record.updated_unix_ms {
            return Err(OperationJournalStoreError::InvalidCancellationRequest {
                reason: format!(
                    "request timestamp {} precedes the stored update timestamp {}",
                    observation.requested_unix_ms(),
                    current.record.updated_unix_ms
                ),
            });
        }

        let next_record = current.record.with_cancellation_request(observation);
        let encoded = encode(&next_record)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: format!("encoded record exceeds {MAX_RECORD_BYTES} bytes"),
            });
        }
        self.validate_namespace_without_sidecars()?;
        let sequence = self.next_sequence_inside_transaction()?;
        let changed = self.connection.execute(
            "UPDATE operation_journal
             SET sequence = ?1, record = ?2
             WHERE operation_id = ?3 AND sequence = ?4",
            params![
                sequence,
                encoded.as_slice(),
                operation_id.to_string(),
                expected_sequence
            ],
        )?;
        if changed != 1 {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        self.validate_namespace_without_sidecars()?;
        Ok(CancelTransition::Updated {
            receipt: JournalCancelRequestReceipt {
                operation_id,
                sequence,
                encoded_bytes: encoded.len(),
            },
            record: next_record,
        })
    }

    /// Persist the next typed V5 retry observation using a sequence compare-and-swap.
    ///
    /// An identical observation already stored for the operation is a read-only idempotent
    /// replay, even when the caller still holds an older sequence. A changed observation must
    /// carry the current sequence and exactly the next attempt number. This method stores no live
    /// lease authority and does not mutate capacity, filesystem, or participant state.
    pub fn record_retry_pending(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<JournalRetryReceipt, OperationJournalStoreError> {
        if expected_sequence <= 0 {
            return Err(OperationJournalStoreError::InvalidExpectedSequence {
                expected: expected_sequence,
            });
        }
        observation.validate().map_err(|error| {
            OperationJournalStoreError::InvalidRetryObservation {
                reason: error.to_string(),
            }
        })?;

        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result =
            self.record_retry_inside_transaction(operation_id, expected_sequence, observation);
        match result {
            Ok(RetryTransition::Replay(receipt)) => {
                let rollback = self.connection.execute_batch("ROLLBACK");
                if let Err(error) = rollback {
                    return Err(error.into());
                }
                self.validate_namespace()?;
                Ok(receipt)
            }
            Ok(RetryTransition::Updated { receipt, record }) => {
                if let Err(error) = self.validate_namespace_without_sidecars() {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error);
                }
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let persisted = self.get(operation_id)?;
                if persisted.as_ref() != Some(&record) {
                    return Err(retained_error(
                        Some(operation_id),
                        String::from("committed retry observation could not be read back"),
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

    fn record_retry_inside_transaction(
        &self,
        operation_id: OperationId,
        expected_sequence: i64,
        observation: RetryObservation,
    ) -> Result<RetryTransition, OperationJournalStoreError> {
        self.validate_namespace_without_sidecars()?;
        let Some(current) = self.read_row(operation_id)? else {
            return Err(OperationJournalStoreError::OperationNotFound { operation_id });
        };

        if current.record.disposition == OperationDisposition::CancelRequestedBeforePublish {
            return Err(OperationJournalStoreError::RetryObservationConflict { operation_id });
        }
        if current.record.schema_version == SCHEMA_V5
            && current.record.disposition == OperationDisposition::RetryPending
            && current.record.retry_observation() == Some(&observation)
        {
            return Ok(RetryTransition::Replay(JournalRetryReceipt {
                operation_id,
                sequence: current.sequence,
                encoded_bytes: current.encoded.len(),
            }));
        }

        if current.sequence != expected_sequence {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        if current.record.schema_version != SCHEMA_V5 {
            return Err(OperationJournalStoreError::UnsupportedRetrySchema {
                schema_version: current.record.schema_version,
            });
        }
        if current.record.phase != OperationPhase::IntentDurable
            || !matches!(
                current.record.disposition,
                OperationDisposition::None | OperationDisposition::RetryPending
            )
        {
            return Err(OperationJournalStoreError::UnsupportedRetryState {
                reason: format!(
                    "expected V5 IntentDurable/None or RetryPending, found {:?}/{:?}",
                    current.record.phase, current.record.disposition
                ),
            });
        }
        let expected_attempt = current
            .record
            .retry_observation()
            .map_or(1, |previous| previous.attempt().checked_add(1).unwrap_or(0));
        if expected_attempt == 0 || observation.attempt() != expected_attempt {
            return Err(OperationJournalStoreError::InvalidRetryObservation {
                reason: format!(
                    "retry attempt must advance from {} to exactly {}",
                    current
                        .record
                        .retry_observation()
                        .map_or(0, RetryObservation::attempt),
                    expected_attempt
                ),
            });
        }
        if observation.requested_unix_ms() < current.record.updated_unix_ms {
            return Err(OperationJournalStoreError::InvalidRetryObservation {
                reason: format!(
                    "retry request timestamp {} precedes the stored update timestamp {}",
                    observation.requested_unix_ms(),
                    current.record.updated_unix_ms
                ),
            });
        }

        let next_record = current.record.with_retry_pending(observation);
        let encoded = encode(&next_record)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(OperationJournalStoreError::InconsistentRecord {
                reason: format!("encoded record exceeds {MAX_RECORD_BYTES} bytes"),
            });
        }
        self.validate_namespace_without_sidecars()?;
        let sequence = self.next_sequence_inside_transaction()?;
        let changed = self.connection.execute(
            "UPDATE operation_journal
             SET sequence = ?1, record = ?2
             WHERE operation_id = ?3 AND sequence = ?4",
            params![
                sequence,
                encoded.as_slice(),
                operation_id.to_string(),
                expected_sequence
            ],
        )?;
        if changed != 1 {
            return Err(OperationJournalStoreError::StaleSequence {
                operation_id,
                expected: expected_sequence,
                actual: current.sequence,
            });
        }
        self.validate_namespace_without_sidecars()?;
        Ok(RetryTransition::Updated {
            receipt: JournalRetryReceipt {
                operation_id,
                sequence,
                encoded_bytes: encoded.len(),
            },
            record: next_record,
        })
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

        self.persist_record_with_encoded(record, &encoded)
    }

    fn persist_record_with_encoded(
        &self,
        record: &StoredOperationRecord,
        encoded: &[u8],
    ) -> Result<JournalInsertReceipt, OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let result = self.insert_inside_transaction(record, encoded);
        match result {
            Ok(receipt) => {
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                self.validate_namespace()?;
                let persisted = self.get(record.operation_id)?;
                if persisted.as_ref() != Some(record) {
                    return Err(retained_error(
                        Some(record.operation_id),
                        String::from("committed journal row could not be read back identically"),
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
        let sequence = self.next_sequence_inside_transaction()?;
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

    fn next_sequence_inside_transaction(&self) -> Result<i64, OperationJournalStoreError> {
        let previous_sequence = self.connection.query_row(
            "SELECT MAX(sequence) FROM operation_journal",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        if previous_sequence.is_some_and(|sequence| sequence <= 0) {
            return Err(audit_error(
                &self.database_path,
                "stored journal-observation sequence is not positive",
            ));
        }
        previous_sequence
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(OperationJournalStoreError::SequenceExhausted)
    }

    /// Read one row with the same bounded key, sequence, blob, codec, and consistency checks used
    /// by point reads. This helper deliberately performs no namespace or sidecar inspection so it
    /// can run inside the store's exclusive transaction.
    fn read_row(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<JournalRow>, OperationJournalStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT typeof(operation_id), typeof(sequence), sequence,
                        typeof(record), length(record), record
                 FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| {
                    let operation_id_type: String = row.get(0)?;
                    let sequence_type: String = row.get(1)?;
                    let sequence = match row.get_ref(2)? {
                        ValueRef::Integer(value) => Some(value),
                        _ => None,
                    };
                    let record_type: String = row.get(3)?;
                    let record_length = match row.get_ref(4)? {
                        ValueRef::Integer(value) => Some(value),
                        ValueRef::Null => None,
                        _ => None,
                    };
                    let encoded = match row.get_ref(5)? {
                        ValueRef::Blob(value) => Some(value.to_vec()),
                        _ => None,
                    };
                    Ok((
                        operation_id_type,
                        sequence_type,
                        sequence,
                        record_type,
                        record_length,
                        encoded,
                    ))
                },
            )
            .optional()?;
        let Some((operation_id_type, sequence_type, sequence, record_type, record_length, encoded)) =
            row
        else {
            return Ok(None);
        };
        if operation_id_type != "text" {
            return Err(retained_error(
                Some(operation_id),
                String::from("operation id column must contain a SQLite TEXT key"),
            ));
        }
        if sequence_type != "integer" {
            return Err(retained_error(
                Some(operation_id),
                String::from("sequence column must contain a SQLite integer"),
            ));
        }
        let Some(sequence) = sequence else {
            return Err(retained_error(
                Some(operation_id),
                String::from("sequence column is not an SQLite integer"),
            ));
        };
        if sequence <= 0 {
            return Err(retained_error(
                Some(operation_id),
                String::from("stored sequence is not positive"),
            ));
        }
        if record_type != "blob" {
            return Err(retained_error(
                Some(operation_id),
                String::from("record column must contain a SQLite BLOB"),
            ));
        }
        let Some(record_length) = record_length else {
            return Err(retained_error(
                Some(operation_id),
                String::from("stored record length is NULL"),
            ));
        };
        if !(0..=MAX_RECORD_BYTES as i64).contains(&record_length) {
            return Err(retained_error(
                Some(operation_id),
                format!("stored record length {record_length} exceeds the bounded record size"),
            ));
        }
        let Some(encoded) = encoded else {
            return Err(retained_error(
                Some(operation_id),
                String::from("record column could not be read as a SQLite BLOB"),
            ));
        };
        if encoded.len() != record_length as usize || encoded.len() > MAX_RECORD_BYTES {
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
            RecordConsistency::Consistent => Ok(Some(JournalRow {
                sequence,
                encoded,
                record,
            })),
            RecordConsistency::LegacyMissingPublicationEvidence => Err(retained_error(
                Some(operation_id),
                String::from("legacy record is retained without publication evidence"),
            )),
            RecordConsistency::Invalid(reason) => Err(retained_error(Some(operation_id), reason)),
        }
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
    sqlite_identity::open_database_handle(root, name, path).map_err(map_identity_error)
}

fn ensure_regular_file(file: &File, path: &Path) -> Result<(), OperationJournalStoreError> {
    sqlite_identity::ensure_regular_file(file, path).map_err(map_identity_error)
}

struct PreexistingRollbackJournal {
    _file: File,
    identity: String,
}

fn validate_pre_sqlite_sidecars(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    expected: Option<&PreexistingRollbackJournal>,
) -> Result<(), OperationJournalStoreError> {
    let observed = inspect_preexisting_sidecars(root, database_path)?;
    match (expected, observed) {
        (None, None) => Ok(()),
        (Some(expected), Some(observed)) if expected.identity == observed.identity => Ok(()),
        (Some(_), None) => Err(audit_error(
            database_path,
            "validated SQLite rollback sidecar disappeared before database open",
        )),
        (None, Some(_)) => Err(audit_error(
            database_path,
            "SQLite rollback sidecar appeared before database open",
        )),
        (Some(_), Some(_)) => Err(audit_error(
            database_path,
            "validated SQLite rollback sidecar was replaced before database open",
        )),
    }
}

fn inspect_preexisting_sidecars(
    root: &BoundDatabaseRoot,
    database_path: &Path,
) -> Result<Option<PreexistingRollbackJournal>, OperationJournalStoreError> {
    let mut rollback_journal = None;
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
                if suffix != SQLITE_JOURNAL_SUFFIX {
                    return Err(audit_error(
                        database_path,
                        format!("unexpected SQLite sidecar {name} is present"),
                    ));
                }
                let mut file = root
                    .open_file_nofollow(Path::new(&name), false)
                    .map_err(|source| io_error(database_path, source))?;
                validate_inactive_rollback_journal(&mut file, database_path, &name)?;
                let identity =
                    stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
                        OperationJournalStoreError::RequiresAudit {
                            path: database_path.to_path_buf(),
                            reason: format!(
                                "stable identity is unavailable for SQLite sidecar {name}"
                            ),
                        }
                    })?;
                rollback_journal = Some(PreexistingRollbackJournal {
                    _file: file,
                    identity,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(database_path, source)),
        }
    }
    Ok(rollback_journal)
}

fn validate_inactive_rollback_journal(
    file: &mut File,
    database_path: &Path,
    sidecar_name: &str,
) -> Result<(), OperationJournalStoreError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error(database_path, source))?;
    if !metadata.is_file() {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} is not a regular file"),
        ));
    }
    let length = metadata.len();
    if length == 0 {
        return Ok(());
    }
    if length < SQLITE_ROLLBACK_HEADER_BYTES as u64 {
        return Err(audit_error(
            database_path,
            format!("SQLite rollback sidecar {sidecar_name} is an ambiguous {length}-byte file"),
        ));
    }
    let mut header = [0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut header))
        .map_err(|source| io_error(database_path, source))?;
    if header.iter().any(|byte| *byte != 0) {
        return Err(audit_error(
            database_path,
            format!("SQLite rollback sidecar {sidecar_name} has an active or ambiguous header"),
        ));
    }
    Ok(())
}

fn validate_rollback_journal_identity(
    root: &BoundDatabaseRoot,
    database_path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    let sidecar_name = format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_JOURNAL_SUFFIX}");
    let metadata = match root.symlink_metadata(Path::new(&sidecar_name)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(database_path, source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} is a symlink"),
        ));
    }
    if !metadata.is_file() {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} is not a regular file"),
        ));
    }
    let file = root
        .open_file_nofollow(Path::new(&sidecar_name), false)
        .map_err(|source| io_error(database_path, source))?;
    let mut file = file;
    validate_inactive_rollback_journal(&mut file, database_path, &sidecar_name)?;
    let actual_identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
        OperationJournalStoreError::RequiresAudit {
            path: database_path.to_path_buf(),
            reason: format!("stable identity is unavailable for SQLite sidecar {sidecar_name}"),
        }
    })?;
    if actual_identity != expected_identity {
        return Err(audit_error(
            database_path,
            format!("SQLite sidecar {sidecar_name} was replaced while opening the database"),
        ));
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
    sqlite_identity::validate_database_file_identity_with_unavailable_reason(
        root,
        Path::new(JOURNAL_DATABASE_FILE_NAME),
        database_path,
        expected_identity,
        "stable identity is unavailable for the open journal file",
    )
    .map_err(map_identity_error)
}

fn validate_connection_identity(
    connection: &Connection,
    _root: &BoundDatabaseRoot,
    path: &Path,
    expected_identity: &str,
) -> Result<(), OperationJournalStoreError> {
    sqlite_identity::validate_connection_identity(connection, path, expected_identity)
        .map_err(map_identity_error)
}

fn map_identity_error(error: DatabaseIdentityError) -> OperationJournalStoreError {
    match error {
        DatabaseIdentityError::Io { path, source } => io_error(&path, source),
        DatabaseIdentityError::NotRegular { path } => {
            audit_error(&path, "journal database is not a regular file")
        }
        DatabaseIdentityError::Replaced { path } => {
            OperationJournalStoreError::DatabaseReplaced { path }
        }
        DatabaseIdentityError::Unavailable { reason, .. } => {
            OperationJournalStoreError::IdentityCheckUnavailable { reason }
        }
        DatabaseIdentityError::ConnectionMismatch { path, reason } => {
            OperationJournalStoreError::ConnectionIdentityMismatch { path, reason }
        }
        DatabaseIdentityError::UnsafeSidecar { path, reason } => audit_error(&path, reason),
    }
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
    #[cfg(unix)]
    use std::io::Write;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::durable_capacity::{
        CapacityAllocationClass, DurableCapacityPlan, DurableVolumeCapacity,
        PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity,
    };
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        ParticipantCheckpoint, ParticipantDescriptor, ParticipantKind, ParticipantRequirement,
        ParticipantSet, RecoveryCursor, RecoveryRowOutcome, RetryErrorClass, RetryLeaseId,
        RetryObservation, StoredRelativePath, StoredSourceFence, UniversalCommandKind,
        UniversalIntent,
    };
    use crate::sample_sources::SourceId;
    use crate::test_runtime::TestRuntimeGuard;

    struct Fixture {
        owner: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _config: ConfigBaseGuard,
        _base: TempDir,
        // Release test serialization only after handles, overrides, and files are gone.
        _runtime: TestRuntimeGuard,
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

    fn universal_intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let value = serde_json::json!({
            "schema_version": 3,
            "operation_id": operation_id.as_uuid(),
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "store-v3-test"
            },
            "universal_intent": {
                "parent_operation_id": null,
                "command_kind": "CreateFile",
                "user_status_key": "operation.status.store-v3",
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
            "payload": {"store": "v3"},
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
            "created_unix_ms": 41,
            "updated_unix_ms": 42
        });
        decode(&serde_json::to_vec(&value).expect("serialize universal intent fixture"))
            .expect("decode universal intent fixture")
    }

    fn v5_copy_inputs() -> (
        OperationId,
        OperationIntent,
        UniversalIntent,
        CopyIntentArgs,
        DurableCapacityPlan,
    ) {
        let operation_id = OperationId::new();
        let source_id = SourceId::from_string("source-a");
        let source_participant_id = crate::operation_journal::ParticipantId::new();
        let filesystem_participant_id = crate::operation_journal::ParticipantId::new();
        let source_fence =
            StoredSourceFence::new(source_id.clone(), "root-a", "database-a", 1, 7, 1)
                .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant_id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_fence),
                    None,
                )
                .expect("source participant"),
                ParticipantDescriptor::new(
                    filesystem_participant_id,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem participant"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_participant_id,
                    crate::operation_journal::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant_id,
                    crate::operation_journal::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        let universal_intent = UniversalIntent::new(
            None,
            UniversalCommandKind::Copy,
            "operation.status.copy",
            participants,
        )
        .expect("copy universal intent");
        let source = CopyEndpoint::new(
            source_participant_id,
            source_id.clone(),
            StoredRelativePath::new("in.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            source_participant_id,
            source_id,
            StoredRelativePath::new("out.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let expected_source =
            CopySourceExpectation::new(7, "file-identity", 10, 11).expect("source expectation");
        let command_args = CopyIntentArgs::new(
            source,
            destination,
            expected_source,
            CopyCollisionPolicy::NoReplace,
            Some(String::from("metadata/entry")),
        )
        .expect("copy arguments");
        let capacity_plan = DurableCapacityPlan {
            volumes: vec![DurableVolumeCapacity {
                identity: VolumeIdentity { device: 1 },
                allocation_unit: 1,
                allocation_class: CapacityAllocationClass::DestinationStaging,
                logical_bytes: 1,
                allocated_bytes: 1,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            }],
        };
        let intent = OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::FileMutation,
            label: String::from("copy store test"),
        };
        (
            operation_id,
            intent,
            universal_intent,
            command_args,
            capacity_plan,
        )
    }

    fn external_universal_intent_record(operation_id: OperationId) -> StoredOperationRecord {
        let mut value: serde_json::Value = serde_json::from_slice(
            &encode(&universal_intent_record(operation_id)).expect("v3 bytes"),
        )
        .expect("parse v3 fixture");
        value["intent"]["actor"] = serde_json::Value::from("ExternalFs");
        value["intent"]["kind"] = serde_json::Value::from("ExternalReconciliation");
        value["universal_intent"]["command_kind"] =
            serde_json::Value::from("ExternalReconciliation");
        decode(&serde_json::to_vec(&value).expect("serialize external fixture"))
            .expect("decode external intent fixture")
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
    fn v3_intent_insert_get_and_restart_preserve_universal_participants() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let receipt = store.insert_intent(&record).expect("insert v3 intent");
        assert_eq!(receipt.operation_id(), record.operation_id);
        assert_eq!(
            record
                .universal_intent
                .as_ref()
                .expect("universal intent")
                .participants()
                .descriptors()
                .len(),
            1
        );
        assert_eq!(
            store.get(record.operation_id).expect("get v3 intent"),
            Some(record.clone())
        );
        drop(store);

        let reopened = OperationJournalStore::open(&fixture.owner).expect("reopen store");
        assert_eq!(
            reopened
                .get(record.operation_id)
                .expect("get restarted v3 intent"),
            Some(record)
        );
    }

    #[test]
    fn v5_copy_retry_and_cancel_preserve_typed_envelope_and_cas_semantics() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v5_copy_inputs();
        let inserted = store
            .persist_copy_intent(
                operation_id,
                intent.clone(),
                universal_intent.clone(),
                command_args.clone(),
                capacity_plan.clone(),
                50,
            )
            .expect("persist v5 copy intent");
        assert_eq!(inserted.sequence(), 1);

        let first_retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("retry-lease-1").expect("retry lease"),
            53,
            RetryErrorClass::TransientContention,
            "writer_busy",
            Some(String::from("source database is busy")),
        )
        .expect("first retry observation");
        let retry_receipt = store
            .record_retry_pending(operation_id, inserted.sequence(), first_retry.clone())
            .expect("persist first retry");
        assert_eq!(retry_receipt.sequence(), 2);

        let persisted = store
            .get(operation_id)
            .expect("get retry row")
            .expect("retry row");
        assert_eq!(persisted.schema_version, SCHEMA_V5);
        assert_eq!(persisted.intent, intent);
        assert_eq!(persisted.universal_intent, Some(universal_intent));
        assert_eq!(persisted.command_args(), Some(&command_args));
        assert_eq!(persisted.capacity_plan, Some(capacity_plan));
        assert_eq!(persisted.retry_observation(), Some(&first_retry));
        assert_eq!(persisted.disposition, OperationDisposition::RetryPending);

        let page_after_old_cursor = store
            .scan_recovery_page(Some(
                RecoveryCursor::new(inserted.sequence()).expect("positive recovery cursor"),
            ))
            .expect("reobserve retry row after old cursor");
        assert_eq!(page_after_old_cursor.entries().len(), 1);
        assert_eq!(page_after_old_cursor.entries()[0].sequence(), 2);
        assert!(matches!(
            page_after_old_cursor.entries()[0].outcome(),
            RecoveryRowOutcome::Consistent(observed)
                if observed.retry_observation() == Some(&first_retry)
        ));

        // Replaying from the original observation is read-only and does not allocate a new
        // global sequence.
        let replay = store
            .record_retry_pending(operation_id, inserted.sequence(), first_retry.clone())
            .expect("replay first retry");
        assert_eq!(replay, retry_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 2);

        let stale_different = RetryObservation::new(
            2,
            53,
            54,
            RetryLeaseId::new("retry-lease-2").expect("retry lease"),
            55,
            RetryErrorClass::TransientAvailability,
            "source_unavailable",
            None,
        )
        .expect("second retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), stale_different.clone()),
            Err(OperationJournalStoreError::StaleSequence { operation_id: actual, expected: 1, actual: 2 })
                if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 2);

        // Another operation advances the global sequence; the next update still uses MAX+1.
        let (second_id, second_intent, second_universal, second_args, second_plan) =
            v5_copy_inputs();
        let second_inserted = store
            .persist_copy_intent(
                second_id,
                second_intent,
                second_universal,
                second_args,
                second_plan,
                60,
            )
            .expect("persist second copy intent");
        assert_eq!(second_inserted.sequence(), 3);

        let second_retry_receipt = store
            .record_retry_pending(
                operation_id,
                retry_receipt.sequence(),
                stale_different.clone(),
            )
            .expect("persist second retry");
        assert_eq!(second_retry_receipt.sequence(), 4);

        let cancellation = CancellationRequestObservation::new(56).expect("cancel timestamp");
        let cancel_receipt = store
            .request_cancel_before_publish(
                operation_id,
                second_retry_receipt.sequence(),
                cancellation,
            )
            .expect("persist cancellation over retry");
        assert_eq!(cancel_receipt.sequence(), 5);
        let canceled = store
            .get(operation_id)
            .expect("get canceled v5 row")
            .expect("canceled v5 row");
        assert_eq!(canceled.schema_version, SCHEMA_V5);
        assert_eq!(
            canceled.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(canceled.command_args(), Some(&command_args));
        assert_eq!(canceled.retry_observation(), Some(&stale_different));
        assert_eq!(canceled.cancellation_request(), Some(&cancellation));

        let cancel_replay = store
            .request_cancel_before_publish(operation_id, inserted.sequence(), cancellation)
            .expect("replay cancellation from stale sequence");
        assert_eq!(cancel_replay, cancel_receipt);
        assert_eq!(journal_sequence(&store, operation_id), 5);

        let retry_after_cancel = RetryObservation::new(
            3,
            57,
            58,
            RetryLeaseId::new("retry-lease-3").expect("retry lease"),
            59,
            RetryErrorClass::TransientContention,
            "writer_busy",
            None,
        )
        .expect("retry after cancellation");
        assert!(matches!(
            store.record_retry_pending(operation_id, cancel_receipt.sequence(), retry_after_cancel),
            Err(OperationJournalStoreError::RetryObservationConflict { operation_id: actual })
                if actual == operation_id
        ));
        assert_eq!(journal_sequence(&store, operation_id), 5);
    }

    fn journal_sequence(store: &OperationJournalStore, operation_id: OperationId) -> i64 {
        store
            .connection
            .query_row(
                "SELECT sequence FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read journal sequence")
    }

    #[test]
    fn cancellation_request_persists_v4_without_execution_authority() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");

        let receipt = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("persist cancellation request");
        assert_eq!(receipt.operation_id(), record.operation_id);
        assert_eq!(receipt.sequence(), 2);
        assert_eq!(
            receipt.encoded_bytes(),
            encode(
                &store
                    .get(record.operation_id)
                    .expect("get canceled row")
                    .unwrap()
            )
            .expect("encode persisted cancellation")
            .len()
        );

        let persisted = store
            .get(record.operation_id)
            .expect("get persisted cancellation")
            .expect("cancellation row");
        assert_eq!(persisted.schema_version, SCHEMA_V4);
        assert_eq!(persisted.phase, OperationPhase::IntentDurable);
        assert_eq!(
            persisted.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(persisted.cancellation_request(), Some(&observation));
        assert_eq!(persisted.intent, record.intent);
        assert_eq!(persisted.universal_intent, record.universal_intent);
        assert_eq!(persisted.capacity_plan, record.capacity_plan);
        assert_eq!(persisted.payload, record.payload);
        assert_eq!(persisted.created_unix_ms, record.created_unix_ms);
        assert_eq!(persisted.updated_unix_ms, observation.requested_unix_ms());

        assert!(matches!(
            store.insert_intent(&persisted),
            Err(OperationJournalStoreError::InconsistentRecord { reason })
                if reason.contains("None disposition")
        ));

        let page = store
            .scan_recovery_page(None)
            .expect("scan cancellation recovery row");
        assert_eq!(page.entries().len(), 1);
        assert_eq!(page.entries()[0].sequence(), receipt.sequence());
        assert!(matches!(
            page.entries()[0].outcome(),
            RecoveryRowOutcome::Consistent(observed) if observed == &persisted
        ));
    }

    #[test]
    fn cancellation_request_replay_is_idempotent_and_different_request_is_rejected() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        let receipt = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("persist cancellation request");

        let replay = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect("replay exact cancellation request");
        assert_eq!(replay, receipt);

        let different = CancellationRequestObservation::new(44).expect("different timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(record.operation_id, receipt.sequence(), different),
            Err(OperationJournalStoreError::CancellationConflict { operation_id })
                if operation_id == record.operation_id
        ));
        let persisted = store
            .get(record.operation_id)
            .expect("get after replay conflict")
            .expect("cancellation row");
        assert_eq!(persisted.cancellation_request(), Some(&observation));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT sequence FROM operation_journal WHERE operation_id = ?1",
                    params![record.operation_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read cancellation sequence"),
            receipt.sequence()
        );
    }

    #[test]
    fn cancellation_request_rejects_stale_sequence_and_legacy_rows_unchanged() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = universal_intent_record(OperationId::new());
        let first_receipt = store.insert_intent(&first).expect("insert first v3 intent");
        let second = universal_intent_record(OperationId::new());
        store
            .insert_intent(&second)
            .expect("insert second v3 intent");
        let before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![first.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read first bytes");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(first.operation_id, first_receipt.sequence() + 1, observation),
            Err(OperationJournalStoreError::StaleSequence { operation_id, expected, actual })
                if operation_id == first.operation_id
                    && expected == first_receipt.sequence() + 1
                    && actual == first_receipt.sequence()
        ));
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![first.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged first bytes");
        assert_eq!(after, before);

        let legacy = intent_record(OperationId::new());
        let legacy_receipt = store.insert_intent(&legacy).expect("insert legacy intent");
        let legacy_before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![legacy.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read legacy bytes");
        assert!(matches!(
            store.request_cancel_before_publish(
                legacy.operation_id,
                legacy_receipt.sequence(),
                observation
            ),
            Err(OperationJournalStoreError::UnsupportedCancellationSchema { schema_version: 1 })
        ));
        let legacy_after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![legacy.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged legacy bytes");
        assert_eq!(legacy_after, legacy_before);

        let external = external_universal_intent_record(OperationId::new());
        let external_receipt = store
            .insert_intent(&external)
            .expect("insert external intent");
        assert!(matches!(
            store.request_cancel_before_publish(
                external.operation_id,
                external_receipt.sequence(),
                observation
            ),
            Err(OperationJournalStoreError::UnsupportedCancellationActor {
                actor: OperationActor::ExternalFs
            })
        ));
    }

    #[test]
    fn cancellation_request_allocates_global_sequence_after_other_operation() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let first = universal_intent_record(OperationId::new());
        let first_receipt = store.insert_intent(&first).expect("insert first v3 intent");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        let cancellation = store
            .request_cancel_before_publish(
                first.operation_id,
                first_receipt.sequence(),
                observation,
            )
            .expect("persist cancellation request");
        let second = universal_intent_record(OperationId::new());
        let second_receipt = store
            .insert_intent(&second)
            .expect("insert second v3 intent");
        assert_eq!(cancellation.sequence(), 2);
        assert_eq!(second_receipt.sequence(), 3);
    }

    #[test]
    fn cancellation_request_rejects_nonpositive_expected_sequence_without_io() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(OperationId::new(), 0, observation),
            Err(OperationJournalStoreError::InvalidExpectedSequence { expected: 0 })
        ));
    }

    #[test]
    fn cancellation_request_rejects_timestamp_before_current_update() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let observation = CancellationRequestObservation::new(record.updated_unix_ms - 1)
            .expect("earlier non-negative timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(record.operation_id, inserted.sequence(), observation),
            Err(OperationJournalStoreError::InvalidCancellationRequest { reason })
                if reason.contains("precedes")
        ));
        assert_eq!(
            store
                .get(record.operation_id)
                .expect("get unchanged intent"),
            Some(record)
        );
    }

    #[test]
    fn cancellation_request_rejects_sequence_exhaustion_without_mutating_row() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let max_id = OperationId::new();
        let max_record = universal_intent_record(max_id);
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    max_id.to_string(),
                    i64::MAX,
                    encode(&max_record).expect("max row bytes")
                ],
            )
            .expect("insert sequence exhaustion fixture");
        let before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read row before exhaustion");
        let observation = CancellationRequestObservation::new(43).expect("request timestamp");
        assert!(matches!(
            store.request_cancel_before_publish(
                record.operation_id,
                inserted.sequence(),
                observation
            ),
            Err(OperationJournalStoreError::SequenceExhausted)
        ));
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![record.operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read row after exhaustion");
        assert_eq!(after, before);
    }

    #[test]
    fn retry_observation_rejects_sequence_exhaustion_without_mutating_row() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let (operation_id, intent, universal_intent, command_args, capacity_plan) =
            v5_copy_inputs();
        let inserted = store
            .persist_copy_intent(
                operation_id,
                intent,
                universal_intent,
                command_args,
                capacity_plan,
                1,
            )
            .expect("persist v5 copy intent");
        let max_id = OperationId::new();
        let (_, max_intent, max_universal_intent, max_command_args, max_plan) = v5_copy_inputs();
        let max_record = StoredOperationRecord::new_v5_copy(
            max_id,
            max_intent,
            max_universal_intent,
            max_command_args,
            max_plan,
            1,
        );
        store
            .connection
            .execute(
                "INSERT INTO operation_journal (operation_id, sequence, record)
                 VALUES (?1, ?2, ?3)",
                params![
                    max_id.to_string(),
                    i64::MAX,
                    encode(&max_record).expect("max v5 row bytes")
                ],
            )
            .expect("insert sequence exhaustion fixture");
        let before: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read row before exhaustion");
        let observation = RetryObservation::new(
            1,
            2,
            3,
            RetryLeaseId::new("retry-lease").expect("retry lease"),
            4,
            RetryErrorClass::TransientContention,
            "busy",
            None,
        )
        .expect("retry observation");
        assert!(matches!(
            store.record_retry_pending(operation_id, inserted.sequence(), observation),
            Err(OperationJournalStoreError::SequenceExhausted)
        ));
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal WHERE operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged row after exhaustion");
        assert_eq!(after, before);
        assert_eq!(journal_sequence(&store, operation_id), inserted.sequence());
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
    fn corrupt_sequence_is_not_used_to_admit_another_intent() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let retained = intent_record(OperationId::new());
        store
            .connection
            .execute(
                "INSERT INTO operation_journal VALUES (?1, -1, ?2)",
                params![
                    retained.operation_id.to_string(),
                    encode(&retained).expect("fixture bytes")
                ],
            )
            .expect("corrupt sequence fixture");
        let fresh = intent_record(OperationId::new());
        assert!(matches!(
            store.insert_intent(&fresh),
            Err(OperationJournalStoreError::RequiresAudit { .. })
        ));
        assert!(
            store
                .get(fresh.operation_id)
                .expect("lookup rejected intent")
                .is_none()
        );
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
    fn database_replacement_is_rejected_before_cancellation_effects() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = universal_intent_record(OperationId::new());
        let inserted = store.insert_intent(&record).expect("insert v3 intent");
        let path = database_path(&fixture.owner);
        let displaced = path.with_extension("sqlite3.cancel-displaced");
        fs::rename(&path, &displaced).expect("displace database");
        let replacement = b"replacement must remain untouched";
        fs::write(&path, replacement).expect("write replacement database");
        let observation = CancellationRequestObservation::new(53).expect("request timestamp");
        let error = store
            .request_cancel_before_publish(record.operation_id, inserted.sequence(), observation)
            .expect_err("reject cancellation against replacement");
        assert!(matches!(
            error,
            OperationJournalStoreError::DatabaseReplaced { .. }
        ));
        assert_eq!(fs::read(&path).expect("read replacement"), replacement);
        drop(store);
        fs::remove_file(&path).expect("remove replacement");
        fs::rename(displaced, path).expect("restore original database");
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
    fn inactive_rollback_sidecar_variants_allow_restart() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        let record = intent_record(OperationId::new());
        store.insert_intent(&record).expect("persist intent");
        drop(store);

        let sidecar = database_path(&fixture.owner).with_file_name("journal.sqlite3-journal");
        for contents in [Vec::new(), vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES]] {
            fs::write(&sidecar, &contents).expect("write inactive rollback sidecar");
            let reopened = OperationJournalStore::open(&fixture.owner)
                .expect("inactive rollback sidecar is safe to reopen");
            assert_eq!(
                reopened
                    .get(record.operation_id)
                    .expect("read persisted intent"),
                Some(record.clone())
            );
            drop(reopened);
            if sidecar.exists() {
                fs::remove_file(&sidecar).expect("remove inactive rollback sidecar");
            }
        }
    }

    #[test]
    fn ambiguous_rollback_sidecars_are_retained_unchanged() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let sidecar = database_path(&fixture.owner).with_file_name("journal.sqlite3-journal");
        let cases = [vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES - 1], {
            let mut bytes = vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
            bytes[0] = 1;
            bytes
        }];
        for contents in cases {
            fs::write(&sidecar, &contents).expect("write ambiguous rollback sidecar");
            let error = OperationJournalStore::open(&fixture.owner)
                .expect_err("ambiguous rollback sidecar requires audit");
            assert!(matches!(
                error,
                OperationJournalStoreError::RequiresAudit { .. }
            ));
            assert_eq!(fs::read(&sidecar).expect("read retained sidecar"), contents);
            fs::remove_file(&sidecar).expect("remove retained sidecar fixture");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rollback_sidecar_replacement_between_validation_and_open_is_rejected() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let sidecar = database_path(&fixture.owner).with_file_name("journal.sqlite3-journal");
        fs::write(&sidecar, vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES])
            .expect("write inactive rollback sidecar");
        let replacement = vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
        let gate = |path: &Path| {
            let sidecar = path.with_file_name("journal.sqlite3-journal");
            fs::rename(&sidecar, sidecar.with_extension("displaced"))
                .expect("displace validated sidecar");
            fs::write(&sidecar, &replacement).expect("install replacement sidecar");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("replaced rollback sidecar requires audit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("sidecar") && reason.contains("replaced")
        ));
        assert_eq!(
            fs::read(&sidecar).expect("read retained replacement"),
            replacement
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollback_sidecar_header_change_on_same_file_is_rejected_before_sqlite_open() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let database = database_path(&fixture.owner);
        let before_database = fs::read(&database).expect("read original database");
        let sidecar = database.with_file_name("journal.sqlite3-journal");
        fs::write(&sidecar, vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES])
            .expect("write inactive rollback sidecar");
        let gate = |path: &Path| {
            let sidecar = path.with_file_name("journal.sqlite3-journal");
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&sidecar)
                .expect("open sidecar without replacement");
            file.write_all(&[1]).expect("make sidecar header active");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("same-file active rollback sidecar requires audit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { reason, .. }
                if reason.contains("active") || reason.contains("ambiguous")
        ));
        assert_eq!(
            fs::read(&database).expect("read retained database"),
            before_database
        );
        let mut expected_sidecar = vec![0_u8; SQLITE_ROLLBACK_HEADER_BYTES];
        expected_sidecar[0] = 1;
        assert_eq!(
            fs::read(&sidecar).expect("read retained sidecar"),
            expected_sidecar
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_appearing_rollback_sidecar_is_rejected_before_sqlite_open() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("open store");
        drop(store);
        let database = database_path(&fixture.owner);
        let before_database = fs::read(&database).expect("read original database");
        let sidecar = database.with_file_name("journal.sqlite3-journal");
        let replacement = vec![1_u8; SQLITE_ROLLBACK_HEADER_BYTES];
        let gate = |path: &Path| {
            fs::write(path.with_file_name("journal.sqlite3-journal"), &replacement)
                .expect("install newly appearing sidecar");
        };
        let error = OperationJournalStore::open_inner(&fixture.owner, Some(&gate))
            .expect_err("newly appearing rollback sidecar requires audit");
        assert!(matches!(
            error,
            OperationJournalStoreError::RequiresAudit { .. }
        ));
        assert_eq!(
            fs::read(&database).expect("read retained database"),
            before_database
        );
        assert_eq!(
            fs::read(&sidecar).expect("read retained sidecar"),
            replacement
        );
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
