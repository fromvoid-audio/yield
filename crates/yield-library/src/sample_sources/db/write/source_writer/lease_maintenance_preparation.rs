//! Genuine pre-audit source MAIN custody. Empty preparation, never a journal/effect grant.
use super::{
    SourceLeaseMaintenanceAllocation, SourceWriter, SourceWriterError,
    metadata_policy_state::EnteredMetadataPolicy,
};
use crate::{WalAdmissionError, lifecycle_allocation::LifecycleAllocationError};
use std::{
    fmt,
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Bounded rejection; no retry, lease renewal or settlement permission follows.
#[derive(Debug, thiserror::Error)]
pub enum SourceLeaseMaintenancePreparationError {
    /// Original common deadline expired or exceeds two seconds.
    #[error("lease-maintenance preparation deadline expired")]
    Deadline,
    /// A transaction predating this preparation is never adopted or rolled back.
    #[error("source already has a transaction")]
    TransactionActive,
    /// Original owner, schema or exact native allocation cannot be confirmed.
    #[error(transparent)]
    Allocation(#[from] LifecycleAllocationError),
    /// Actual source owner or restrictive settings validation failed.
    #[error(transparent)]
    Owner(#[from] SourceWriterError),
    /// Actual retained WAL is unavailable or above the hard watermark.
    #[error(transparent)]
    Wal(#[from] WalAdmissionError),
    /// Same-connection acquisition or timeout restoration failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Original empty transaction cleanup remains uncertain.
    #[error("lease-maintenance rollback remains unconfirmed")]
    RollbackUnconfirmed,
    /// Settings restoration is sticky failed; this writer must remain private until close.
    #[error("lease-maintenance policy restoration remains unconfirmed")]
    PolicyUnconfirmed,
}

/// Exclusive original-writer empty MAIN reservation and selected pager/extension policy.
/// Non-Clone, non-deserializable and confined to the owning thread by its extension guard.
/// No authoritative audit/revision, claim, required permit or lease release is issued.
pub struct SourceLeaseMaintenancePreparation {
    custody: Custody,
    nonce: Uuid,
    wal_bytes: u64,
}
/// Rejected preparation retains its exact writer and settings until cleanup is confirmed.
pub struct SourceLeaseMaintenancePreparationFailure {
    custody: Custody,
    error: SourceLeaseMaintenancePreparationError,
}
impl fmt::Debug for SourceLeaseMaintenancePreparation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceLeaseMaintenancePreparation")
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SourceLeaseMaintenancePreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceLeaseMaintenancePreparationFailure")
            .field("error", &self.error)
            .finish()
    }
}

// Field order keeps the original ordinary connection/lease ahead of the extension gate.
struct Custody {
    writer: Option<SourceWriter>,
    policy: Option<EnteredMetadataPolicy>,
    observation: SourceLeaseMaintenanceAllocation,
    deadline: Instant,
    created_transaction: bool,
    preexisting_transaction: bool,
    restore_timeout: Option<Duration>,
    poisoned: bool,
    #[cfg(test)]
    timeout_restore_fault: bool,
}
impl Custody {
    fn writer(&self) -> &SourceWriter {
        self.writer
            .as_ref()
            .expect("original lease-maintenance writer")
    }
    fn restore_timeout(&mut self) -> Result<(), SourceLeaseMaintenancePreparationError> {
        if let Some(timeout) = self.restore_timeout {
            #[cfg(test)]
            if self.timeout_restore_fault {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    None,
                )
                .into());
            }
            #[cfg(feature = "test-support")]
            if self
                .writer()
                .wal_timeout_restore_fault
                .as_ref()
                .is_some_and(|fault| fault.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    None,
                )
                .into());
            }
            self.writer().database.connection.busy_timeout(timeout)?;
            let actual: u64 = self.writer().database.connection.pragma_query_value(
                None,
                "busy_timeout",
                |r| r.get(0),
            )?;
            if u128::from(actual) != timeout.as_millis() {
                return Err(SourceLeaseMaintenancePreparationError::PolicyUnconfirmed);
            }
            self.restore_timeout = None;
        }
        Ok(())
    }
    fn release(&mut self) -> Result<SourceWriter, SourceLeaseMaintenancePreparationError> {
        if self.poisoned {
            return Err(SourceLeaseMaintenancePreparationError::PolicyUnconfirmed);
        }
        let connection = &self.writer().database.connection;
        if self.preexisting_transaction {
            return Err(SourceLeaseMaintenancePreparationError::TransactionActive);
        }
        if self.created_transaction {
            if connection.execute_batch("ROLLBACK").is_err() || !connection.is_autocommit() {
                return Err(SourceLeaseMaintenancePreparationError::RollbackUnconfirmed);
            }
            self.created_transaction = false;
        }
        if !self.writer().database.connection.is_autocommit() {
            return Err(SourceLeaseMaintenancePreparationError::RollbackUnconfirmed);
        }
        self.restore_timeout()?;
        // This separate finite cleanup window proves safe reuse only. It does not change the
        // original preparation deadline, nonce, observation or ability to admit an effect.
        let cleanup = Instant::now() + Duration::from_secs(2);
        // Even failed pre-policy acquisition must not return a foreign/replaced owner.
        self.observation
            .validate_retained(self.writer(), cleanup, self.policy.is_some())
            .or_else(|error| {
                if self.policy.is_some() {
                    // A partially failed installation may still have the original pager cap.
                    self.observation
                        .validate_retained(self.writer(), cleanup, false)
                } else {
                    Err(error)
                }
            })?;
        if let Some(policy) = self.policy.as_mut() {
            let database = &self.writer.as_ref().expect("original writer").database;
            if policy.restore(database).is_err() {
                self.poisoned = true;
                return Err(SourceLeaseMaintenancePreparationError::PolicyUnconfirmed);
            }
        }
        if let Err(error) = self
            .observation
            .validate_retained(self.writer(), cleanup, false)
        {
            self.poisoned = true;
            return Err(error.into());
        }
        // Drop the policy guard only after all SQL/owner restoration checks finish.
        self.policy = None;
        Ok(self
            .writer
            .take()
            .expect("original lease-maintenance writer"))
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        if self.writer.is_some() {
            // Successful cleanup's returned writer is dropped here. On failure fields close
            // the original ordinary connection before its lease and before the extension gate.
            // No uncertain connection escapes and no control writer is queued behind MAIN.
            let _ = self.release();
        }
    }
}

impl SourceWriter {
    /// Consume this genuine writer and its original passive allocation into one empty,
    /// nonblocking MAIN preparation. No lease renewal, ordinary mutation or audit fabrication.
    /// Cancellation is explicit `abort`; expiry never proves rollback or returns an owner.
    pub fn prepare_lease_maintenance(
        self,
        observation: SourceLeaseMaintenanceAllocation,
        requested_deadline: Instant,
    ) -> Result<SourceLeaseMaintenancePreparation, SourceLeaseMaintenancePreparationFailure> {
        let deadline = requested_deadline.min(observation.deadline());
        let preexisting_transaction = !self.database.connection.is_autocommit();
        let mut custody = Custody {
            writer: Some(self),
            policy: None,
            observation,
            deadline,
            created_transaction: false,
            preexisting_transaction,
            restore_timeout: None,
            poisoned: false,
            #[cfg(test)]
            timeout_restore_fault: false,
        };
        let result = prepare(&mut custody);
        match result {
            Ok(wal_bytes) => Ok(SourceLeaseMaintenancePreparation {
                custody,
                nonce: Uuid::new_v4(),
                wal_bytes,
            }),
            Err(error) => Err(SourceLeaseMaintenancePreparationFailure { custody, error }),
        }
    }
}
fn prepare(custody: &mut Custody) -> Result<u64, SourceLeaseMaintenancePreparationError> {
    if custody.preexisting_transaction {
        return Err(SourceLeaseMaintenancePreparationError::TransactionActive);
    }
    if custody
        .deadline
        .checked_duration_since(Instant::now())
        .is_none_or(|v| v.is_zero() || v > Duration::from_secs(2))
    {
        return Err(SourceLeaseMaintenancePreparationError::Deadline);
    }
    let validated =
        custody
            .observation
            .validate_retained(custody.writer(), custody.deadline, false);
    // If an observer's read rollback failed, retain the transaction it created as well.
    custody.created_transaction = !custody.writer().database.connection.is_autocommit();
    validated?;
    if custody.created_transaction {
        return Err(SourceLeaseMaintenancePreparationError::RollbackUnconfirmed);
    }
    custody.policy = Some(EnteredMetadataPolicy::new(
        &custody.writer().database,
        custody.observation.allocation().selected_maximum_pages(),
    )?);
    custody
        .policy
        .as_ref()
        .expect("selected policy")
        .install(&custody.writer().database)?;
    custody
        .policy
        .as_ref()
        .expect("selected policy")
        .validate(&custody.writer().database)?;
    custody
        .observation
        .validate_retained(custody.writer(), custody.deadline, true)?;
    let timeout: u64 =
        custody
            .writer()
            .database
            .connection
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))?;
    custody.restore_timeout = Some(Duration::from_millis(timeout));
    custody
        .writer()
        .database
        .connection
        .busy_timeout(Duration::ZERO)?;
    let acquired = custody
        .writer()
        .database
        .connection
        .execute_batch("BEGIN IMMEDIATE");
    custody.created_transaction = !custody.writer().database.connection.is_autocommit();
    acquired?;
    custody
        .observation
        .validate_retained(custody.writer(), custody.deadline, true)?;
    custody
        .policy
        .as_ref()
        .expect("selected policy")
        .validate(&custody.writer().database)?;
    let wal = crate::sqlite_wal::enforce_wal_admission(&custody.writer().database.connection)?;
    if Instant::now() >= custody.deadline {
        return Err(SourceLeaseMaintenancePreparationError::Deadline);
    }
    Ok(wal.observed_bytes())
}
impl SourceLeaseMaintenancePreparation {
    /// Local opaque identity, not durable acceptance or reservation authority.
    pub fn nonce(&self) -> Uuid {
        self.nonce
    }
    /// Historical pre-audit-capable scope and allocation, not SourceRevision or permission.
    pub fn observation(&self) -> &SourceLeaseMaintenanceAllocation {
        &self.custody.observation
    }
    /// Original common deadline; no revalidation can extend it.
    pub fn deadline(&self) -> Instant {
        self.custody.deadline
    }
    /// Actual descriptor size observed under the empty reservation.
    pub fn wal_bytes(&self) -> u64 {
        self.wal_bytes
    }
    /// Check snapshot custody only; no source retirement/release acknowledgement is issued.
    pub fn source_authority_close_ready(&self) -> bool {
        self.custody
            .writer()
            .database
            .source_authority_close_ready()
    }
    /// Readonly original owner, exact scope/native objects/settings and WAL health checks.
    /// Failure retains exclusive custody; cancellation and cleanup remain explicit.
    pub fn revalidate(&self) -> Result<(), SourceLeaseMaintenancePreparationError> {
        if Instant::now() >= self.deadline() {
            return Err(SourceLeaseMaintenancePreparationError::Deadline);
        }
        self.custody
            .observation
            .validate_retained(self.custody.writer(), self.deadline(), true)?;
        self.custody
            .policy
            .as_ref()
            .expect("selected policy")
            .validate(&self.custody.writer().database)?;
        crate::sqlite_wal::enforce_wal_admission(&self.custody.writer().database.connection)?;
        if Instant::now() >= self.deadline() {
            return Err(SourceLeaseMaintenancePreparationError::Deadline);
        }
        Ok(())
    }
    /// Cancel before intent and recover only the confirmed original writer/settings.
    pub fn abort(mut self) -> Result<SourceWriter, SourceLeaseMaintenancePreparationFailure> {
        match self.custody.release() {
            Ok(writer) => Ok(writer),
            Err(error) => Err(SourceLeaseMaintenancePreparationFailure {
                custody: self.custody,
                error,
            }),
        }
    }
}
impl SourceLeaseMaintenancePreparationFailure {
    /// Inspect the rejection while the original owner remains inaccessible.
    pub fn error(&self) -> &SourceLeaseMaintenancePreparationError {
        &self.error
    }
    /// Diagnostic snapshot readiness; never close/release authority.
    pub fn source_authority_close_ready(&self) -> bool {
        self.custody
            .writer()
            .database
            .source_authority_close_ready()
    }
    /// Cleanup may retry a confirmed-empty rollback/timeout; poisoned policy never revives.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        match self.custody.release() {
            Ok(writer) => Ok(writer),
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}

#[cfg(test)]
mod tests;
