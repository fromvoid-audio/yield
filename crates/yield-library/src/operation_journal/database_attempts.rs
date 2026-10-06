//! Strict retained observations. These values cannot construct or restore a live write permit.
use super::codec::StoredOperationRecord;
use super::database_claims::{
    self, ClaimDirection, DatabaseClaimInventory, DatabaseRole, RevisionDomain,
};
use super::full_capacity::FullCapacityAllocationClass;
use super::model::{MAX_RECORD_BYTES, OperationDisposition, OperationPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) mod inventory_bytes;

#[cfg(test)]
pub(super) mod record_backing;

pub(crate) const MAX_ATTEMPTS_PER_OPERATION: usize = 256;
pub(crate) const MAX_ATTEMPTS: usize = 8192;
pub(crate) const AUDIT_PAGE_ROWS: usize = 64;
// A full <=1MiB normalized family plus a <=64KiB binary inventory represented in JSON,
// tagged command/context and fixed identities fit below this closed 2MiB image ceiling.
pub(crate) const MAX_START_BYTES: usize = 2 * MAX_RECORD_BYTES;
pub(crate) const MAX_OUTCOME_BYTES: usize = 16 * 1024;
type Result<T> = std::result::Result<T, database_claims::DatabaseClaimError>;
fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(database_claims::DatabaseClaimError(message))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatabaseAttemptStart {
    pub version: u8,
    pub attempt: Uuid,
    pub nonce: Uuid,
    pub operation: Uuid,
    pub binding: Uuid,
    pub participant: Uuid,
    pub database_slice: Uuid,
    pub sidecar_slice: Uuid,
    /// Full exact canonical binding image includes every owner/profile/lease/physical identity.
    #[serde(deserialize_with = "inventory_bytes::deserialize")]
    pub inventory: Vec<u8>,
    pub schema: u32,
    pub phase: OperationPhase,
    pub disposition: OperationDisposition,
    /// Closed tagged request derived from actual codec-normalized family bytes and direction.
    /// Actual owner SQL/inverse derivation and live legality remain OPT-2168's responsibility.
    pub command: serde_json::Value,
    pub command_digest: [u8; 32],
    pub revision_domain: RevisionDomain,
    pub before: u64,
    pub after: u64,
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
}

impl DatabaseAttemptStart {
    #[allow(clippy::too_many_arguments, dead_code)] // Private genuine issuer integration: OPT-2168.
    pub(crate) fn from_context(
        record: &StoredOperationRecord,
        inventory: &DatabaseClaimInventory,
        attempt: Uuid,
        nonce: Uuid,
        database_slice: Uuid,
        sidecar_slice: Uuid,
        database_bytes: u64,
        wal_bytes: u64,
        shm_bytes: u64,
    ) -> Result<Self> {
        inventory.validate_record(record)?;
        let db = inventory
            .slices()
            .iter()
            .find(|s| s.id == database_slice)
            .ok_or(database_claims::DatabaseClaimError(
                "missing database slice",
            ))?;
        let sidecar = inventory
            .slices()
            .iter()
            .find(|s| s.id == sidecar_slice)
            .ok_or(database_claims::DatabaseClaimError("missing sidecar slice"))?;
        let binding = inventory
            .bindings()
            .iter()
            .find(|b| b.id == db.binding)
            .ok_or(database_claims::DatabaseClaimError(
                "missing database binding",
            ))?;
        let participant = binding
            .participants
            .iter()
            .find(|p| p.id == db.participant)
            .ok_or(database_claims::DatabaseClaimError(
                "missing database participant",
            ))?;
        let (main_class, sidecar_class) = match binding.role {
            DatabaseRole::Source => (
                FullCapacityAllocationClass::SourceDatabaseCommit,
                FullCapacityAllocationClass::SourceWalShm,
            ),
            DatabaseRole::Global => (
                FullCapacityAllocationClass::GlobalDatabaseCommit,
                FullCapacityAllocationClass::GlobalWalShm,
            ),
        };
        require(
            db.class == main_class
                && sidecar.class == sidecar_class
                && sidecar.binding == db.binding
                && sidecar.participant == db.participant
                && sidecar.direction == db.direction,
            "attempt slice role/direction/participant",
        )?;
        require(
            record.phase != OperationPhase::Terminal,
            "terminal attempt phase",
        )?;
        use OperationDisposition::*;
        if db.direction == ClaimDirection::RequiredForward {
            require(
                match binding.role {
                    DatabaseRole::Source => matches!(
                        record.phase,
                        OperationPhase::IntentDurable
                            | OperationPhase::Prepared
                            | OperationPhase::FilesystemStaged
                            | OperationPhase::FilesystemPublished
                    ),
                    DatabaseRole::Global => matches!(
                        record.phase,
                        OperationPhase::IntentDurable
                            | OperationPhase::Prepared
                            | OperationPhase::FilesystemStaged
                            | OperationPhase::FilesystemPublished
                            | OperationPhase::SourceReconciled
                    ),
                },
                "required database phase already reconciled",
            )?;
        }
        match db.direction {
            ClaimDirection::RequiredForward | ClaimDirection::Routine => require(
                matches!(record.disposition, None | RetryPending | PartialNeedsRetry),
                "forward attempt disposition",
            )?,
            ClaimDirection::RequiredCompensation => require(
                matches!(
                    record.disposition,
                    CancelRequestedBeforePublish
                        | CancelRequestedAfterPublish
                        | FailedPreservingData
                ),
                "compensation attempt disposition",
            )?,
            ClaimDirection::LeaseMaintenance => require(
                binding.source.is_some()
                    && record.phase.is_pre_publication()
                    && record.disposition == None,
                "lease maintenance envelope",
            )?,
        }
        let sidecar_bytes =
            wal_bytes
                .checked_add(shm_bytes)
                .ok_or(database_claims::DatabaseClaimError(
                    "attempt sidecar overflow",
                ))?;
        require(
            database_bytes > 0
                && sidecar_bytes > 0
                && database_bytes <= db.ceiling
                && wal_bytes <= sidecar.wal_ceiling
                && shm_bytes <= sidecar.shm_ceiling
                && sidecar_bytes <= sidecar.ceiling,
            "attempt reservation exceeds exact slice",
        )?;
        for n in [database_bytes, wal_bytes, shm_bytes] {
            require(n <= i64::MAX as u64, "attempt SQLite integer")?;
            require(
                n % binding.allocation_unit == 0,
                "attempt allocation rounding",
            )?;
        }
        for id in [attempt, nonce, record.operation_id.as_uuid()] {
            require(!id.is_nil(), "nil attempt identity")?;
        }
        let family: serde_json::Value =
            serde_json::from_slice(&database_claims::command_bytes(record)?)
                .map_err(|_| database_claims::DatabaseClaimError("attempt command family"))?;
        let command = serde_json::json!({"schema":record.schema_version,"requirement":participant.requirement,"participant":participant.id,"direction":db.direction,"family":family});
        let command_bytes = encode_bounded(&command, MAX_START_BYTES)?;
        let mut hasher = blake3::Hasher::new_derive_key("yield.database-attempt-command.v1");
        hasher.update(&command_bytes);
        let compensation = db.direction == ClaimDirection::RequiredCompensation;
        require(
            participant.after >= participant.before,
            "revision domain cannot move backwards",
        )?;
        // Undoing content never rewinds an authoritative revision counter. An inverse that
        // changes the domain again advances once; a forward that leaves it unchanged does not
        // invent an authoritative mutation. The real issuer must verify the actual inverse.
        let after = if db.direction == ClaimDirection::LeaseMaintenance {
            participant.before
        } else if compensation && participant.after != participant.before {
            participant
                .after
                .checked_add(1)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or(database_claims::DatabaseClaimError(
                    "compensation revision overflow",
                ))?
        } else {
            participant.after
        };
        let value = Self {
            version: if inventory.encode()?.starts_with(b"YWCB\x02\x00") {
                2
            } else {
                1
            },
            attempt,
            nonce,
            operation: record.operation_id.as_uuid(),
            binding: binding.id,
            participant: participant.id,
            database_slice,
            sidecar_slice,
            inventory: inventory.encode()?,
            schema: record.schema_version,
            phase: record.phase,
            disposition: record.disposition,
            command,
            command_digest: *hasher.finalize().as_bytes(),
            revision_domain: participant.revision_domain,
            before: if compensation {
                participant.after
            } else {
                participant.before
            },
            after,
            database_bytes,
            wal_bytes,
            shm_bytes,
        };
        value.encode()?;
        Ok(value)
    }
    /// Restart validates immutable context and historical phase, without requiring the current
    /// operation to remain in that historical phase or recreating dispatch permission.
    pub(crate) fn validate_bound(
        &self,
        record: &StoredOperationRecord,
        inventory: &DatabaseClaimInventory,
        new_start: bool,
    ) -> Result<()> {
        let mut historical;
        let context = if new_start {
            record
        } else {
            historical = record.clone();
            #[cfg(test)]
            record_backing::observe(record, &historical, false);
            historical.phase = self.phase;
            historical.disposition = self.disposition;
            &historical
        };
        let expected = Self::from_context(
            context,
            inventory,
            self.attempt,
            self.nonce,
            self.database_slice,
            self.sidecar_slice,
            self.database_bytes,
            self.wal_bytes,
            self.shm_bytes,
        )?;
        require(
            self == &expected,
            "attempt differs from actual immutable intent/binding/phase",
        )
    }
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        require(
            self.inventory.len() <= database_claims::MAX_INVENTORY_BYTES
                && match self.version {
                    1 => {
                        self.inventory.starts_with(b"YWCB\x01\x00")
                            && self.revision_domain != RevisionDomain::SourceLeaseLifecycle
                    }
                    2 => self.inventory.starts_with(b"YWCB\x02\x00"),
                    _ => false,
                },
            "attempt image shape",
        )?;
        DatabaseClaimInventory::decode(&self.inventory)?;
        encode_bounded(self, MAX_START_BYTES)
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        require(
            !bytes.is_empty() && bytes.len() <= MAX_START_BYTES,
            "attempt encoding bound",
        )?;
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|_| database_claims::DatabaseClaimError("unsupported attempt image"))?;
        let canonical = encode_bounded(&value, MAX_START_BYTES)?;
        require(
            if value.inventory.len() > database_claims::MAX_INVENTORY_BYTES {
                inventory_bytes::canonical_overflow(bytes, &canonical, MAX_START_BYTES)?
            } else {
                canonical == bytes
            },
            "noncanonical attempt image",
        )?;
        value.encode()?;
        Ok(value)
    }
    pub(crate) fn digest(&self) -> Result<[u8; 32]> {
        let mut h = blake3::Hasher::new_derive_key(if self.version == 2 {
            "yield.database-attempt-start.v2"
        } else {
            "yield.database-attempt-start.v1"
        });
        h.update(&self.encode()?);
        Ok(*h.finalize().as_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum AttemptOutcomeKind {
    CommittedObservation,
    NoCommitObservation,
    Unknown,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatabaseAttemptOutcome {
    pub version: u8,
    pub observation: Uuid,
    pub attempt: Uuid,
    pub start_digest: [u8; 32],
    pub kind: AttemptOutcomeKind,
    pub receipt: Option<Uuid>,
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub allocated_database: Option<u64>,
    pub allocated_wal: Option<u64>,
    pub allocated_shm: Option<u64>,
    pub budget_observation: Option<[u8; 32]>,
}
impl DatabaseAttemptOutcome {
    pub(crate) fn validate(&self, start: &DatabaseAttemptStart) -> Result<()> {
        require(
            self.version == 1
                && !self.observation.is_nil()
                && self.attempt == start.attempt
                && self.start_digest == start.digest()?,
            "outcome start identity",
        )?;
        for n in [
            self.before,
            self.after,
            self.allocated_database,
            self.allocated_wal,
            self.allocated_shm,
        ]
        .into_iter()
        .flatten()
        {
            require(n <= i64::MAX as u64, "outcome integer bound")?;
        }
        if let Some(id) = self.receipt {
            require(!id.is_nil(), "nil outcome receipt")?;
        }
        if let Some(before) = self.before {
            require(before == start.before, "outcome prior revision")?;
        }
        match self.kind {
            AttemptOutcomeKind::CommittedObservation => require(
                self.receipt.is_some()
                    && self.before == Some(start.before)
                    && self.after == Some(start.after),
                "committed observation requires exact receipt/revisions",
            )?,
            AttemptOutcomeKind::NoCommitObservation => require(
                self.receipt.is_none() && self.after.is_none_or(|n| n == start.before),
                "no-commit observation cannot assert commit",
            )?,
            AttemptOutcomeKind::Unknown => {}
        }
        // Unknown allocation stays None. Known allocation observations never change the nominal
        // charge, including zero, no-commit, or a receipt that has not been genuinely reconciled.
        self.encode()?;
        Ok(())
    }
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        encode_bounded(self, MAX_OUTCOME_BYTES)
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        decode_canonical(bytes, MAX_OUTCOME_BYTES)
    }
}
fn decode_canonical<T: serde::de::DeserializeOwned + Serialize>(
    bytes: &[u8],
    cap: usize,
) -> Result<T> {
    require(
        !bytes.is_empty() && bytes.len() <= cap,
        "attempt encoding bound",
    )?;
    let value = serde_json::from_slice(bytes)
        .map_err(|_| database_claims::DatabaseClaimError("unsupported attempt image"))?;
    require(
        encode_bounded(&value, cap)? == bytes,
        "noncanonical attempt image",
    )?;
    Ok(value)
}
fn encode_bounded<T: Serialize>(value: &T, cap: usize) -> Result<Vec<u8>> {
    struct Writer {
        bytes: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self
                .bytes
                .len()
                .checked_add(b.len())
                .is_none_or(|n| n > self.cap)
            {
                return Err(std::io::Error::other("attempt image bound"));
            }
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| database_claims::DatabaseClaimError("attempt encoding bound"))?;
    Ok(writer.bytes)
}
