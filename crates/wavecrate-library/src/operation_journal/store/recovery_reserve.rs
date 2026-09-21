//! Private durable recovery-reserve ledger observations.
//!
//! This module only decodes and reports persisted rows. It does not expose charge, spend, release,
//! reconstitution, lock, admission, or physical-reserve authority.

use rusqlite::OptionalExtension;
use rusqlite::{Connection, params};
use thiserror::Error;
use uuid::Uuid;

use crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR;
use crate::durable_capacity::VolumeIdentity;
use crate::operation_journal::capacity_reserve_namespace::PhysicalReserveProof;
use crate::operation_journal::model::OperationId;

pub(super) const CHARGE_TABLE_NAME: &str = "operation_journal_recovery_reserve";
pub(super) const TRANSITION_TABLE_NAME: &str = "operation_journal_recovery_reserve_transition";

pub(super) const CREATE_CHARGE_TABLE: &str = "CREATE TABLE operation_journal_recovery_reserve (
    charge_id TEXT NOT NULL PRIMARY KEY,
    volume_device TEXT NOT NULL,
    active_volume_device TEXT UNIQUE,
    operation_id TEXT NOT NULL,
    purpose TEXT NOT NULL,
    budget_bytes INTEGER NOT NULL,
    margin_bytes INTEGER NOT NULL,
    precharge_capacity_bytes INTEGER NOT NULL,
    remaining_bytes INTEGER NOT NULL,
    state TEXT NOT NULL,
    transition_sequence INTEGER NOT NULL,
    CHECK (state IN ('Charged', 'Spent', 'ReleaseObserved', 'Reconstituted')),
    CHECK ((state = 'Reconstituted' AND active_volume_device IS NULL)
        OR (state <> 'Reconstituted' AND active_volume_device = volume_device)),
    CHECK (budget_bytes > 0 AND margin_bytes >= 0
        AND budget_bytes + margin_bytes <= 268435456
        AND remaining_bytes >= 0 AND remaining_bytes <= budget_bytes
        AND transition_sequence > 0),
    FOREIGN KEY (operation_id) REFERENCES operation_journal(operation_id)
)";

pub(super) const CREATE_TRANSITION_TABLE: &str =
    "CREATE TABLE operation_journal_recovery_reserve_transition (
    charge_id TEXT NOT NULL,
    transition_id TEXT NOT NULL,
    transition_sequence INTEGER NOT NULL,
    kind TEXT NOT NULL,
    amount_bytes INTEGER NOT NULL,
    remaining_bytes INTEGER NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY (charge_id, transition_id),
    UNIQUE (charge_id, transition_sequence),
    CHECK (kind IN ('Charged', 'Spent', 'ReleaseObserved', 'Reconstituted')),
    CHECK (amount_bytes >= 0 AND remaining_bytes >= 0 AND transition_sequence > 0),
    FOREIGN KEY (charge_id) REFERENCES operation_journal_recovery_reserve(charge_id)
)";

const CHARGE_KINDS: [&str; 4] = ["Charged", "Spent", "ReleaseObserved", "Reconstituted"];
const PURPOSES: [&str; 3] = ["OperationRecovery", "JournalRecovery", "OwnershipRelease"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryReservePurpose {
    OperationRecovery,
    JournalRecovery,
    OwnershipRelease,
}

impl RecoveryReservePurpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::OperationRecovery => "OperationRecovery",
            Self::JournalRecovery => "JournalRecovery",
            Self::OwnershipRelease => "OwnershipRelease",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryReserveState {
    Charged,
    Spent,
    ReleaseObserved,
    Reconstituted,
}

impl RecoveryReserveState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Charged => "Charged",
            Self::Spent => "Spent",
            Self::ReleaseObserved => "ReleaseObserved",
            Self::Reconstituted => "Reconstituted",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChargeRequest {
    pub(super) charge_id: Uuid,
    pub(super) operation_id: OperationId,
    pub(super) volume: VolumeIdentity,
    pub(super) purpose: RecoveryReservePurpose,
    pub(super) budget_bytes: u64,
    pub(super) margin_bytes: u64,
    pub(super) precharge_capacity_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChargeSnapshot {
    pub(super) charge_id: Uuid,
    pub(super) operation_id: OperationId,
    pub(super) volume: VolumeIdentity,
    pub(super) purpose: RecoveryReservePurpose,
    pub(super) budget_bytes: u64,
    pub(super) margin_bytes: u64,
    pub(super) precharge_capacity_bytes: u64,
    pub(super) remaining_bytes: u64,
    pub(super) state: RecoveryReserveState,
    pub(super) transition_sequence: i64,
}

#[derive(Debug, Error, PartialEq)]
pub(crate) enum LedgerError {
    #[error("recovery-reserve SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("recovery-reserve charge is invalid: {0}")]
    Invalid(String),
    #[error("recovery-reserve record is corrupt: {0}")]
    Corrupt(String),
    #[error("durable operation {0} was not found")]
    OperationNotFound(OperationId),
    #[error("charge {0} conflicts with an existing durable charge")]
    ChargeConflict(Uuid),
    #[error("volume {0:?} already has an active charge")]
    ActiveVolumeConflict(VolumeIdentity),
    #[error("charge {charge_id} is bound to volume {actual:?}, expected held volume {expected:?}")]
    VolumeConflict {
        charge_id: Uuid,
        expected: VolumeIdentity,
        actual: VolumeIdentity,
    },
    #[error("charge {charge_id} has insufficient remaining budget for {requested} bytes")]
    BudgetExceeded { charge_id: Uuid, requested: u64 },
    #[error("transition {charge_id}/{transition_id} conflicts with its durable retry")]
    TransitionConflict {
        charge_id: Uuid,
        transition_id: Uuid,
    },
    #[error("charge {0} is already reconstituted")]
    AlreadyReconstituted(Uuid),
    #[error("charge {0} is not the active charge for its volume")]
    ChargeNotActive(Uuid),
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) struct TestReconstitutionProof {
    pub(super) volume: VolumeIdentity,
    pub(super) physical_bytes: u64,
    pub(super) non_sparse: bool,
}

fn corrupt(reason: impl Into<String>) -> String {
    format!("recovery-reserve ledger requires audit: {}", reason.into())
}

fn parse_uuid(value: String, field: &str) -> Result<Uuid, String> {
    let parsed =
        Uuid::parse_str(&value).map_err(|_| corrupt(format!("{field} is not UUID text")))?;
    if parsed.to_string() != value {
        return Err(corrupt(format!("{field} is not canonical UUID text")));
    }
    Ok(parsed)
}

fn parse_device(value: String) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| corrupt("volume identity is not canonical u64 text"))?;
    if parsed.to_string() != value {
        return Err(corrupt("volume identity is not canonical u64 text"));
    }
    Ok(parsed)
}

fn purpose_from_str(value: &str) -> Result<RecoveryReservePurpose, LedgerError> {
    match value {
        "OperationRecovery" => Ok(RecoveryReservePurpose::OperationRecovery),
        "JournalRecovery" => Ok(RecoveryReservePurpose::JournalRecovery),
        "OwnershipRelease" => Ok(RecoveryReservePurpose::OwnershipRelease),
        _ => Err(LedgerError::Corrupt(format!(
            "unsupported recovery-reserve purpose {value:?}"
        ))),
    }
}

fn state_from_str(value: &str) -> Result<RecoveryReserveState, LedgerError> {
    match value {
        "Charged" => Ok(RecoveryReserveState::Charged),
        "Spent" => Ok(RecoveryReserveState::Spent),
        "ReleaseObserved" => Ok(RecoveryReserveState::ReleaseObserved),
        "Reconstituted" => Ok(RecoveryReserveState::Reconstituted),
        _ => Err(LedgerError::Corrupt(format!(
            "unsupported recovery-reserve state {value:?}"
        ))),
    }
}

fn to_sql_integer(value: u64, field: &str) -> Result<i64, LedgerError> {
    i64::try_from(value)
        .map_err(|_| LedgerError::Invalid(format!("{field} exceeds SQLite INTEGER range")))
}

fn validate_request(request: &ChargeRequest) -> Result<(), LedgerError> {
    if request.budget_bytes == 0 {
        return Err(LedgerError::Invalid("budget must be positive".into()));
    }
    if request.margin_bytes > PROTECTED_FREE_SPACE_FLOOR {
        return Err(LedgerError::Invalid(
            "margin exceeds protected floor".into(),
        ));
    }
    let charge_with_margin = request
        .budget_bytes
        .checked_add(request.margin_bytes)
        .ok_or_else(|| LedgerError::Invalid("budget plus margin overflowed".into()))?;
    if charge_with_margin > PROTECTED_FREE_SPACE_FLOOR {
        return Err(LedgerError::Invalid(
            "budget plus margin exceeds protected floor".into(),
        ));
    }
    for (value, field) in [
        (request.budget_bytes, "budget_bytes"),
        (request.margin_bytes, "margin_bytes"),
        (request.precharge_capacity_bytes, "precharge_capacity_bytes"),
    ] {
        let _ = to_sql_integer(value, field)?;
    }
    Ok(())
}

fn operation_exists(
    connection: &Connection,
    operation_id: OperationId,
) -> Result<bool, LedgerError> {
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM operation_journal WHERE operation_id = ?1
            UNION ALL
            SELECT 1 FROM operation_journal_full_progress WHERE operation_id = ?1
        )",
        params![operation_id.to_string()],
        |row| row.get(0),
    )?)
}

fn validate_charge_row(
    charge_id: String,
    volume_device: String,
    active_volume_device: Option<String>,
    operation_id: String,
    purpose: String,
    budget_bytes: i64,
    margin_bytes: i64,
    precharge_capacity_bytes: i64,
    remaining_bytes: i64,
    state: String,
    transition_sequence: i64,
) -> Result<
    (
        Uuid,
        u64,
        Option<u64>,
        String,
        u64,
        u64,
        u64,
        u64,
        String,
        i64,
    ),
    String,
> {
    let charge_id = parse_uuid(charge_id, "charge_id")?;
    let volume_device = parse_device(volume_device)?;
    let active_volume_device = active_volume_device.map(parse_device).transpose()?;
    let _ = parse_uuid(operation_id.clone(), "operation_id")?;
    if !PURPOSES.contains(&purpose.as_str()) {
        return Err(corrupt(format!("unsupported purpose {purpose:?}")));
    }
    if !CHARGE_KINDS.contains(&state.as_str()) {
        return Err(corrupt(format!("unsupported state {state:?}")));
    }
    let budget_bytes =
        u64::try_from(budget_bytes).map_err(|_| corrupt("budget_bytes is negative"))?;
    let margin_bytes =
        u64::try_from(margin_bytes).map_err(|_| corrupt("margin_bytes is negative"))?;
    let precharge_capacity_bytes = u64::try_from(precharge_capacity_bytes)
        .map_err(|_| corrupt("precharge_capacity_bytes is negative"))?;
    let remaining_bytes =
        u64::try_from(remaining_bytes).map_err(|_| corrupt("remaining_bytes is negative"))?;
    if budget_bytes == 0 {
        return Err(corrupt("budget_bytes is zero"));
    }
    let charge_with_margin = budget_bytes
        .checked_add(margin_bytes)
        .ok_or_else(|| corrupt("budget_bytes plus margin_bytes overflowed"))?;
    if margin_bytes > PROTECTED_FREE_SPACE_FLOOR
        || charge_with_margin > PROTECTED_FREE_SPACE_FLOOR
        || remaining_bytes > budget_bytes
        || transition_sequence <= 0
    {
        return Err(corrupt("charge arithmetic or sequence bounds are invalid"));
    }
    if state == "Reconstituted" {
        if active_volume_device.is_some() {
            return Err(corrupt("reconstituted charge retains an active volume key"));
        }
    } else if active_volume_device != Some(volume_device) {
        return Err(corrupt("active charge does not retain its volume key"));
    }
    Ok((
        charge_id,
        volume_device,
        active_volume_device,
        operation_id,
        budget_bytes,
        margin_bytes,
        precharge_capacity_bytes,
        remaining_bytes,
        state,
        transition_sequence,
    ))
}

fn validate_transitions(
    connection: &Connection,
    charge_id: Uuid,
    budget_bytes: u64,
    final_remaining_bytes: u64,
    final_state: &str,
    final_sequence: i64,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT transition_id, transition_sequence, kind, amount_bytes,
                    remaining_bytes, state
             FROM {TRANSITION_TABLE_NAME}
             WHERE charge_id = ?1 ORDER BY transition_sequence"
        ))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![charge_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut expected_sequence = 1_i64;
    let mut count = 0_u64;
    let mut previous_remaining = 0_u64;
    let mut previous_state: Option<String> = None;
    for row in rows {
        let (transition_id, sequence, kind, amount_bytes, remaining_bytes, state) =
            row.map_err(|error| error.to_string())?;
        let _ = parse_uuid(transition_id, "transition_id")?;
        if sequence != expected_sequence {
            return Err(corrupt("transition sequence is not contiguous"));
        }
        if !CHARGE_KINDS.contains(&kind.as_str()) || kind != state {
            return Err(corrupt(
                "transition kind and state are unsupported or disagree",
            ));
        }
        let amount_bytes =
            u64::try_from(amount_bytes).map_err(|_| corrupt("transition amount is negative"))?;
        let remaining_bytes = u64::try_from(remaining_bytes)
            .map_err(|_| corrupt("transition remaining budget is negative"))?;
        if remaining_bytes > budget_bytes {
            return Err(corrupt("transition remaining budget exceeds charge budget"));
        }
        if sequence == 1 {
            if kind != "Charged" || amount_bytes != budget_bytes || remaining_bytes != budget_bytes
            {
                return Err(corrupt("first transition is not the complete charge"));
            }
        } else {
            let prior_state = previous_state
                .as_deref()
                .ok_or_else(|| corrupt("transition history has no prior state"))?;
            match kind.as_str() {
                "Spent" => {
                    if !matches!(prior_state, "Charged" | "Spent") || amount_bytes == 0 {
                        return Err(corrupt(
                            "spent transition follows an unavailable state or has zero amount",
                        ));
                    }
                    let expected_remaining = previous_remaining
                        .checked_sub(amount_bytes)
                        .ok_or_else(|| corrupt("spent transition exceeds remaining budget"))?;
                    if remaining_bytes != expected_remaining {
                        return Err(corrupt(
                            "spent transition does not subtract its amount exactly",
                        ));
                    }
                }
                "ReleaseObserved" => {
                    if !matches!(prior_state, "Charged" | "Spent") || amount_bytes == 0 {
                        return Err(corrupt(
                            "release observation follows an unavailable state or has zero amount",
                        ));
                    }
                    let spent_bytes = budget_bytes
                        .checked_sub(previous_remaining)
                        .ok_or_else(|| corrupt("release observation has invalid prior budget"))?;
                    if amount_bytes > spent_bytes || remaining_bytes != previous_remaining {
                        return Err(corrupt(
                            "release observation exceeds unreleased spend or increases budget",
                        ));
                    }
                }
                "Reconstituted" => {
                    if !matches!(prior_state, "Charged" | "Spent" | "ReleaseObserved")
                        || amount_bytes != 0
                        || remaining_bytes != 0
                    {
                        return Err(corrupt(
                            "reconstitution follows an unavailable state or restores budget",
                        ));
                    }
                }
                "Charged" => {
                    return Err(corrupt("charge transition appears after initial charge"));
                }
                _ => unreachable!("kind was checked above"),
            }
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| corrupt("transition sequence overflowed"))?;
        count += 1;
        previous_remaining = remaining_bytes;
        previous_state = Some(kind);
        if sequence == final_sequence
            && (remaining_bytes != final_remaining_bytes || state != final_state)
        {
            return Err(corrupt("charge row disagrees with its final transition"));
        }
    }
    if count == 0 || final_sequence != expected_sequence - 1 {
        return Err(corrupt("charge has incomplete transition history"));
    }
    Ok(())
}

fn read_charge(
    connection: &Connection,
    charge_id: Uuid,
) -> Result<Option<ChargeSnapshot>, LedgerError> {
    let raw = connection
        .query_row(
            &format!(
                "SELECT charge_id, volume_device, active_volume_device, operation_id, purpose,
                        budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                        state, transition_sequence
                 FROM {CHARGE_TABLE_NAME} WHERE charge_id = ?1"
            ),
            params![charge_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = raw else {
        return Ok(None);
    };
    let purpose_text = row.4.clone();
    let (
        stored_id,
        volume_device,
        _active_volume_device,
        operation_id,
        budget_bytes,
        margin_bytes,
        precharge_capacity_bytes,
        remaining_bytes,
        state,
        transition_sequence,
    ) = validate_charge_row(
        row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
    )
    .map_err(LedgerError::Corrupt)?;
    if stored_id != charge_id {
        return Err(LedgerError::Corrupt(
            "charge lookup identity mismatch".into(),
        ));
    }
    let operation_id = OperationId::parse(&operation_id)
        .map_err(|_| LedgerError::Corrupt("operation id is not UUID text".into()))?;
    if !operation_exists(connection, operation_id)? {
        return Err(LedgerError::OperationNotFound(operation_id));
    }
    let state_enum = state_from_str(&state)?;
    let purpose_enum = purpose_from_str(&purpose_text)?;
    validate_transitions(
        connection,
        charge_id,
        budget_bytes,
        remaining_bytes,
        &state,
        transition_sequence,
    )
    .map_err(LedgerError::Corrupt)?;
    Ok(Some(ChargeSnapshot {
        charge_id,
        operation_id,
        volume: VolumeIdentity {
            device: volume_device,
        },
        purpose: purpose_enum,
        budget_bytes,
        margin_bytes,
        precharge_capacity_bytes,
        remaining_bytes,
        state: state_enum,
        transition_sequence,
    }))
}

fn read_transition(
    connection: &Connection,
    charge_id: Uuid,
    transition_id: Uuid,
) -> Result<Option<(i64, String, u64, u64, RecoveryReserveState)>, LedgerError> {
    let row = connection
        .query_row(
            &format!(
                "SELECT transition_sequence, kind, amount_bytes, remaining_bytes, state
                 FROM {TRANSITION_TABLE_NAME}
                 WHERE charge_id = ?1 AND transition_id = ?2"
            ),
            params![charge_id.to_string(), transition_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((sequence, kind, amount, remaining, state)) = row else {
        return Ok(None);
    };
    if sequence <= 0 {
        return Err(LedgerError::Corrupt(
            "transition sequence must be positive".into(),
        ));
    }
    let amount = u64::try_from(amount)
        .map_err(|_| LedgerError::Corrupt("transition amount is negative".into()))?;
    let remaining = u64::try_from(remaining)
        .map_err(|_| LedgerError::Corrupt("transition remaining budget is negative".into()))?;
    let state = state_from_str(&state)?;
    if kind != state.as_str() {
        return Err(LedgerError::Corrupt(
            "transition kind and state disagree".into(),
        ));
    }
    Ok(Some((sequence, kind, amount, remaining, state)))
}

fn historical_snapshot(
    mut snapshot: ChargeSnapshot,
    sequence: i64,
    remaining: u64,
    state: RecoveryReserveState,
) -> ChargeSnapshot {
    snapshot.transition_sequence = sequence;
    snapshot.remaining_bytes = remaining;
    snapshot.state = state;
    snapshot
}

pub(super) struct SpendOutcome {
    pub(super) snapshot: ChargeSnapshot,
    pub(super) head: ChargeSnapshot,
    pub(super) applied: bool,
}

pub(super) enum ReconstitutePreflight {
    Historical(ChargeSnapshot),
    Pending,
}

fn next_sequence(sequence: i64) -> Result<i64, LedgerError> {
    sequence
        .checked_add(1)
        .ok_or_else(|| LedgerError::Invalid("transition sequence exhausted".into()))
}

fn begin_transaction(connection: &Connection) -> Result<(), LedgerError> {
    connection.execute_batch("BEGIN IMMEDIATE")?;
    Ok(())
}

fn finish_transaction<T>(
    connection: &Connection,
    result: Result<T, LedgerError>,
) -> Result<T, LedgerError> {
    match result {
        Ok(value) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(LedgerError::Sql(error));
            }
            Ok(value)
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

/// Read all persisted charges. Any malformed value fails closed for startup.
pub(super) fn readiness(connection: &Connection) -> Result<Option<String>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT charge_id, volume_device, active_volume_device, operation_id, purpose,
                    budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                    state, transition_sequence
             FROM {CHARGE_TABLE_NAME} ORDER BY charge_id"
        ))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let row = row.map_err(|error| error.to_string())?;
        let (
            charge_id,
            volume_device,
            _active_volume_device,
            operation_id,
            budget_bytes,
            _margin_bytes,
            _precharge_capacity_bytes,
            remaining_bytes,
            state,
            transition_sequence,
        ) = validate_charge_row(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
        )?;
        let operation_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM operation_journal WHERE operation_id = ?1
                    UNION ALL
                    SELECT 1 FROM operation_journal_full_progress WHERE operation_id = ?1
                )",
                params![operation_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !operation_exists {
            return Err(corrupt("charge references a missing durable operation"));
        }
        validate_transitions(
            connection,
            charge_id,
            budget_bytes,
            remaining_bytes,
            &state,
            transition_sequence,
        )?;
        if state != "Reconstituted" {
            return Ok(Some(format!(
                "recovery-reserve charge {charge_id} on volume {volume_device} is {state} and requires physical reconstitution"
            )));
        }
        return Ok(Some(format!(
            "recovery-reserve charge {charge_id} on volume {volume_device} is reconstituted but lacks live physical revalidation"
        )));
    }
    Ok(None)
}

fn active_charge_for_volume(
    connection: &Connection,
    volume: &VolumeIdentity,
) -> Result<Option<Uuid>, LedgerError> {
    let charge_id = connection
        .query_row(
            &format!(
                "SELECT charge_id FROM {CHARGE_TABLE_NAME}
                 WHERE active_volume_device = ?1"
            ),
            params![volume.device.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    charge_id
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|_| LedgerError::Corrupt("active charge id is not UUID text".into()))
        })
        .transpose()
}

pub(super) fn active_charge_snapshot_for_volume(
    connection: &Connection,
    volume: &VolumeIdentity,
) -> Result<Option<ChargeSnapshot>, LedgerError> {
    let Some(charge_id) = active_charge_for_volume(connection, volume)? else {
        return Ok(None);
    };
    let snapshot = read_charge(connection, charge_id)?
        .ok_or_else(|| LedgerError::Corrupt("active charge row is missing".into()))?;
    if snapshot.volume != *volume {
        return Err(LedgerError::VolumeConflict {
            charge_id,
            expected: volume.clone(),
            actual: snapshot.volume,
        });
    }
    Ok(Some(snapshot))
}

pub(super) fn begin_charge(
    connection: &Connection,
    request: &ChargeRequest,
) -> Result<ChargeSnapshot, LedgerError> {
    validate_request(request)?;
    begin_transaction(connection)?;
    let result = (|| {
        if !operation_exists(connection, request.operation_id)? {
            return Err(LedgerError::OperationNotFound(request.operation_id));
        }
        if let Some(existing) = read_charge(connection, request.charge_id)? {
            if existing.operation_id == request.operation_id
                && existing.volume == request.volume
                && existing.purpose == request.purpose
                && existing.budget_bytes == request.budget_bytes
                && existing.margin_bytes == request.margin_bytes
            {
                return Ok(existing);
            }
            return Err(LedgerError::ChargeConflict(request.charge_id));
        }
        if active_charge_for_volume(connection, &request.volume)?.is_some() {
            return Err(LedgerError::ActiveVolumeConflict(request.volume.clone()));
        }
        connection
            .execute(
                &format!(
                    "INSERT INTO {CHARGE_TABLE_NAME}
                 (charge_id, volume_device, active_volume_device, operation_id, purpose,
                  budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                  state, transition_sequence)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?5, 'Charged', 1)"
                ),
                params![
                    request.charge_id.to_string(),
                    request.volume.device.to_string(),
                    request.operation_id.to_string(),
                    request.purpose.as_str(),
                    to_sql_integer(request.budget_bytes, "budget_bytes")?,
                    to_sql_integer(request.margin_bytes, "margin_bytes")?,
                    to_sql_integer(request.precharge_capacity_bytes, "precharge_capacity_bytes")?,
                ],
            )
            .map_err(|error| {
                if let rusqlite::Error::SqliteFailure(_, message) = &error {
                    if message
                        .as_deref()
                        .is_some_and(|message| message.contains("UNIQUE"))
                    {
                        return LedgerError::ActiveVolumeConflict(request.volume.clone());
                    }
                }
                LedgerError::Sql(error)
            })?;
        connection.execute(
            &format!(
                "INSERT INTO {TRANSITION_TABLE_NAME}
                 (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                  remaining_bytes, state)
                 VALUES (?1, ?1, 1, 'Charged', ?2, ?2, 'Charged')"
            ),
            params![
                request.charge_id.to_string(),
                to_sql_integer(request.budget_bytes, "budget_bytes")?,
            ],
        )?;
        read_charge(connection, request.charge_id)?
            .ok_or_else(|| LedgerError::Corrupt("new charge disappeared before commit".into()))
    })();
    finish_transaction(connection, result)
}

pub(super) fn spend(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
    transition_id: Uuid,
    amount_bytes: u64,
) -> Result<ChargeSnapshot, LedgerError> {
    Ok(spend_with_outcome(
        connection,
        expected_volume,
        charge_id,
        transition_id,
        amount_bytes,
    )?
    .snapshot)
}

pub(super) fn spend_with_outcome(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
    transition_id: Uuid,
    amount_bytes: u64,
) -> Result<SpendOutcome, LedgerError> {
    if amount_bytes == 0 {
        return Err(LedgerError::Invalid("spend amount must be positive".into()));
    }
    let _ = to_sql_integer(amount_bytes, "spend amount_bytes")?;
    begin_transaction(connection)?;
    let result = (|| {
        let snapshot = read_charge(connection, charge_id)?
            .ok_or_else(|| LedgerError::Corrupt("spend references missing charge".into()))?;
        if snapshot.volume != *expected_volume {
            return Err(LedgerError::VolumeConflict {
                charge_id,
                expected: expected_volume.clone(),
                actual: snapshot.volume,
            });
        }
        if let Some((sequence, kind, amount, remaining, state)) =
            read_transition(connection, charge_id, transition_id)?
        {
            if kind == "Spent" && amount == amount_bytes {
                return Ok(SpendOutcome {
                    snapshot: historical_snapshot(snapshot.clone(), sequence, remaining, state),
                    head: snapshot,
                    applied: false,
                });
            }
            return Err(LedgerError::TransitionConflict {
                charge_id,
                transition_id,
            });
        }
        match snapshot.state {
            RecoveryReserveState::Charged | RecoveryReserveState::Spent => {}
            RecoveryReserveState::ReleaseObserved => {
                return Err(LedgerError::Invalid(
                    "released charge is unavailable until reconstituted".into(),
                ));
            }
            RecoveryReserveState::Reconstituted => {
                return Err(LedgerError::AlreadyReconstituted(charge_id));
            }
        }
        if amount_bytes > snapshot.remaining_bytes {
            return Err(LedgerError::BudgetExceeded {
                charge_id,
                requested: amount_bytes,
            });
        }
        let remaining_bytes = snapshot.remaining_bytes - amount_bytes;
        let transition_sequence = next_sequence(snapshot.transition_sequence)?;
        connection.execute(
            &format!(
                "UPDATE {CHARGE_TABLE_NAME}
                 SET remaining_bytes = ?1, state = 'Spent', transition_sequence = ?2
                 WHERE charge_id = ?3"
            ),
            params![
                to_sql_integer(remaining_bytes, "remaining_bytes")?,
                transition_sequence,
                charge_id.to_string(),
            ],
        )?;
        connection.execute(
            &format!(
                "INSERT INTO {TRANSITION_TABLE_NAME}
                 (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                  remaining_bytes, state)
                 VALUES (?1, ?2, ?3, 'Spent', ?4, ?5, 'Spent')"
            ),
            params![
                charge_id.to_string(),
                transition_id.to_string(),
                transition_sequence,
                to_sql_integer(amount_bytes, "spend amount_bytes")?,
                to_sql_integer(remaining_bytes, "remaining_bytes")?,
            ],
        )?;
        let head = read_charge(connection, charge_id)?.ok_or_else(|| {
            LedgerError::Corrupt("spent charge disappeared before commit".to_string())
        })?;
        Ok(SpendOutcome {
            snapshot: head.clone(),
            head,
            applied: true,
        })
    })();
    finish_transaction(connection, result)
}

pub(super) fn charge_snapshot_for_volume(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
) -> Result<ChargeSnapshot, LedgerError> {
    let snapshot = read_charge(connection, charge_id)?
        .ok_or_else(|| LedgerError::Corrupt("charge references missing charge".into()))?;
    if snapshot.volume != *expected_volume {
        return Err(LedgerError::VolumeConflict {
            charge_id,
            expected: expected_volume.clone(),
            actual: snapshot.volume,
        });
    }
    Ok(snapshot)
}

pub(super) fn require_active_charge_for_volume(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
) -> Result<ChargeSnapshot, LedgerError> {
    begin_transaction(connection)?;
    let result = (|| {
        let snapshot = charge_snapshot_for_volume(connection, expected_volume, charge_id)?;
        if active_charge_for_volume(connection, expected_volume)? != Some(charge_id) {
            return Err(LedgerError::ChargeNotActive(charge_id));
        }
        Ok(snapshot)
    })();
    finish_transaction(connection, result)
}

pub(super) fn reconstitute_preflight(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
    transition_id: Uuid,
) -> Result<ReconstitutePreflight, LedgerError> {
    begin_transaction(connection)?;
    let result = (|| {
        let snapshot = charge_snapshot_for_volume(connection, expected_volume, charge_id)?;
        if let Some((sequence, kind, amount, remaining, state)) =
            read_transition(connection, charge_id, transition_id)?
        {
            if kind == "Reconstituted" && amount == 0 && remaining == 0 {
                return Ok(ReconstitutePreflight::Historical(historical_snapshot(
                    snapshot, sequence, remaining, state,
                )));
            }
            return Err(LedgerError::TransitionConflict {
                charge_id,
                transition_id,
            });
        }
        if snapshot.state == RecoveryReserveState::Reconstituted {
            return Err(LedgerError::AlreadyReconstituted(charge_id));
        }
        if active_charge_for_volume(connection, expected_volume)? != Some(charge_id) {
            return Err(LedgerError::ChargeNotActive(charge_id));
        }
        Ok(ReconstitutePreflight::Pending)
    })();
    finish_transaction(connection, result)
}

pub(super) fn record_release_observation(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
    transition_id: Uuid,
    amount_bytes: u64,
) -> Result<ChargeSnapshot, LedgerError> {
    if amount_bytes == 0 {
        return Err(LedgerError::Invalid(
            "release observation amount must be positive".into(),
        ));
    }
    let _ = to_sql_integer(amount_bytes, "release amount_bytes")?;
    begin_transaction(connection)?;
    let result = (|| {
        let snapshot = read_charge(connection, charge_id)?
            .ok_or_else(|| LedgerError::Corrupt("release references missing charge".into()))?;
        if snapshot.volume != *expected_volume {
            return Err(LedgerError::VolumeConflict {
                charge_id,
                expected: expected_volume.clone(),
                actual: snapshot.volume,
            });
        }
        if let Some((sequence, kind, amount, remaining, state)) =
            read_transition(connection, charge_id, transition_id)?
        {
            if kind == "ReleaseObserved" && amount == amount_bytes {
                return Ok(historical_snapshot(snapshot, sequence, remaining, state));
            }
            return Err(LedgerError::TransitionConflict {
                charge_id,
                transition_id,
            });
        }
        if !matches!(
            snapshot.state,
            RecoveryReserveState::Charged | RecoveryReserveState::Spent
        ) {
            return Err(if snapshot.state == RecoveryReserveState::Reconstituted {
                LedgerError::AlreadyReconstituted(charge_id)
            } else {
                LedgerError::Invalid("release observation follows an unavailable state".into())
            });
        }
        let spent_bytes = snapshot
            .budget_bytes
            .checked_sub(snapshot.remaining_bytes)
            .ok_or_else(|| LedgerError::Corrupt("charge remaining budget is invalid".into()))?;
        if amount_bytes > spent_bytes {
            return Err(LedgerError::BudgetExceeded {
                charge_id,
                requested: amount_bytes,
            });
        }
        let transition_sequence = next_sequence(snapshot.transition_sequence)?;
        connection.execute(
            &format!(
                "UPDATE {CHARGE_TABLE_NAME}
                 SET state = 'ReleaseObserved', transition_sequence = ?1
                 WHERE charge_id = ?2"
            ),
            params![transition_sequence, charge_id.to_string()],
        )?;
        connection.execute(
            &format!(
                "INSERT INTO {TRANSITION_TABLE_NAME}
                 (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                  remaining_bytes, state)
                 VALUES (?1, ?2, ?3, 'ReleaseObserved', ?4, ?5, 'ReleaseObserved')"
            ),
            params![
                charge_id.to_string(),
                transition_id.to_string(),
                transition_sequence,
                to_sql_integer(amount_bytes, "release amount_bytes")?,
                to_sql_integer(snapshot.remaining_bytes, "remaining_bytes")?,
            ],
        )?;
        read_charge(connection, charge_id)?.ok_or_else(|| {
            LedgerError::Corrupt("released charge disappeared before commit".to_string())
        })
    })();
    finish_transaction(connection, result)
}

pub(super) fn reconstitute(
    connection: &Connection,
    expected_volume: &VolumeIdentity,
    charge_id: Uuid,
    transition_id: Uuid,
    proof: &PhysicalReserveProof,
) -> Result<ChargeSnapshot, LedgerError> {
    begin_transaction(connection)?;
    let result = (|| {
        let snapshot = read_charge(connection, charge_id)?.ok_or_else(|| {
            LedgerError::Corrupt("reconstitution references missing charge".into())
        })?;
        if snapshot.volume != *expected_volume {
            return Err(LedgerError::VolumeConflict {
                charge_id,
                expected: expected_volume.clone(),
                actual: snapshot.volume,
            });
        }
        if snapshot.volume != *proof.volume() {
            return Err(LedgerError::Invalid(
                "reconstitution proof volume differs from charge".into(),
            ));
        }
        if !proof.non_sparse() || proof.physical_bytes() < PROTECTED_FREE_SPACE_FLOOR {
            return Err(LedgerError::Invalid(
                "reconstitution proof does not prove the protected floor".into(),
            ));
        }
        if let Some((sequence, kind, amount, remaining, state)) =
            read_transition(connection, charge_id, transition_id)?
        {
            if kind == "Reconstituted" && amount == 0 && remaining == 0 {
                return Ok(historical_snapshot(snapshot, sequence, remaining, state));
            }
            return Err(LedgerError::TransitionConflict {
                charge_id,
                transition_id,
            });
        }
        if snapshot.state == RecoveryReserveState::Reconstituted {
            return Err(LedgerError::AlreadyReconstituted(charge_id));
        }
        let transition_sequence = next_sequence(snapshot.transition_sequence)?;
        connection.execute(
            &format!(
                "UPDATE {CHARGE_TABLE_NAME}
                 SET active_volume_device = NULL, remaining_bytes = 0,
                     state = 'Reconstituted', transition_sequence = ?1
                 WHERE charge_id = ?2"
            ),
            params![transition_sequence, charge_id.to_string()],
        )?;
        connection.execute(
            &format!(
                "INSERT INTO {TRANSITION_TABLE_NAME}
                 (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                  remaining_bytes, state)
                 VALUES (?1, ?2, ?3, 'Reconstituted', 0, 0, 'Reconstituted')"
            ),
            params![
                charge_id.to_string(),
                transition_id.to_string(),
                transition_sequence,
            ],
        )?;
        read_charge(connection, charge_id)?.ok_or_else(|| {
            LedgerError::Corrupt("reconstituted charge disappeared before commit".to_string())
        })
    })();
    finish_transaction(connection, result)
}

#[cfg(test)]
pub(super) fn reconstitute_fixture(
    connection: &Connection,
    charge_id: Uuid,
    transition_id: Uuid,
    proof: TestReconstitutionProof,
) -> Result<ChargeSnapshot, LedgerError> {
    let proof = crate::operation_journal::capacity_reserve_namespace::test_physical_reserve_proof(
        proof.volume,
        proof.physical_bytes,
        proof.non_sparse,
    );
    reconstitute(connection, proof.volume(), charge_id, transition_id, &proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Connection, OperationId) {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(&format!(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE operation_journal (operation_id TEXT PRIMARY KEY);
                 CREATE TABLE operation_journal_full_progress (operation_id TEXT PRIMARY KEY);
                 {CREATE_CHARGE_TABLE};
                 {CREATE_TRANSITION_TABLE};"
            ))
            .expect("create ledger fixture");
        let operation_id = OperationId::new();
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id) VALUES (?1)",
                params![operation_id.to_string()],
            )
            .expect("insert operation fixture");
        (connection, operation_id)
    }

    fn request(
        operation_id: OperationId,
        charge_id: Uuid,
        device: u64,
        budget_bytes: u64,
    ) -> ChargeRequest {
        ChargeRequest {
            charge_id,
            operation_id,
            volume: VolumeIdentity { device },
            purpose: RecoveryReservePurpose::OperationRecovery,
            budget_bytes,
            margin_bytes: 16,
            precharge_capacity_bytes: budget_bytes + 16,
        }
    }

    #[test]
    fn charge_spend_release_reconstitute_and_stable_retries_are_atomic() {
        let (connection, operation_id) = fixture();
        let charge_id = Uuid::new_v4();
        let request = request(operation_id, charge_id, 7, 100);
        let charged = begin_charge(&connection, &request).expect("charge");
        assert_eq!(charged.state, RecoveryReserveState::Charged);
        assert_eq!(charged.remaining_bytes, 100);
        assert_eq!(charged.transition_sequence, 1);

        let spend_id = Uuid::new_v4();
        let spent = spend(&connection, &request.volume, charge_id, spend_id, 40).expect("spend");
        assert_eq!(spent.state, RecoveryReserveState::Spent);
        assert_eq!(spent.remaining_bytes, 60);
        assert_eq!(spent.transition_sequence, 2);

        let later_spend_id = Uuid::new_v4();
        let later = spend(&connection, &request.volume, charge_id, later_spend_id, 10)
            .expect("later spend");
        assert_eq!(later.remaining_bytes, 50);
        let retry = spend(&connection, &request.volume, charge_id, spend_id, 40)
            .expect("stable spend retry");
        assert_eq!(retry.remaining_bytes, 60);
        assert_eq!(retry.transition_sequence, 2);
        assert_eq!(
            spend(&connection, &request.volume, charge_id, spend_id, 41),
            Err(LedgerError::TransitionConflict {
                charge_id,
                transition_id: spend_id,
            })
        );
        let current = spend(&connection, &request.volume, charge_id, Uuid::new_v4(), 1)
            .expect("current spend");
        assert_eq!(current.remaining_bytes, 49);

        let release_id = Uuid::new_v4();
        let released =
            record_release_observation(&connection, &request.volume, charge_id, release_id, 20)
                .expect("release");
        assert_eq!(released.state, RecoveryReserveState::ReleaseObserved);
        assert_eq!(released.remaining_bytes, 49);
        assert_eq!(
            record_release_observation(&connection, &request.volume, charge_id, release_id, 20)
                .expect("stable release retry")
                .transition_sequence,
            released.transition_sequence
        );
        assert!(matches!(
            spend(&connection, &request.volume, charge_id, Uuid::new_v4(), 1),
            Err(LedgerError::Invalid(_))
        ));
        assert!(matches!(
            record_release_observation(&connection, &request.volume, charge_id, Uuid::new_v4(), 40,),
            Err(LedgerError::Invalid(_))
        ));

        let reconstitute_id = Uuid::new_v4();
        let proof = TestReconstitutionProof {
            volume: VolumeIdentity { device: 7 },
            physical_bytes: PROTECTED_FREE_SPACE_FLOOR,
            non_sparse: true,
        };
        let reconstituted = reconstitute_fixture(&connection, charge_id, reconstitute_id, proof)
            .expect("reconstitute fixture");
        assert_eq!(reconstituted.state, RecoveryReserveState::Reconstituted);
        assert_eq!(reconstituted.remaining_bytes, 0);
        assert!(matches!(
            spend(&connection, &request.volume, charge_id, Uuid::new_v4(), 1),
            Err(LedgerError::AlreadyReconstituted(id)) if id == charge_id
        ));
        assert!(readiness(&connection).expect("read readiness").is_some());
    }

    #[test]
    fn missing_operation_overlap_independent_volume_and_budget_bounds_fail_closed() {
        let (connection, operation_id) = fixture();
        let missing = OperationId::new();
        assert!(matches!(
            begin_charge(
                &connection,
                &request(missing, Uuid::new_v4(), 7, 100)
            ),
            Err(LedgerError::OperationNotFound(id)) if id == missing
        ));
        let first = begin_charge(&connection, &request(operation_id, Uuid::new_v4(), 7, 100))
            .expect("first volume charge");
        assert_eq!(first.volume.device, 7);
        assert!(matches!(
            begin_charge(&connection, &request(operation_id, Uuid::new_v4(), 7, 100)),
            Err(LedgerError::ActiveVolumeConflict(VolumeIdentity {
                device: 7
            }))
        ));
        let second = begin_charge(&connection, &request(operation_id, Uuid::new_v4(), 8, 100))
            .expect("independent volume charge");
        assert_eq!(second.volume.device, 8);

        let mut over_floor = request(operation_id, Uuid::new_v4(), 9, PROTECTED_FREE_SPACE_FLOOR);
        over_floor.margin_bytes = 1;
        over_floor.precharge_capacity_bytes = u64::MAX;
        assert!(matches!(
            begin_charge(&connection, &over_floor),
            Err(LedgerError::Invalid(_))
        ));
        let mut overflow = request(operation_id, Uuid::new_v4(), 10, u64::MAX - 16);
        overflow.margin_bytes = u64::MAX;
        overflow.precharge_capacity_bytes = u64::MAX;
        assert!(matches!(
            begin_charge(&connection, &overflow),
            Err(LedgerError::Invalid(_))
        ));
    }

    #[test]
    fn zero_observed_free_capacity_still_records_a_protected_floor_charge() {
        let (connection, operation_id) = fixture();
        let mut request = request(
            operation_id,
            Uuid::new_v4(),
            11,
            PROTECTED_FREE_SPACE_FLOOR - 16,
        );
        request.precharge_capacity_bytes = 0;

        let charged = begin_charge(&connection, &request)
            .expect("protected reserve charge does not spend ordinary free capacity");
        assert_eq!(charged.precharge_capacity_bytes, 0);
        assert_eq!(
            charged.budget_bytes + charged.margin_bytes,
            PROTECTED_FREE_SPACE_FLOOR
        );
        assert!(
            readiness(&connection)
                .expect("readiness remains conservative")
                .is_some_and(|reason| reason.contains("Charged"))
        );
    }

    #[test]
    fn charge_retry_preserves_the_original_free_space_observation() {
        let (connection, operation_id) = fixture();
        let charge_id = Uuid::new_v4();
        let mut first_request = request(operation_id, charge_id, 12, 100);
        first_request.precharge_capacity_bytes = 0;
        let first = begin_charge(&connection, &first_request).expect("first charge");

        let mut retry_request = first_request.clone();
        retry_request.precharge_capacity_bytes = 4096;
        let retry = begin_charge(&connection, &retry_request).expect("idempotent charge retry");
        assert_eq!(
            retry.precharge_capacity_bytes,
            first.precharge_capacity_bytes
        );
        assert_eq!(retry.transition_sequence, first.transition_sequence);
    }

    #[test]
    fn replay_arithmetic_and_canonical_aliases_fail_closed_without_mutation() {
        let (connection, operation_id) = fixture();
        let charge_id = Uuid::new_v4();
        let expected_volume = VolumeIdentity { device: 7 };
        begin_charge(&connection, &request(operation_id, charge_id, 7, 100)).expect("charge");
        let spend_id = Uuid::new_v4();
        spend(&connection, &expected_volume, charge_id, spend_id, 20).expect("spend");
        connection
            .execute(
                &format!(
                    "UPDATE {TRANSITION_TABLE_NAME}
                     SET amount_bytes = 99, remaining_bytes = 1
                     WHERE charge_id = ?1 AND transition_id = ?2"
                ),
                params![charge_id.to_string(), spend_id.to_string()],
            )
            .expect("tamper arithmetic fixture");
        let before: (i64, i64, String) = connection
            .query_row(
                &format!(
                    "SELECT remaining_bytes, transition_sequence, state
                     FROM {CHARGE_TABLE_NAME} WHERE charge_id = ?1"
                ),
                params![charge_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read charge before failed retry");
        assert!(matches!(
            spend(&connection, &expected_volume, charge_id, Uuid::new_v4(), 1),
            Err(LedgerError::Corrupt(_))
        ));
        let after: (i64, i64, String) = connection
            .query_row(
                &format!(
                    "SELECT remaining_bytes, transition_sequence, state
                     FROM {CHARGE_TABLE_NAME} WHERE charge_id = ?1"
                ),
                params![charge_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read charge after failed retry");
        assert_eq!(after, before);

        connection
            .execute(
                &format!(
                    "UPDATE {CHARGE_TABLE_NAME}
                     SET volume_device = '07', active_volume_device = '07'
                     WHERE charge_id = ?1"
                ),
                params![charge_id.to_string()],
            )
            .expect("install volume alias fixture");
        assert!(
            readiness(&connection)
                .expect_err("volume alias must fail closed")
                .contains("canonical")
        );

        connection
            .execute(
                &format!(
                    "UPDATE {CHARGE_TABLE_NAME}
                     SET volume_device = '7', active_volume_device = '7'
                     WHERE charge_id = ?1"
                ),
                params![charge_id.to_string()],
            )
            .expect("restore canonical volume fixture");
        connection
            .execute(
                &format!(
                    "UPDATE {TRANSITION_TABLE_NAME}
                     SET transition_id = ?1
                     WHERE charge_id = ?2 AND transition_id = ?3"
                ),
                params![
                    spend_id.to_string().to_uppercase(),
                    charge_id.to_string(),
                    spend_id.to_string(),
                ],
            )
            .expect("install UUID alias fixture");
        assert!(
            readiness(&connection)
                .expect_err("UUID alias must fail closed")
                .contains("canonical UUID")
        );
    }

    #[test]
    fn transition_sequence_overflow_fails_before_mutation() {
        assert!(matches!(
            next_sequence(i64::MAX),
            Err(LedgerError::Invalid(message)) if message.contains("sequence exhausted")
        ));
        assert_eq!(next_sequence(41).expect("next sequence"), 42);
    }

    #[test]
    fn empty_ledger_is_clear() {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(&format!(
                "CREATE TABLE operation_journal (operation_id TEXT PRIMARY KEY);
                 {CREATE_CHARGE_TABLE};
                 {CREATE_TRANSITION_TABLE};"
            ))
            .expect("create ledger fixture");
        assert_eq!(readiness(&connection).expect("read readiness"), None);
    }

    #[test]
    fn active_and_reconstituted_rows_block_without_live_proof() {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(&format!(
                "CREATE TABLE operation_journal (operation_id TEXT PRIMARY KEY);
                 CREATE TABLE operation_journal_full_progress (operation_id TEXT PRIMARY KEY);
                 {CREATE_CHARGE_TABLE};
                 {CREATE_TRANSITION_TABLE};"
            ))
            .expect("create ledger fixture");
        let operation_id = Uuid::new_v4();
        let charge_id = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO operation_journal (operation_id) VALUES (?1)",
                params![operation_id.to_string()],
            )
            .expect("insert operation fixture");
        connection
            .execute(
                &format!(
                    "INSERT INTO {charge_table}
                     (charge_id, volume_device, active_volume_device, operation_id, purpose,
                      budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                      state, transition_sequence)
                     VALUES (?1, '7', '7', ?2, 'OperationRecovery', 10, 1, 11, 10,
                             'Charged', 1)",
                    charge_table = CHARGE_TABLE_NAME
                ),
                params![charge_id.to_string(), operation_id.to_string()],
            )
            .expect("insert charged fixture");
        connection
            .execute(
                &format!(
                    "INSERT INTO {transition_table}
                     (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                      remaining_bytes, state)
                     VALUES (?1, ?2, 1, 'Charged', 10, 10, 'Charged')",
                    transition_table = TRANSITION_TABLE_NAME
                ),
                params![charge_id.to_string(), charge_id.to_string()],
            )
            .expect("insert charge transition");
        let reason = readiness(&connection)
            .expect("read charged readiness")
            .expect("charged row blocks readiness");
        assert!(reason.contains("Charged"));

        let spent_transition = Uuid::new_v4();
        connection
            .execute(
                &format!(
                    "UPDATE {charge_table}
                     SET remaining_bytes = 5, state = 'Spent', transition_sequence = 2
                     WHERE charge_id = ?1",
                    charge_table = CHARGE_TABLE_NAME
                ),
                params![charge_id.to_string()],
            )
            .expect("update spent fixture");
        connection
            .execute(
                &format!(
                    "INSERT INTO {transition_table}
                     (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                      remaining_bytes, state)
                     VALUES (?1, ?2, 2, 'Spent', 5, 5, 'Spent')",
                    transition_table = TRANSITION_TABLE_NAME
                ),
                params![charge_id.to_string(), spent_transition.to_string()],
            )
            .expect("insert spent transition");
        let reason = readiness(&connection)
            .expect("read spent readiness")
            .expect("spent row blocks readiness");
        assert!(reason.contains("Spent"));

        let reconstituted_transition = Uuid::new_v4();
        connection
            .execute(
                &format!(
                    "UPDATE {charge_table}
                     SET active_volume_device = NULL, remaining_bytes = 0,
                         state = 'Reconstituted', transition_sequence = 3
                     WHERE charge_id = ?1",
                    charge_table = CHARGE_TABLE_NAME
                ),
                params![charge_id.to_string()],
            )
            .expect("update reconstituted fixture");
        connection
            .execute(
                &format!(
                    "INSERT INTO {transition_table}
                     (charge_id, transition_id, transition_sequence, kind, amount_bytes,
                      remaining_bytes, state)
                     VALUES (?1, ?2, 3, 'Reconstituted', 0, 0, 'Reconstituted')",
                    transition_table = TRANSITION_TABLE_NAME
                ),
                params![charge_id.to_string(), reconstituted_transition.to_string()],
            )
            .expect("insert reconstituted transition");
        let reason = readiness(&connection)
            .expect("read reconstituted readiness")
            .expect("reconstituted row still needs live proof");
        assert!(reason.contains("live physical revalidation"));
    }

    #[test]
    fn malformed_state_blocks_startup_with_audit_error() {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(&format!(
                "CREATE TABLE operation_journal (operation_id TEXT PRIMARY KEY);
                 {CREATE_CHARGE_TABLE};
                 {CREATE_TRANSITION_TABLE};"
            ))
            .expect("create ledger fixture");
        let operation_id = Uuid::new_v4();
        let charge_id = Uuid::new_v4();
        connection
            .execute(
                &format!(
                    "INSERT INTO {CHARGE_TABLE_NAME}
                     (charge_id, volume_device, active_volume_device, operation_id, purpose,
                      budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes,
                      state, transition_sequence)
                     VALUES (?1, '7', '7', ?2, 'OperationRecovery', 10, 1, 11, 10, 'Unknown', 1)"
                ),
                params![charge_id.to_string(), operation_id.to_string()],
            )
            .expect_err("SQL state check rejects unknown value");
    }
}
