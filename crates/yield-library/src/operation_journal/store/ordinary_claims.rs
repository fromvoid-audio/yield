//! Durable observations of ordinary capacity claims.
//!
//! Rows in this table preserve the claim identity and the operation association that an owner
//! recorded. Ordinary claim rows remain immutable observations; the private release relation is
//! written only by the owner-thread store transaction and proves one logical release identity.
//! Neither relation grants admission, physical cleanup, or physical-capacity authority.

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row, params};
use thiserror::Error;
use uuid::Uuid;

use crate::durable_capacity::VolumeIdentity;
use crate::operation_journal::admission_claims::{AdmissionClaimId, MAX_ADMISSION_CLAIMS};
use crate::operation_journal::full_capacity::FullCapacityAllocationClass;
use crate::operation_journal::model::OperationId;

pub(super) const TABLE_NAME: &str = "operation_journal_ordinary_claim";
pub(super) const RELEASE_TABLE_NAME: &str = "operation_journal_ordinary_claim_release";
pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_ordinary_claim (
    claim_id TEXT NOT NULL PRIMARY KEY,
    operation_id TEXT NOT NULL,
    volume_device TEXT NOT NULL,
    allocation_class TEXT NOT NULL,
    allocated_bytes INTEGER NOT NULL,
    CHECK (
        length(claim_id) = 36
        AND substr(claim_id, 9, 1) = '-'
        AND substr(claim_id, 14, 1) = '-'
        AND substr(claim_id, 19, 1) = '-'
        AND substr(claim_id, 24, 1) = '-'
        AND length(replace(claim_id, '-', '')) = 32
        AND claim_id NOT GLOB '*[^0-9a-f-]*'
    ),
    CHECK (claim_id <> '00000000-0000-0000-0000-000000000000'),
    CHECK (
        length(operation_id) = 36
        AND substr(operation_id, 9, 1) = '-'
        AND substr(operation_id, 14, 1) = '-'
        AND substr(operation_id, 19, 1) = '-'
        AND substr(operation_id, 24, 1) = '-'
        AND length(replace(operation_id, '-', '')) = 32
        AND operation_id NOT GLOB '*[^0-9a-f-]*'
    ),
    CHECK (operation_id <> '00000000-0000-0000-0000-000000000000'),
    CHECK (
        length(volume_device) BETWEEN 1 AND 20
        AND volume_device NOT GLOB '*[^0-9]*'
        AND (
            volume_device = '0'
            OR volume_device GLOB '[1-9]*'
        )
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
    CHECK (typeof(allocated_bytes) = 'integer' AND allocated_bytes > 0)
)";
pub(super) const CREATE_RELEASE_TABLE: &str =
    "CREATE TABLE operation_journal_ordinary_claim_release (
    claim_id TEXT NOT NULL PRIMARY KEY,
    operation_id TEXT NOT NULL,
    release_id TEXT NOT NULL UNIQUE,
    CHECK (
        length(claim_id) = 36
        AND substr(claim_id, 9, 1) = '-'
        AND substr(claim_id, 14, 1) = '-'
        AND substr(claim_id, 19, 1) = '-'
        AND substr(claim_id, 24, 1) = '-'
        AND length(replace(claim_id, '-', '')) = 32
        AND claim_id NOT GLOB '*[^0-9a-f-]*'
    ),
    CHECK (claim_id <> '00000000-0000-0000-0000-000000000000'),
    CHECK (
        length(operation_id) = 36
        AND substr(operation_id, 9, 1) = '-'
        AND substr(operation_id, 14, 1) = '-'
        AND substr(operation_id, 19, 1) = '-'
        AND substr(operation_id, 24, 1) = '-'
        AND length(replace(operation_id, '-', '')) = 32
        AND operation_id NOT GLOB '*[^0-9a-f-]*'
    ),
    CHECK (operation_id <> '00000000-0000-0000-0000-000000000000'),
    CHECK (
        length(release_id) = 36
        AND substr(release_id, 9, 1) = '-'
        AND substr(release_id, 14, 1) = '-'
        AND substr(release_id, 19, 1) = '-'
        AND substr(release_id, 24, 1) = '-'
        AND length(replace(release_id, '-', '')) = 32
        AND release_id NOT GLOB '*[^0-9a-f-]*'
    ),
    CHECK (release_id <> '00000000-0000-0000-0000-000000000000'),
    FOREIGN KEY (claim_id) REFERENCES operation_journal_ordinary_claim(claim_id)
)";

pub(crate) const MAX_PAGE_ROWS: usize = MAX_ADMISSION_CLAIMS;
const MAX_UUID_TEXT_BYTES: usize = 36;
const MAX_VOLUME_DEVICE_TEXT_BYTES: usize = 20;
const MAX_ALLOCATION_CLASS_TEXT_BYTES: usize = 26;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimCursor {
    claim_id: AdmissionClaimId,
}

impl OrdinaryClaimCursor {
    pub(crate) fn from_claim_id(claim_id: AdmissionClaimId) -> Self {
        Self { claim_id }
    }

    pub(crate) fn claim_id(self) -> AdmissionClaimId {
        self.claim_id
    }

    fn as_text(self) -> String {
        self.claim_id.to_string()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimRecord {
    claim_id: AdmissionClaimId,
    operation_id: OperationId,
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
}

/// Immutable proof that one durable ordinary claim was logically released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimReleaseRecord {
    claim_id: AdmissionClaimId,
    operation_id: OperationId,
    release_id: Uuid,
}

impl OrdinaryClaimReleaseRecord {
    pub(crate) fn new(
        claim_id: AdmissionClaimId,
        operation_id: OperationId,
        release_id: Uuid,
    ) -> Self {
        Self {
            claim_id,
            operation_id,
            release_id,
        }
    }

    pub(crate) fn claim_id(self) -> AdmissionClaimId {
        self.claim_id
    }

    pub(crate) fn operation_id(self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn release_id(self) -> Uuid {
        self.release_id
    }
}

impl OrdinaryClaimRecord {
    pub(crate) fn claim_id(&self) -> AdmissionClaimId {
        self.claim_id
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimPage {
    records: Vec<OrdinaryClaimRecord>,
    next_cursor: Option<OrdinaryClaimCursor>,
}

impl OrdinaryClaimPage {
    pub(crate) fn records(&self) -> &[OrdinaryClaimRecord] {
        &self.records
    }

    pub(crate) fn next_cursor(&self) -> Option<OrdinaryClaimCursor> {
        self.next_cursor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrdinaryClaimReleasePage {
    records: Vec<OrdinaryClaimReleaseRecord>,
    next_cursor: Option<OrdinaryClaimCursor>,
}

impl OrdinaryClaimReleasePage {
    pub(crate) fn records(&self) -> &[OrdinaryClaimReleaseRecord] {
        &self.records
    }

    pub(crate) fn next_cursor(&self) -> Option<OrdinaryClaimCursor> {
        self.next_cursor
    }
}

#[derive(Debug, Error)]
pub(crate) enum OrdinaryClaimReadError {
    #[error("ordinary-claim SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("ordinary-claim namespace validation failed: {0}")]
    Namespace(String),
    #[error("ordinary-claim page limit {requested} exceeds the maximum {maximum}")]
    PageLimitExceeded { requested: usize, maximum: usize },
    #[error("ordinary-claim row is malformed{claim}: {reason}")]
    MalformedRow { claim: String, reason: String },
}

#[derive(Debug, Error)]
pub(crate) enum OrdinaryClaimWriteError {
    #[error(
        "ordinary claim {claim_id} allocated bytes exceed SQLite INTEGER range: {allocated_bytes}"
    )]
    AllocatedBytesOutOfRange {
        claim_id: AdmissionClaimId,
        allocated_bytes: u64,
    },
    #[error("ordinary claim readback for operation {operation_id} did not match the submitted set")]
    ReadbackMismatch { operation_id: OperationId },
    #[error(
        "ordinary claim release readback for claim {claim_id} did not match the submitted identity"
    )]
    ReleaseReadbackMismatch { claim_id: AdmissionClaimId },
    #[error("ordinary claim SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Read(#[from] OrdinaryClaimReadError),
}

pub(super) fn validate_storage_values(
    claims: &crate::operation_journal::admission_claims::AdmissionClaimInventory,
) -> Result<(), OrdinaryClaimWriteError> {
    for claim in claims.claims() {
        if i64::try_from(claim.allocated_bytes()).is_err() {
            return Err(OrdinaryClaimWriteError::AllocatedBytesOutOfRange {
                claim_id: claim.claim_id(),
                allocated_bytes: claim.allocated_bytes(),
            });
        }
    }
    Ok(())
}

pub(super) fn insert_rows_inside_transaction(
    connection: &Connection,
    operation_id: OperationId,
    claims: &crate::operation_journal::admission_claims::AdmissionClaimInventory,
) -> Result<(), OrdinaryClaimWriteError> {
    validate_storage_values(claims)?;
    let operation_text = operation_id.to_string();
    for claim in claims.claims() {
        connection.execute(
            &format!(
                "INSERT INTO {TABLE_NAME}
                 (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5)"
            ),
            params![
                claim.claim_id().to_string(),
                &operation_text,
                claim.volume().device.to_string(),
                allocation_class_text(claim.allocation_class()),
                i64::try_from(claim.allocated_bytes()).expect("validated SQLite INTEGER range"),
            ],
        )?;
    }
    Ok(())
}

pub(super) fn verify_rows_inside_transaction(
    connection: &Connection,
    operation_id: OperationId,
    claims: &crate::operation_journal::admission_claims::AdmissionClaimInventory,
) -> Result<(), OrdinaryClaimWriteError> {
    let mut expected = expected_records(operation_id, claims);
    let mut actual = read_operation_rows(connection, operation_id)?;
    expected.sort_by_key(OrdinaryClaimRecord::claim_id);
    actual.sort_by_key(OrdinaryClaimRecord::claim_id);
    if actual != expected {
        return Err(OrdinaryClaimWriteError::ReadbackMismatch { operation_id });
    }
    Ok(())
}

pub(super) fn read_claim(
    connection: &Connection,
    claim_id: AdmissionClaimId,
) -> Result<Option<OrdinaryClaimRecord>, OrdinaryClaimReadError> {
    let claim_text = claim_id.to_string();
    let mut statement = connection.prepare(&format!(
        "SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes
         FROM {TABLE_NAME}
         WHERE claim_id = ?1"
    ))?;
    let mut rows = statement.query(params![claim_text])?;
    rows.next()?.map(decode_row).transpose()
}

pub(super) fn read_release(
    connection: &Connection,
    claim_id: AdmissionClaimId,
) -> Result<Option<OrdinaryClaimReleaseRecord>, OrdinaryClaimReadError> {
    let claim_text = claim_id.to_string();
    let mut statement = connection.prepare(&format!(
        "SELECT claim_id, operation_id, release_id
         FROM {RELEASE_TABLE_NAME}
         WHERE claim_id = ?1"
    ))?;
    let mut rows = statement.query(params![claim_text])?;
    rows.next()?.map(decode_release_row).transpose()
}

pub(super) fn insert_release_inside_transaction(
    connection: &Connection,
    release: OrdinaryClaimReleaseRecord,
) -> Result<(), OrdinaryClaimWriteError> {
    connection.execute(
        &format!(
            "INSERT INTO {RELEASE_TABLE_NAME} (claim_id, operation_id, release_id)
             VALUES (?1, ?2, ?3)"
        ),
        params![
            release.claim_id().to_string(),
            release.operation_id().to_string(),
            release.release_id().to_string(),
        ],
    )?;
    Ok(())
}

pub(super) fn verify_release_inside_transaction(
    connection: &Connection,
    release: OrdinaryClaimReleaseRecord,
) -> Result<(), OrdinaryClaimWriteError> {
    if read_release(connection, release.claim_id())? != Some(release) {
        return Err(OrdinaryClaimWriteError::ReleaseReadbackMismatch {
            claim_id: release.claim_id(),
        });
    }
    Ok(())
}

fn expected_records(
    operation_id: OperationId,
    claims: &crate::operation_journal::admission_claims::AdmissionClaimInventory,
) -> Vec<OrdinaryClaimRecord> {
    claims
        .claims()
        .iter()
        .map(|claim| OrdinaryClaimRecord {
            claim_id: claim.claim_id(),
            operation_id,
            volume: claim.volume().clone(),
            allocation_class: claim.allocation_class(),
            allocated_bytes: claim.allocated_bytes(),
        })
        .collect()
}

fn read_operation_rows(
    connection: &Connection,
    operation_id: OperationId,
) -> Result<Vec<OrdinaryClaimRecord>, OrdinaryClaimReadError> {
    let mut statement = connection.prepare(&format!(
        "SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes
         FROM {TABLE_NAME}
         WHERE operation_id = ?1
         ORDER BY claim_id ASC
         LIMIT ?2"
    ))?;
    let fetch_limit = i64::try_from(MAX_PAGE_ROWS + 1).expect("bounded ordinary claim limit");
    let mut rows = statement.query(params![operation_id.to_string(), fetch_limit])?;
    let mut records = Vec::with_capacity(MAX_PAGE_ROWS);
    while let Some(row) = rows.next()? {
        if records.len() == MAX_PAGE_ROWS {
            return Err(malformed(
                None,
                format!("operation {operation_id} has more than {MAX_PAGE_ROWS} claim rows"),
            ));
        }
        records.push(decode_row(row)?);
    }
    Ok(records)
}

fn allocation_class_text(class: FullCapacityAllocationClass) -> &'static str {
    match class {
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

pub(crate) fn read_page(
    connection: &Connection,
    cursor: Option<OrdinaryClaimCursor>,
    limit: usize,
) -> Result<OrdinaryClaimPage, OrdinaryClaimReadError> {
    if limit > MAX_PAGE_ROWS {
        return Err(OrdinaryClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        });
    }

    let Some(fetch_limit) = limit.checked_add(1) else {
        return Err(OrdinaryClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        });
    };
    if limit == 0 {
        if let Some(cursor) = cursor {
            validate_cursor(cursor)?;
        }
        return Ok(OrdinaryClaimPage {
            records: Vec::new(),
            next_cursor: None,
        });
    }

    let fetch_limit = i64::try_from(fetch_limit).expect("bounded ordinary-claim page limit");
    let mut statement = if cursor.is_some() {
        connection.prepare(&format!(
            "SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes
             FROM {TABLE_NAME}
             WHERE claim_id > ?1
             ORDER BY claim_id ASC
             LIMIT ?2"
        ))?
    } else {
        connection.prepare(&format!(
            "SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes
             FROM {TABLE_NAME}
             ORDER BY claim_id ASC
             LIMIT ?1"
        ))?
    };

    let mut rows = match cursor {
        Some(cursor) => {
            validate_cursor(cursor)?;
            statement.query(params![cursor.as_text(), fetch_limit])?
        }
        None => statement.query(params![fetch_limit])?,
    };
    let mut records = Vec::with_capacity(limit.min(MAX_PAGE_ROWS));
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        if records.len() == limit {
            has_more = true;
            break;
        }
        records.push(decode_row(row)?);
    }
    let next_cursor = has_more
        .then(|| {
            records
                .last()
                .map(|record| OrdinaryClaimCursor::from_claim_id(record.claim_id))
        })
        .flatten();
    Ok(OrdinaryClaimPage {
        records,
        next_cursor,
    })
}

pub(crate) fn read_release_page(
    connection: &Connection,
    cursor: Option<OrdinaryClaimCursor>,
    limit: usize,
) -> Result<OrdinaryClaimReleasePage, OrdinaryClaimReadError> {
    if limit > MAX_PAGE_ROWS {
        return Err(OrdinaryClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        });
    }
    let Some(fetch_limit) = limit.checked_add(1) else {
        return Err(OrdinaryClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        });
    };
    if limit == 0 {
        if let Some(cursor) = cursor {
            validate_cursor(cursor)?;
        }
        return Ok(OrdinaryClaimReleasePage {
            records: Vec::new(),
            next_cursor: None,
        });
    }

    let fetch_limit = i64::try_from(fetch_limit).expect("bounded ordinary-claim page limit");
    let mut statement = if cursor.is_some() {
        connection.prepare(&format!(
            "SELECT claim_id, operation_id, release_id
             FROM {RELEASE_TABLE_NAME}
             WHERE claim_id > ?1
             ORDER BY claim_id ASC
             LIMIT ?2"
        ))?
    } else {
        connection.prepare(&format!(
            "SELECT claim_id, operation_id, release_id
             FROM {RELEASE_TABLE_NAME}
             ORDER BY claim_id ASC
             LIMIT ?1"
        ))?
    };
    let mut rows = match cursor {
        Some(cursor) => {
            validate_cursor(cursor)?;
            statement.query(params![cursor.as_text(), fetch_limit])?
        }
        None => statement.query(params![fetch_limit])?,
    };
    let mut records = Vec::with_capacity(limit.min(MAX_PAGE_ROWS));
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        if records.len() == limit {
            has_more = true;
            break;
        }
        records.push(decode_release_row(row)?);
    }
    let next_cursor = has_more
        .then(|| {
            records
                .last()
                .map(|record| OrdinaryClaimCursor::from_claim_id(record.claim_id()))
        })
        .flatten();
    Ok(OrdinaryClaimReleasePage {
        records,
        next_cursor,
    })
}

fn validate_cursor(cursor: OrdinaryClaimCursor) -> Result<(), OrdinaryClaimReadError> {
    cursor
        .claim_id
        .validate()
        .map_err(|error| malformed(None, error.to_string()))
}

fn decode_row(row: &Row<'_>) -> Result<OrdinaryClaimRecord, OrdinaryClaimReadError> {
    let claim_text = text_value(row, 0, "claim_id", None, MAX_UUID_TEXT_BYTES)?;
    let claim_uuid = parse_uuid(&claim_text, "claim_id", false)?;
    let claim_id = AdmissionClaimId::from_uuid(claim_uuid)
        .map_err(|error| malformed(Some(&claim_text), error.to_string()))?;

    let operation_text = text_value(
        row,
        1,
        "operation_id",
        Some(&claim_text),
        MAX_UUID_TEXT_BYTES,
    )?;
    let operation_uuid = parse_uuid(&operation_text, "operation_id", false)?;
    let operation_id = OperationId::from_uuid(operation_uuid);

    let volume_text = text_value(
        row,
        2,
        "volume_device",
        Some(&claim_text),
        MAX_VOLUME_DEVICE_TEXT_BYTES,
    )?;
    let volume_device = parse_device(&volume_text, &claim_text)?;
    let allocation_class_text = text_value(
        row,
        3,
        "allocation_class",
        Some(&claim_text),
        MAX_ALLOCATION_CLASS_TEXT_BYTES,
    )?;
    let allocation_class = parse_allocation_class(&allocation_class_text, &claim_text)?;
    let allocated_bytes = positive_integer_value(row, 4, "allocated_bytes", &claim_text)?;

    Ok(OrdinaryClaimRecord {
        claim_id,
        operation_id,
        volume: VolumeIdentity {
            device: volume_device,
        },
        allocation_class,
        allocated_bytes,
    })
}

fn decode_release_row(row: &Row<'_>) -> Result<OrdinaryClaimReleaseRecord, OrdinaryClaimReadError> {
    let claim_text = text_value(row, 0, "claim_id", None, MAX_UUID_TEXT_BYTES)?;
    let claim_uuid = parse_uuid(&claim_text, "claim_id", false)?;
    let claim_id = AdmissionClaimId::from_uuid(claim_uuid)
        .map_err(|error| malformed(Some(&claim_text), error.to_string()))?;
    let operation_text = text_value(
        row,
        1,
        "operation_id",
        Some(&claim_text),
        MAX_UUID_TEXT_BYTES,
    )?;
    let operation_id = OperationId::from_uuid(parse_uuid(&operation_text, "operation_id", false)?);
    let release_text = text_value(row, 2, "release_id", Some(&claim_text), MAX_UUID_TEXT_BYTES)?;
    let release_id = parse_uuid(&release_text, "release_id", false)?;
    Ok(OrdinaryClaimReleaseRecord {
        claim_id,
        operation_id,
        release_id,
    })
}

fn text_value(
    row: &Row<'_>,
    index: usize,
    field: &str,
    claim: Option<&str>,
    maximum_bytes: usize,
) -> Result<String, OrdinaryClaimReadError> {
    match row.get_ref(index)? {
        ValueRef::Text(value) if value.len() > maximum_bytes => Err(malformed(
            claim,
            format!("{field} exceeds {maximum_bytes} bytes"),
        )),
        ValueRef::Text(value) => String::from_utf8(value.to_vec())
            .map_err(|_| malformed(claim, format!("{field} is not valid UTF-8 text"))),
        _ => Err(malformed(claim, format!("{field} is not TEXT"))),
    }
}

fn parse_uuid(value: &str, field: &str, allow_nil: bool) -> Result<Uuid, OrdinaryClaimReadError> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| malformed(Some(value), format!("{field} is not UUID text")))?;
    if parsed.to_string() != value {
        return Err(malformed(
            Some(value),
            format!("{field} is not canonical UUID text"),
        ));
    }
    if !allow_nil && parsed.is_nil() {
        return Err(malformed(Some(value), format!("{field} is nil")));
    }
    Ok(parsed)
}

fn parse_device(value: &str, claim: &str) -> Result<u64, OrdinaryClaimReadError> {
    let parsed = value.parse::<u64>().map_err(|_| {
        malformed(
            Some(claim),
            "volume_device is not canonical u64 text".to_owned(),
        )
    })?;
    if parsed.to_string() != value {
        return Err(malformed(
            Some(claim),
            "volume_device is not canonical u64 text".to_owned(),
        ));
    }
    Ok(parsed)
}

fn parse_allocation_class(
    value: &str,
    claim: &str,
) -> Result<FullCapacityAllocationClass, OrdinaryClaimReadError> {
    let class = match value {
        "DestinationStaging" => FullCapacityAllocationClass::DestinationStaging,
        "JournalRecord" => FullCapacityAllocationClass::JournalRecord,
        "SourceDatabaseCommit" => FullCapacityAllocationClass::SourceDatabaseCommit,
        "SourceWalShm" => FullCapacityAllocationClass::SourceWalShm,
        "GlobalDatabaseCommit" => FullCapacityAllocationClass::GlobalDatabaseCommit,
        "GlobalWalShm" => FullCapacityAllocationClass::GlobalWalShm,
        "ExistingDestination" => FullCapacityAllocationClass::ExistingDestination,
        "ExistingRecoveryPayload" => FullCapacityAllocationClass::ExistingRecoveryPayload,
        "OperationRecoveryPayload" => FullCapacityAllocationClass::OperationRecoveryPayload,
        _ => {
            return Err(malformed(
                Some(claim),
                format!("allocation_class {value:?} is unsupported"),
            ));
        }
    };
    Ok(class)
}

fn positive_integer_value(
    row: &Row<'_>,
    index: usize,
    field: &str,
    claim: &str,
) -> Result<u64, OrdinaryClaimReadError> {
    match row.get_ref(index)? {
        ValueRef::Integer(value) if value > 0 => u64::try_from(value)
            .map_err(|_| malformed(Some(claim), format!("{field} is outside the u64 range"))),
        ValueRef::Integer(_) => Err(malformed(Some(claim), format!("{field} must be positive"))),
        _ => Err(malformed(Some(claim), format!("{field} is not an INTEGER"))),
    }
}

fn malformed(claim: Option<&str>, reason: impl Into<String>) -> OrdinaryClaimReadError {
    OrdinaryClaimReadError::MalformedRow {
        claim: claim
            .map(|value| format!(" for {value}"))
            .unwrap_or_default(),
        reason: reason.into(),
    }
}
