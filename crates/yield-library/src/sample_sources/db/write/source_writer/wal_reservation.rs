//! Exclusive retained-source preparation; no operation, capacity or effect permission.
use super::{SourceRevision, SourceWriter, SourceWriterError};
use crate::WalAdmissionError;
use crate::sample_sources::db::SourceDbError;
use crate::sqlite_wal::enforce_wal_admission;
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

const MAX_PREPARATION: Duration = Duration::from_secs(2);

/// Failure before any preparation metadata, revision or receipt is written.
#[derive(Debug, Error)]
pub enum SourceWalPreparationError {
    /// Queue/SQL/validation consumed the cooperative preparation deadline.
    #[error("source WAL preparation deadline expired or exceeds two seconds")]
    Deadline,
    /// Preparation cannot renew a lease as a hidden pre-intent side effect.
    #[error("source lease needs separately admitted renewal before WAL preparation")]
    LeaseRenewalRequired,
    /// Retained ownership or exact revision failed validation.
    #[error(transparent)]
    Owner(#[from] SourceWriterError),
    /// Hard or unavailable retained WAL measurement rejects new work.
    #[error(transparent)]
    Wal(#[from] WalAdmissionError),
    /// Writer acquisition or timeout restoration failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// An existing caller-owned transaction cannot become preparation.
    #[error("source connection already has a transaction")]
    TransactionActive,
    /// The empty preparation could not be rolled back; the writer stays private.
    #[error("source WAL preparation rollback remains unconfirmed")]
    RollbackUnconfirmed,
}

/// Rejected preparation retaining its writer until rollback and policy restoration succeed.
///
/// It exposes no SQL connection or write method. Drop closes the retained connection before
/// source lease release; callers may recover the same writer through `into_writer` only.
pub struct SourceWalPreparationFailure {
    held: HeldWriter,
    error: SourceWalPreparationError,
}

impl fmt::Debug for SourceWalPreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceWalPreparationFailure")
            .field("error", &self.error)
            .finish()
    }
}

impl SourceWalPreparationFailure {
    /// Inspect the bounded typed rejection; it grants no retry authority.
    pub fn error(&self) -> &SourceWalPreparationError {
        &self.error
    }

    /// Return the exact retained writer only after confirming empty rollback and timeout policy.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        match self.held.release() {
            Ok(writer) => Ok(writer),
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}

/// An empty SQLite writer reservation on the existing source connection.
///
/// Non-Clone and exclusively owning its SourceWriter, it prevents other source methods from
/// executing SQL until explicit abort. The nonce/revision/size are observations, not a journal
/// decision latch, an OperationId, a commit permit or a capacity reservation. Drop aborts and
/// closes the writer. A coordinator must call `abort` to reuse it; no deadline proves rollback.
pub struct SourceWalPreparation {
    held: HeldWriter,
    nonce: Uuid,
    revision: SourceRevision,
    wal_bytes: u64,
    deadline: Instant,
}

impl fmt::Debug for SourceWalPreparation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceWalPreparation")
            .field("wal_bytes", &self.wal_bytes)
            .finish()
    }
}

impl SourceWalPreparation {
    /// Opaque local preparation identity, never durable acceptance.
    pub fn nonce(&self) -> Uuid {
        self.nonce
    }
    /// Exact owner-validated revision observed under the SQLite reservation.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Size from this connection's retained WAL VFS descriptor.
    pub fn wal_bytes(&self) -> u64 {
        self.wal_bytes
    }
    /// Cooperatively check expiry; this does not release a SQLite reservation.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
    /// Revalidate the retained profile, roots, main-file binding and current lease
    /// through the existing read-only control connection. This neither queries
    /// the reserved ordinary connection nor renews the lease or extends expiry.
    /// Deadline/owner failure leaves custody held until explicit abort or drop;
    /// success grants no catalog registration, journal or capacity authority.
    pub fn revalidate_owner(&self) -> Result<(), SourceWalPreparationError> {
        if self.is_expired() {
            return Err(SourceWalPreparationError::Deadline);
        }
        let lease = self
            .held
            .writer()
            .database
            .source_lease
            .as_deref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)
            .map_err(SourceWriterError::from)?;
        let validated = lease
            .validate_current_owner()
            .map_err(SourceDbError::from)
            .map_err(SourceWriterError::from);
        if self.is_expired() {
            return Err(SourceWalPreparationError::Deadline);
        }
        validated?;
        Ok(())
    }
    /// Cancel/release without changing source rows, returning the same writer only on confirmation.
    pub fn abort(mut self) -> Result<SourceWriter, SourceWalPreparationFailure> {
        match self.held.release() {
            Ok(writer) => Ok(writer),
            Err(error) => Err(SourceWalPreparationFailure {
                held: self.held,
                error,
            }),
        }
    }
}

// Separate ownership container gives both successful and rejected preparations the same cleanup.
struct HeldWriter {
    writer: Option<SourceWriter>,
    reservation: bool,
    restore_timeout: Option<Duration>,
}

impl HeldWriter {
    fn writer(&self) -> &SourceWriter {
        self.writer.as_ref().expect("retained source writer")
    }
    fn release(&mut self) -> Result<SourceWriter, SourceWalPreparationError> {
        let connection = &self.writer().database.connection;
        if self.reservation {
            if connection.execute_batch("ROLLBACK").is_err() || !connection.is_autocommit() {
                return Err(SourceWalPreparationError::RollbackUnconfirmed);
            }
            self.reservation = false;
        }
        if let Some(timeout) = self.restore_timeout {
            self.writer().database.connection.busy_timeout(timeout)?;
            self.restore_timeout = None;
        }
        Ok(self.writer.take().expect("retained source writer"))
    }
}

impl Drop for HeldWriter {
    fn drop(&mut self) {
        if self.reservation {
            if let Some(writer) = &self.writer {
                let _ = writer.database.connection.execute_batch("ROLLBACK");
            }
        }
        // SourceDatabase drops its ordinary connection before its retained source lease.
        // An unconfirmed empty rollback cannot expose this connection to another command.
    }
}

impl SourceWriter {
    /// Retain the existing writer in a bounded, empty WAL preparation reservation.
    ///
    /// Uses strict read-only lease validation; it never auto-renews or reopens a database. SQLite
    /// acquisition makes one nonblocking attempt, then restores the previous timeout. The caller
    /// must drive cancellation/expiry through abort; global/journal handshakes remain separate.
    pub fn prepare_wal_reservation(
        self,
        expected: &SourceRevision,
        deadline: Instant,
    ) -> Result<SourceWalPreparation, SourceWalPreparationFailure> {
        let mut held = HeldWriter {
            writer: Some(self),
            reservation: false,
            restore_timeout: None,
        };
        let prepared = prepare(&mut held, expected, deadline);
        finish_preparation(held, prepared, deadline)
    }
}

#[cfg(test)]
mod test_vfs;

#[cfg(test)]
mod owner_revalidation_tests;

fn finish_preparation(
    held: HeldWriter,
    prepared: Result<(SourceRevision, u64), SourceWalPreparationError>,
    deadline: Instant,
) -> Result<SourceWalPreparation, SourceWalPreparationFailure> {
    match prepared {
        Ok((revision, wal_bytes)) => Ok(SourceWalPreparation {
            held,
            nonce: Uuid::new_v4(),
            revision,
            wal_bytes,
            deadline,
        }),
        Err(error) => {
            // Rollback/restore now when possible. If not confirmed, Failure keeps the
            // writer inaccessible; into_writer must retry release before it can be reused.
            let mut failure = SourceWalPreparationFailure { held, error };
            if failure.held.reservation {
                let connection = &failure.held.writer().database.connection;
                if connection.execute_batch("ROLLBACK").is_ok() && connection.is_autocommit() {
                    failure.held.reservation = false;
                } else {
                    failure.error = SourceWalPreparationError::RollbackUnconfirmed;
                }
            }
            if let Some(timeout) = failure.held.restore_timeout {
                if failure
                    .held
                    .writer()
                    .database
                    .connection
                    .busy_timeout(timeout)
                    .is_ok()
                {
                    failure.held.restore_timeout = None;
                }
            }
            Err(failure)
        }
    }
}

fn validate(
    writer: &SourceWriter,
    expected: &SourceRevision,
    deadline: Instant,
) -> Result<SourceRevision, SourceWalPreparationError> {
    let now = Instant::now();
    let remaining = deadline
        .checked_duration_since(now)
        .filter(|d| !d.is_zero() && *d <= MAX_PREPARATION)
        .ok_or(SourceWalPreparationError::Deadline)?;
    let lease = writer
        .database
        .source_lease
        .as_deref()
        .ok_or(SourceDbError::ProfileOwnershipRequired)
        .map_err(SourceWriterError::from)?;
    let expiry = lease
        .validate_current_owner()
        .map_err(SourceDbError::from)
        .map_err(SourceWriterError::from)?;
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SourceWalPreparationError::LeaseRenewalRequired)?;
    // Round the wall-time deadline up, never down to an already-expiring lease second.
    let required = unix
        .checked_add(remaining)
        .and_then(|d| d.as_secs().checked_add(u64::from(d.subsec_nanos() != 0)))
        .and_then(|s| i64::try_from(s).ok())
        .ok_or(SourceWalPreparationError::LeaseRenewalRequired)?;
    if expiry <= required {
        return Err(SourceWalPreparationError::LeaseRenewalRequired);
    }
    let current = if writer.database.connection.is_autocommit() {
        writer.current_revision()? // strict; never current_revision_for_command
    } else {
        let scan = writer.current_scan_fence()?;
        let audit = writer
            .database
            .last_authoritative_audit_metadata_in_transaction()
            .map_err(SourceWriterError::from)?
            .ok_or(SourceWriterError::InvalidRequest(
                "source writer requires a complete authoritative audit",
            ))?;
        if audit.source_revision != scan.revision {
            return Err(SourceWriterError::InvalidRequest(
                "source revision is not aligned with the authoritative audit",
            )
            .into());
        }
        if audit.root_identity != scan.root_identity {
            return Err(SourceWriterError::StaleFence.into());
        }
        SourceRevision {
            source_id: scan.source_id,
            root_identity: scan.root_identity,
            root_binding_identity: scan.root_binding_identity,
            database_identity: scan.database_identity,
            lifecycle_generation: scan.lifecycle_generation,
            lease_epoch: scan.lease_epoch,
            revision: scan.revision,
        }
    };
    if current.source_id != expected.source_id
        || current.root_identity != expected.root_identity
        || current.root_binding_identity != expected.root_binding_identity
        || current.database_identity != expected.database_identity
        || current.lifecycle_generation != expected.lifecycle_generation
        || current.lease_epoch != expected.lease_epoch
    {
        return Err(SourceWriterError::StaleFence.into());
    }
    if current.revision != expected.revision {
        return Err(SourceWriterError::StaleRevision {
            expected: expected.revision,
            actual: current.revision,
        }
        .into());
    }
    if Instant::now() >= deadline {
        return Err(SourceWalPreparationError::Deadline);
    }
    Ok(current)
}

fn prepare(
    held: &mut HeldWriter,
    expected: &SourceRevision,
    deadline: Instant,
) -> Result<(SourceRevision, u64), SourceWalPreparationError> {
    // Never adopt or roll back a transaction that predates this preparation.
    if !held.writer().database.connection.is_autocommit() {
        return Err(SourceWalPreparationError::TransactionActive);
    }
    let validated = validate(held.writer(), expected, deadline);
    // The standalone bounded audit getter opens its own read transaction. If its
    // rollback fails, retain that newly created transaction privately too; returning
    // the writer would otherwise bypass confirmed cleanup before BEGIN IMMEDIATE.
    held.reservation = !held.writer().database.connection.is_autocommit();
    validated?;
    if held.reservation {
        return Err(SourceWalPreparationError::RollbackUnconfirmed);
    }
    let connection = &held.writer().database.connection;
    let timeout: u64 = connection.pragma_query_value(None, "busy_timeout", |r| r.get(0))?;
    held.restore_timeout = Some(Duration::from_millis(timeout));
    held.writer()
        .database
        .connection
        .busy_timeout(Duration::ZERO)?;
    let acquired = held
        .writer()
        .database
        .connection
        .execute_batch("BEGIN IMMEDIATE");
    held.reservation = !held.writer().database.connection.is_autocommit();
    // Restore even when BEGIN fails; never leave the ordinary owner's timeout disabled.
    held.writer()
        .database
        .connection
        .busy_timeout(Duration::from_millis(timeout))?;
    held.restore_timeout = None;
    acquired?;
    let revision = validate(held.writer(), expected, deadline)?;
    let state = enforce_wal_admission(&held.writer().database.connection)?;
    let wal_bytes = state.observed_bytes();
    if Instant::now() >= deadline {
        return Err(SourceWalPreparationError::Deadline);
    }
    Ok((revision, wal_bytes))
}
