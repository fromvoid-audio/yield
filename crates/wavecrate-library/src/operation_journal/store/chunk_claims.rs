//! Durable post-intent chunk claim state.
//!
//! Chunk rows are deliberately separate from the immutable initial-intent record.  They bind a
//! stable chunk identity to one operation, one ordinary claim, and one bounded output identity.
//! The state transition is owner-controlled and is never inferred from a filesystem observation.

use rusqlite::{Connection, Row, params};
use thiserror::Error;
use uuid::Uuid;

use crate::durable_capacity::VolumeIdentity;
use crate::operation_journal::admission_claims::{AdmissionClaim, AdmissionClaimId};
use crate::operation_journal::full_capacity::FullCapacityAllocationClass;
use crate::operation_journal::model::OperationId;
use crate::operation_journal::participants::ParticipantId;

pub(super) const TABLE_NAME: &str = "operation_journal_chunk_claim";
pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_chunk_claim (
    chunk_id TEXT NOT NULL PRIMARY KEY,
    operation_id TEXT NOT NULL,
    claim_id TEXT NOT NULL UNIQUE,
    participant_id TEXT NOT NULL,
    output_key TEXT NOT NULL,
    logical_offset INTEGER NOT NULL,
    logical_length INTEGER NOT NULL,
    volume_device TEXT NOT NULL,
    allocation_class TEXT NOT NULL,
    allocated_bytes INTEGER NOT NULL,
    allocation_unit INTEGER NOT NULL,
    protected_free_bytes INTEGER NOT NULL,
    state TEXT NOT NULL,
    issue_id TEXT,
    release_id TEXT,
    CHECK (
        length(chunk_id) = 36
        AND substr(chunk_id, 9, 1) = '-'
        AND substr(chunk_id, 14, 1) = '-'
        AND substr(chunk_id, 19, 1) = '-'
        AND substr(chunk_id, 24, 1) = '-'
        AND length(replace(chunk_id, '-', '')) = 32
        AND chunk_id NOT GLOB '*[^0-9a-f-]*'
        AND chunk_id <> '00000000-0000-0000-0000-000000000000'
    ),
    CHECK (
        length(operation_id) = 36
        AND substr(operation_id, 9, 1) = '-'
        AND substr(operation_id, 14, 1) = '-'
        AND substr(operation_id, 19, 1) = '-'
        AND substr(operation_id, 24, 1) = '-'
        AND length(replace(operation_id, '-', '')) = 32
        AND operation_id NOT GLOB '*[^0-9a-f-]*'
        AND operation_id <> '00000000-0000-0000-0000-000000000000'
    ),
    CHECK (
        length(claim_id) = 36
        AND substr(claim_id, 9, 1) = '-'
        AND substr(claim_id, 14, 1) = '-'
        AND substr(claim_id, 19, 1) = '-'
        AND substr(claim_id, 24, 1) = '-'
        AND length(replace(claim_id, '-', '')) = 32
        AND claim_id NOT GLOB '*[^0-9a-f-]*'
        AND claim_id <> '00000000-0000-0000-0000-000000000000'
    ),
    CHECK (
        length(participant_id) = 36
        AND substr(participant_id, 9, 1) = '-'
        AND substr(participant_id, 14, 1) = '-'
        AND substr(participant_id, 19, 1) = '-'
        AND substr(participant_id, 24, 1) = '-'
        AND length(replace(participant_id, '-', '')) = 32
        AND participant_id NOT GLOB '*[^0-9a-f-]*'
        AND participant_id <> '00000000-0000-0000-0000-000000000000'
    ),
    CHECK (length(output_key) BETWEEN 1 AND 1024),
    CHECK (typeof(logical_offset) = 'integer' AND logical_offset >= 0),
    CHECK (typeof(logical_length) = 'integer' AND logical_length > 0),
    CHECK (
        length(volume_device) BETWEEN 1 AND 20
        AND volume_device NOT GLOB '*[^0-9]*'
        AND (volume_device = '0' OR volume_device GLOB '[1-9]*')
    ),
    CHECK (allocation_class IN (
        'DestinationStaging',
        'JournalRecord',
        'SourceDatabaseCommit',
        'SourceWalShm',
        'GlobalDatabaseCommit',
        'GlobalWalShm',
        'ExistingDestination',
        'ExistingRecoveryPayload',
        'OperationRecoveryPayload'
    )),
    CHECK (typeof(allocated_bytes) = 'integer' AND allocated_bytes > 0),
    CHECK (typeof(allocation_unit) = 'integer' AND allocation_unit > 0),
    CHECK (typeof(protected_free_bytes) = 'integer' AND protected_free_bytes > 0),
    CHECK (state IN ('Claimed', 'Issued', 'Released')),
    CHECK (
        (state = 'Claimed' AND issue_id IS NULL AND release_id IS NULL)
        OR (state = 'Issued' AND issue_id IS NOT NULL AND release_id IS NULL)
        OR (state = 'Released' AND release_id IS NOT NULL)
    ),
    FOREIGN KEY (claim_id) REFERENCES operation_journal_ordinary_claim(claim_id)
)";

pub(crate) const MAX_PAGE_ROWS: usize = 256;
const MAX_OUTPUT_KEY_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ChunkId(Uuid);

#[allow(dead_code)]
impl ChunkId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub(crate) fn from_uuid(value: Uuid) -> Result<Self, ChunkClaimValidationError> {
        if value.is_nil() {
            return Err(ChunkClaimValidationError::NilIdentity("chunk"));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_uuid(self) -> Uuid {
        self.0
    }

    pub(crate) fn parse(value: &str) -> Result<Self, ChunkClaimValidationError> {
        Self::from_uuid(
            Uuid::parse_str(value)
                .map_err(|_| ChunkClaimValidationError::InvalidIdentity("chunk"))?,
        )
    }
}

impl std::fmt::Display for ChunkId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkClaimRequest {
    chunk_id: ChunkId,
    operation_id: OperationId,
    claim: AdmissionClaim,
    participant_id: ParticipantId,
    output_key: String,
    logical_offset: u64,
    logical_length: u64,
    allocation_unit: u64,
    protected_free_bytes: u64,
}

#[allow(dead_code)]
impl ChunkClaimRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        chunk_id: ChunkId,
        operation_id: OperationId,
        claim: AdmissionClaim,
        participant_id: ParticipantId,
        output_key: String,
        logical_offset: u64,
        logical_length: u64,
        allocation_unit: u64,
        protected_free_bytes: u64,
    ) -> Result<Self, ChunkClaimValidationError> {
        let request = Self {
            chunk_id,
            operation_id,
            claim,
            participant_id,
            output_key,
            logical_offset,
            logical_length,
            allocation_unit,
            protected_free_bytes,
        };
        request.validate()?;
        Ok(request)
    }

    pub(crate) fn validate(&self) -> Result<(), ChunkClaimValidationError> {
        if self.operation_id.as_uuid().is_nil() {
            return Err(ChunkClaimValidationError::NilIdentity("operation"));
        }
        self.claim
            .validate()
            .map_err(|_| ChunkClaimValidationError::InvalidClaim)?;
        if !supports_chunk_output_class(self.claim.allocation_class()) {
            return Err(ChunkClaimValidationError::UnsupportedAllocationClass);
        }
        if self.participant_id.as_uuid().is_nil() {
            return Err(ChunkClaimValidationError::NilIdentity("participant"));
        }
        if self.output_key.is_empty() || self.output_key.len() > MAX_OUTPUT_KEY_BYTES {
            return Err(ChunkClaimValidationError::OutputKeyBounds);
        }
        if self.logical_length == 0
            || self
                .logical_offset
                .checked_add(self.logical_length)
                .is_none()
        {
            return Err(ChunkClaimValidationError::LogicalRangeBounds);
        }
        if self.allocation_unit == 0 || self.protected_free_bytes == 0 {
            return Err(ChunkClaimValidationError::AllocationFactsBounds);
        }
        if i64::try_from(self.logical_offset).is_err()
            || i64::try_from(self.logical_length).is_err()
            || i64::try_from(self.claim.allocated_bytes()).is_err()
            || i64::try_from(self.allocation_unit).is_err()
            || i64::try_from(self.protected_free_bytes).is_err()
        {
            return Err(ChunkClaimValidationError::SqliteIntegerBounds);
        }
        let allocation = crate::operation_journal::full_capacity::FullCapacityAllocation::new(
            self.claim.volume().clone(),
            self.allocation_unit,
            self.claim.allocation_class(),
            self.logical_length,
            self.claim.allocated_bytes(),
            self.protected_free_bytes,
        )
        .map_err(|_| ChunkClaimValidationError::InvalidAllocation)?;
        crate::operation_journal::full_capacity::FullCapacityPlan::new(vec![allocation])
            .map_err(|_| ChunkClaimValidationError::InvalidAllocation)?;
        Ok(())
    }

    pub(crate) fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn claim(&self) -> &AdmissionClaim {
        &self.claim
    }

    pub(crate) fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    pub(crate) fn output_key(&self) -> &str {
        &self.output_key
    }

    pub(crate) fn logical_offset(&self) -> u64 {
        self.logical_offset
    }

    pub(crate) fn logical_length(&self) -> u64 {
        self.logical_length
    }

    pub(crate) fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }

    pub(crate) fn protected_free_bytes(&self) -> u64 {
        self.protected_free_bytes
    }

    pub(crate) fn identity(&self) -> ChunkWriteIdentity {
        ChunkWriteIdentity {
            participant_id: self.participant_id,
            output_key: self.output_key.clone(),
            logical_offset: self.logical_offset,
            logical_length: self.logical_length,
        }
    }

    pub(crate) fn plan(
        &self,
    ) -> Result<crate::operation_journal::full_capacity::FullCapacityPlan, ChunkClaimValidationError>
    {
        let allocation = crate::operation_journal::full_capacity::FullCapacityAllocation::new(
            self.claim.volume().clone(),
            self.allocation_unit,
            self.claim.allocation_class(),
            self.logical_length,
            self.claim.allocated_bytes(),
            self.protected_free_bytes,
        )
        .map_err(|_| ChunkClaimValidationError::InvalidAllocation)?;
        crate::operation_journal::full_capacity::FullCapacityPlan::new(vec![allocation])
            .map_err(|_| ChunkClaimValidationError::InvalidAllocation)
    }

    pub(crate) fn inventory(
        &self,
    ) -> Result<
        crate::operation_journal::admission_claims::AdmissionClaimInventory,
        ChunkClaimValidationError,
    > {
        crate::operation_journal::admission_claims::AdmissionClaimInventory::new(vec![
            self.claim.clone(),
        ])
        .map_err(|_| ChunkClaimValidationError::InvalidClaim)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkWriteIdentity {
    participant_id: ParticipantId,
    output_key: String,
    logical_offset: u64,
    logical_length: u64,
}

#[allow(dead_code)]
impl ChunkWriteIdentity {
    pub(crate) fn new(
        participant_id: ParticipantId,
        output_key: String,
        logical_offset: u64,
        logical_length: u64,
    ) -> Result<Self, ChunkClaimValidationError> {
        let identity = Self {
            participant_id,
            output_key,
            logical_offset,
            logical_length,
        };
        if identity.participant_id.as_uuid().is_nil()
            || identity.output_key.is_empty()
            || identity.output_key.len() > MAX_OUTPUT_KEY_BYTES
            || identity.logical_length == 0
            || identity
                .logical_offset
                .checked_add(identity.logical_length)
                .is_none()
        {
            return Err(ChunkClaimValidationError::InvalidWriteIdentity);
        }
        Ok(identity)
    }

    fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// The store's readback of a successful Claimed-to-Issued CAS.  A read record is deliberately
/// not enough to construct this value: only the owner transition below can mint the sealed
/// receipt that crosses into the owner-thread issuance registry.
#[derive(Debug)]
pub(crate) struct ChunkIssueReceipt {
    record: ChunkClaimRecord,
}

impl ChunkIssueReceipt {
    pub(super) fn from_record(record: ChunkClaimRecord) -> Option<Self> {
        (record.state == ChunkClaimState::Issued && record.issue_id.is_some())
            .then_some(Self { record })
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn for_test(record: ChunkClaimRecord) -> Self {
        Self { record }
    }

    pub(crate) fn chunk_id(&self) -> ChunkId {
        self.record.chunk_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.record.operation_id
    }

    pub(crate) fn issue_id(&self) -> Uuid {
        self.record
            .issue_id
            .expect("sealed chunk issue receipt has an issue identity")
    }

    pub(crate) fn write_identity(&self) -> ChunkWriteIdentity {
        self.record.write_identity()
    }

    pub(crate) fn logical_length(&self) -> u64 {
        self.record.logical_length
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        self.record.allocated_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChunkClaimState {
    Claimed,
    Issued,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkClaimRecord {
    chunk_id: ChunkId,
    operation_id: OperationId,
    claim_id: AdmissionClaimId,
    participant_id: ParticipantId,
    output_key: String,
    logical_offset: u64,
    logical_length: u64,
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
    allocation_unit: u64,
    protected_free_bytes: u64,
    state: ChunkClaimState,
    issue_id: Option<Uuid>,
    release_id: Option<Uuid>,
}

#[allow(dead_code)]
impl ChunkClaimRecord {
    pub(crate) fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }
    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }
    pub(crate) fn claim_id(&self) -> AdmissionClaimId {
        self.claim_id
    }
    pub(crate) fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }
    pub(crate) fn output_key(&self) -> &str {
        &self.output_key
    }
    pub(crate) fn logical_offset(&self) -> u64 {
        self.logical_offset
    }
    pub(crate) fn logical_length(&self) -> u64 {
        self.logical_length
    }
    pub(crate) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }
    pub(crate) fn allocation_class(&self) -> FullCapacityAllocationClass {
        self.allocation_class
    }
    pub(crate) fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }
    pub(crate) fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }
    pub(crate) fn protected_free_bytes(&self) -> u64 {
        self.protected_free_bytes
    }
    pub(crate) fn state(&self) -> ChunkClaimState {
        self.state
    }
    pub(crate) fn issue_id(&self) -> Option<Uuid> {
        self.issue_id
    }
    pub(crate) fn release_id(&self) -> Option<Uuid> {
        self.release_id
    }
    pub(crate) fn write_identity(&self) -> ChunkWriteIdentity {
        ChunkWriteIdentity {
            participant_id: self.participant_id,
            output_key: self.output_key.clone(),
            logical_offset: self.logical_offset,
            logical_length: self.logical_length,
        }
    }
    pub(crate) fn matches_request(&self, request: &ChunkClaimRequest) -> bool {
        self.chunk_id == request.chunk_id
            && self.operation_id == request.operation_id
            && self.claim_id == request.claim.claim_id()
            && self.participant_id == request.participant_id
            && self.output_key == request.output_key
            && self.logical_offset == request.logical_offset
            && self.logical_length == request.logical_length
            && self.volume == *request.claim.volume()
            && self.allocation_class == request.claim.allocation_class()
            && self.allocated_bytes == request.claim.allocated_bytes()
            && self.allocation_unit == request.allocation_unit
            && self.protected_free_bytes == request.protected_free_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChunkClaimCursor(ChunkId);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkClaimPage {
    records: Vec<ChunkClaimRecord>,
    next_cursor: Option<ChunkClaimCursor>,
}

impl ChunkClaimPage {
    pub(crate) fn records(&self) -> &[ChunkClaimRecord] {
        &self.records
    }
    pub(crate) fn next_cursor(&self) -> Option<ChunkClaimCursor> {
        self.next_cursor
    }
}

#[derive(Debug, Error)]
pub(crate) enum ChunkClaimValidationError {
    #[error("nil {0} identity")]
    NilIdentity(&'static str),
    #[error("invalid {0} identity")]
    InvalidIdentity(&'static str),
    #[error("chunk claim is invalid")]
    InvalidClaim,
    #[error("chunk output key is empty or exceeds 1024 bytes")]
    OutputKeyBounds,
    #[error("chunk logical range is empty or overflows")]
    LogicalRangeBounds,
    #[error("chunk allocation facts are invalid")]
    AllocationFactsBounds,
    #[error("chunk allocation does not satisfy the full capacity contract")]
    InvalidAllocation,
    #[error("chunk output allocation class does not carry incremental write capacity")]
    UnsupportedAllocationClass,
    #[error("chunk claim exceeds SQLite INTEGER bounds")]
    SqliteIntegerBounds,
    #[error("chunk write identity is invalid")]
    #[allow(dead_code)]
    InvalidWriteIdentity,
}

#[derive(Debug, Error)]
pub(crate) enum ChunkClaimReadError {
    #[error("chunk-claim SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("chunk-claim page limit {requested} exceeds maximum {maximum}")]
    PageLimitExceeded { requested: usize, maximum: usize },
    #[error("chunk-claim row is malformed: {0}")]
    Malformed(String),
}

#[derive(Debug, Error)]
pub(crate) enum ChunkClaimWriteError {
    #[error("chunk-claim SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("chunk-claim readback did not match the submitted row")]
    ReadbackMismatch,
    #[error("chunk claim transition is invalid: {0}")]
    InvalidTransition(String),
}

pub(super) fn insert_claimed_inside_transaction(
    connection: &Connection,
    request: &ChunkClaimRequest,
) -> Result<(), ChunkClaimWriteError> {
    request
        .validate()
        .map_err(|error| ChunkClaimWriteError::InvalidTransition(error.to_string()))?;
    connection.execute(
        &format!(
            "INSERT INTO {TABLE_NAME}
             (chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
              logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
              protected_free_bytes, state, issue_id, release_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'Claimed', NULL, NULL)"
        ),
        params![
            request.chunk_id.to_string(),
            request.operation_id.to_string(),
            request.claim.claim_id().to_string(),
            request.participant_id.to_string(),
            &request.output_key,
            i64::try_from(request.logical_offset).expect("validated SQLite range"),
            i64::try_from(request.logical_length).expect("validated SQLite range"),
            request.claim.volume().device.to_string(),
            allocation_class_text(request.claim.allocation_class()),
            i64::try_from(request.claim.allocated_bytes()).expect("validated SQLite range"),
            i64::try_from(request.allocation_unit).expect("validated SQLite range"),
            i64::try_from(request.protected_free_bytes).expect("validated SQLite range"),
        ],
    )?;
    Ok(())
}

pub(super) fn mark_issued_inside_transaction(
    connection: &Connection,
    chunk_id: ChunkId,
    operation_id: OperationId,
    issue_id: Uuid,
) -> Result<Option<ChunkClaimRecord>, ChunkClaimWriteError> {
    if issue_id.is_nil() {
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "issue identity is nil",
        )));
    }
    let Some(current) = read(connection, chunk_id)
        .map_err(|error| ChunkClaimWriteError::InvalidTransition(error.to_string()))?
    else {
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "chunk claim was not found",
        )));
    };
    if current.operation_id != operation_id {
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "chunk claim belongs to a different operation",
        )));
    }
    match current.state {
        ChunkClaimState::Claimed => {
            connection.execute(
                &format!("UPDATE {TABLE_NAME} SET state = 'Issued', issue_id = ?2 WHERE chunk_id = ?1 AND operation_id = ?3 AND state = 'Claimed'"),
                params![chunk_id.to_string(), issue_id.to_string(), operation_id.to_string()],
            )?;
            let updated = read(connection, chunk_id)
                .map_err(|error| ChunkClaimWriteError::InvalidTransition(error.to_string()))?
                .ok_or_else(|| ChunkClaimWriteError::ReadbackMismatch)?;
            if updated.state != ChunkClaimState::Issued || updated.issue_id != Some(issue_id) {
                return Err(ChunkClaimWriteError::ReadbackMismatch);
            }
            Ok(Some(updated))
        }
        ChunkClaimState::Issued | ChunkClaimState::Released => Ok(None),
    }
}

pub(super) fn mark_released_inside_transaction(
    connection: &Connection,
    chunk_id: ChunkId,
    operation_id: OperationId,
    release_id: Uuid,
) -> Result<ChunkClaimRecord, ChunkClaimWriteError> {
    if release_id.is_nil() {
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "release identity is nil",
        )));
    }
    let Some(current) = read(connection, chunk_id)
        .map_err(|error| ChunkClaimWriteError::InvalidTransition(error.to_string()))?
    else {
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "chunk claim was not found",
        )));
    };
    if current.operation_id != operation_id {
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "chunk claim belongs to a different operation",
        )));
    }
    if current.state == ChunkClaimState::Released {
        if current.release_id == Some(release_id) {
            return Ok(current);
        }
        return Err(ChunkClaimWriteError::InvalidTransition(String::from(
            "chunk claim was released with a different identity",
        )));
    }
    connection.execute(
        &format!("UPDATE {TABLE_NAME} SET state = 'Released', release_id = ?2 WHERE chunk_id = ?1 AND operation_id = ?3 AND state IN ('Claimed', 'Issued')"),
        params![chunk_id.to_string(), release_id.to_string(), operation_id.to_string()],
    )?;
    let updated = read(connection, chunk_id)
        .map_err(|error| ChunkClaimWriteError::InvalidTransition(error.to_string()))?
        .ok_or(ChunkClaimWriteError::ReadbackMismatch)?;
    if updated.state != ChunkClaimState::Released || updated.release_id != Some(release_id) {
        return Err(ChunkClaimWriteError::ReadbackMismatch);
    }
    Ok(updated)
}

pub(crate) fn read(
    connection: &Connection,
    chunk_id: ChunkId,
) -> Result<Option<ChunkClaimRecord>, ChunkClaimReadError> {
    let mut statement = connection.prepare(&format!(
        "SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
         logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
         protected_free_bytes, state, issue_id, release_id
         FROM {TABLE_NAME} WHERE chunk_id = ?1"
    ))?;
    let mut rows = statement.query(params![chunk_id.to_string()])?;
    rows.next()?.map(decode_row).transpose()
}

pub(crate) fn read_page(
    connection: &Connection,
    cursor: Option<ChunkClaimCursor>,
    limit: usize,
) -> Result<ChunkClaimPage, ChunkClaimReadError> {
    if limit > MAX_PAGE_ROWS {
        return Err(ChunkClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        });
    }
    let fetch_limit = i64::try_from(limit.checked_add(1).ok_or_else(|| {
        ChunkClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        }
    })?)
    .expect("bounded chunk page limit");
    let sql = if cursor.is_some() {
        format!(
            "SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
             logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
             protected_free_bytes, state, issue_id, release_id FROM {TABLE_NAME}
             WHERE chunk_id > ?1 ORDER BY chunk_id ASC LIMIT ?2"
        )
    } else {
        format!(
            "SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
             logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
             protected_free_bytes, state, issue_id, release_id FROM {TABLE_NAME}
             ORDER BY chunk_id ASC LIMIT ?1"
        )
    };
    let mut statement = connection.prepare(&sql)?;
    let mut rows = match cursor {
        Some(cursor) => statement.query(params![cursor.0.to_string(), fetch_limit])?,
        None => statement.query(params![fetch_limit])?,
    };
    let mut records = Vec::with_capacity(limit);
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        if records.len() == limit {
            has_more = true;
            break;
        }
        records.push(decode_row(row)?);
    }
    Ok(ChunkClaimPage {
        next_cursor: has_more
            .then(|| records.last().map(|row| ChunkClaimCursor(row.chunk_id)))
            .flatten(),
        records,
    })
}

/// Check the relation size with a bounded keyset-style read.  A plain COUNT(*) would scan
/// arbitrary historical rows before rejecting an oversized relation; admission and restart
/// validation must do at most one page of work for this guard.
pub(super) fn exceeds_row_limit(
    connection: &Connection,
    limit: usize,
) -> Result<bool, ChunkClaimReadError> {
    if limit > MAX_PAGE_ROWS {
        return Err(ChunkClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        });
    }
    let fetch_limit = i64::try_from(limit.checked_add(1).ok_or_else(|| {
        ChunkClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        }
    })?)
    .expect("bounded chunk row limit");
    let mut statement = connection.prepare(&format!(
        "SELECT chunk_id FROM {TABLE_NAME} ORDER BY chunk_id ASC LIMIT ?1"
    ))?;
    let mut rows = statement.query(params![fetch_limit])?;
    let mut seen = 0usize;
    while rows.next()?.is_some() {
        seen = seen.saturating_add(1);
        if seen > limit {
            return Ok(true);
        }
    }
    Ok(false)
}

fn decode_row(row: &Row<'_>) -> Result<ChunkClaimRecord, ChunkClaimReadError> {
    let chunk_id = ChunkId::parse(row.get::<_, String>(0)?.as_str())
        .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let operation_id = OperationId::parse(row.get::<_, String>(1)?.as_str())
        .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let claim_id = AdmissionClaimId::from_uuid(
        Uuid::parse_str(row.get::<_, String>(2)?.as_str())
            .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?,
    )
    .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let participant_id = ParticipantId::parse(row.get::<_, String>(3)?.as_str())
        .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let output_key: String = row.get(4)?;
    if output_key.is_empty() || output_key.len() > MAX_OUTPUT_KEY_BYTES {
        return Err(ChunkClaimReadError::Malformed(String::from(
            "output key bounds",
        )));
    }
    let logical_offset = positive_u64(row.get::<_, i64>(5)?, "logical offset")?;
    let logical_length = positive_u64(row.get::<_, i64>(6)?, "logical length")?;
    let volume_device = row
        .get::<_, String>(7)?
        .parse::<u64>()
        .map_err(|_| ChunkClaimReadError::Malformed(String::from("volume device bounds")))?;
    let volume = VolumeIdentity {
        device: volume_device as u64,
    };
    let allocation_class = allocation_class(row.get::<_, String>(8)?.as_str())?;
    if !supports_chunk_output_class(allocation_class) {
        return Err(ChunkClaimReadError::Malformed(String::from(
            "chunk output allocation class is not incrementally chargeable",
        )));
    }
    let allocated_bytes = positive_u64(row.get::<_, i64>(9)?, "allocated bytes")?;
    let allocation_unit = positive_u64(row.get::<_, i64>(10)?, "allocation unit")?;
    let protected_free_bytes = positive_u64(row.get::<_, i64>(11)?, "protected floor")?;
    let state = match row.get::<_, String>(12)?.as_str() {
        "Claimed" => ChunkClaimState::Claimed,
        "Issued" => ChunkClaimState::Issued,
        "Released" => ChunkClaimState::Released,
        value => {
            return Err(ChunkClaimReadError::Malformed(format!(
                "unknown state {value}"
            )));
        }
    };
    let issue_id = optional_uuid(row.get(13)?)?;
    let release_id = optional_uuid(row.get(14)?)?;
    if (state == ChunkClaimState::Claimed && (issue_id.is_some() || release_id.is_some()))
        || (state == ChunkClaimState::Issued && (issue_id.is_none() || release_id.is_some()))
        || (state == ChunkClaimState::Released && release_id.is_none())
    {
        return Err(ChunkClaimReadError::Malformed(String::from(
            "state transition identities",
        )));
    }
    Ok(ChunkClaimRecord {
        chunk_id,
        operation_id,
        claim_id,
        participant_id,
        output_key,
        logical_offset,
        logical_length,
        volume,
        allocation_class,
        allocated_bytes,
        allocation_unit,
        protected_free_bytes,
        state,
        issue_id,
        release_id,
    })
}

fn positive_u64(value: i64, field: &str) -> Result<u64, ChunkClaimReadError> {
    if value < 0 || (field != "logical offset" && value == 0) {
        return Err(ChunkClaimReadError::Malformed(format!("{field} bounds")));
    }
    Ok(value as u64)
}

fn optional_uuid(value: Option<String>) -> Result<Option<Uuid>, ChunkClaimReadError> {
    value
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))
        })
        .transpose()
}

fn allocation_class(value: &str) -> Result<FullCapacityAllocationClass, ChunkClaimReadError> {
    match value {
        "DestinationStaging" => Ok(FullCapacityAllocationClass::DestinationStaging),
        "JournalRecord" => Ok(FullCapacityAllocationClass::JournalRecord),
        "SourceDatabaseCommit" => Ok(FullCapacityAllocationClass::SourceDatabaseCommit),
        "SourceWalShm" => Ok(FullCapacityAllocationClass::SourceWalShm),
        "GlobalDatabaseCommit" => Ok(FullCapacityAllocationClass::GlobalDatabaseCommit),
        "GlobalWalShm" => Ok(FullCapacityAllocationClass::GlobalWalShm),
        "ExistingDestination" => Ok(FullCapacityAllocationClass::ExistingDestination),
        "ExistingRecoveryPayload" => Ok(FullCapacityAllocationClass::ExistingRecoveryPayload),
        "OperationRecoveryPayload" => Ok(FullCapacityAllocationClass::OperationRecoveryPayload),
        value => Err(ChunkClaimReadError::Malformed(format!(
            "unknown allocation class {value}"
        ))),
    }
}

fn allocation_class_text(value: FullCapacityAllocationClass) -> &'static str {
    match value {
        FullCapacityAllocationClass::DestinationStaging => "DestinationStaging",
        FullCapacityAllocationClass::JournalRecord => "JournalRecord",
        FullCapacityAllocationClass::SourceDatabaseCommit => "SourceDatabaseCommit",
        FullCapacityAllocationClass::SourceWalShm => "SourceWalShm",
        FullCapacityAllocationClass::GlobalDatabaseCommit => "GlobalDatabaseCommit",
        FullCapacityAllocationClass::GlobalWalShm => "GlobalWalShm",
        FullCapacityAllocationClass::ExistingDestination => "ExistingDestination",
        FullCapacityAllocationClass::ExistingRecoveryPayload => "ExistingRecoveryPayload",
        FullCapacityAllocationClass::OperationRecoveryPayload => "OperationRecoveryPayload",
    }
}

fn supports_chunk_output_class(value: FullCapacityAllocationClass) -> bool {
    !matches!(
        value,
        FullCapacityAllocationClass::ExistingDestination
            | FullCapacityAllocationClass::ExistingRecoveryPayload
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR;

    #[test]
    fn existing_capacity_classes_cannot_bypass_incremental_chunk_charge() {
        for allocation_class in [
            FullCapacityAllocationClass::ExistingDestination,
            FullCapacityAllocationClass::ExistingRecoveryPayload,
        ] {
            let claim = AdmissionClaim::new(
                AdmissionClaimId::new(),
                VolumeIdentity { device: 17 },
                allocation_class,
                4096,
            )
            .expect("valid claim");
            let error = ChunkClaimRequest::new(
                ChunkId::new(),
                OperationId::new(),
                claim,
                ParticipantId::new(),
                String::from("segment"),
                0,
                4096,
                4096,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect_err("existing capacity must not mint a chunk charge");
            assert!(matches!(
                error,
                ChunkClaimValidationError::UnsupportedAllocationClass
            ));
        }
    }
}
