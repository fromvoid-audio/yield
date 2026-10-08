//! The original chunk codec on borrowed original-connection statements.
use super::super::{
    OperationJournalStoreError,
    historical_claim_observation::borrowed_sql::{Statement, scalar},
};
use super::*;
use crate::operation_journal::owner::{InputPermit, StartupCredit};
use std::cell::RefCell;

impl From<ChunkClaimReadError> for OperationJournalStoreError {
    fn from(error: ChunkClaimReadError) -> Self {
        Self::ChunkClaim {
            reason: error.to_string(),
        }
    }
}
fn sql_error(error: OperationJournalStoreError) -> OperationJournalStoreError {
    match error {
        OperationJournalStoreError::Sql(error) => ChunkClaimReadError::Sql(error).into(),
        error => error,
    }
}
impl DecodeRow for Statement<'_> {
    type Error = OperationJournalStoreError;
    fn text(&self, index: usize) -> Result<&str, Self::Error> {
        use rusqlite::types::{Type, ValueRef};
        let index = index as i32;
        match self.get_ref(index).map_err(sql_error)? {
            ValueRef::Text(bytes) => std::str::from_utf8(bytes).map_err(|error| {
                ChunkClaimReadError::Sql(rusqlite::Error::FromSqlConversionFailure(
                    index as usize,
                    Type::Text,
                    Box::new(error),
                ))
                .into()
            }),
            value => Err(ChunkClaimReadError::Sql(rusqlite::Error::InvalidColumnType(
                index as usize,
                self.column_name(index)?.to_owned(),
                value.data_type(),
            ))
            .into()),
        }
    }
    fn optional_text(&self, index: usize) -> Result<Option<&str>, Self::Error> {
        if matches!(
            self.get_ref(index as i32).map_err(sql_error)?,
            rusqlite::types::ValueRef::Null
        ) {
            Ok(None)
        } else {
            self.text(index).map(Some)
        }
    }
    fn integer(&self, index: usize) -> Result<i64, Self::Error> {
        scalar(self, index as i32).map_err(sql_error)
    }
}

/// Original owned row followed by its exclusive original-pool keeper.
/// No Clone, Deref or owned-row extraction bypasses the keeper.
#[derive(Debug)]
pub(crate) struct FundedChunkClaimRecord {
    record: ChunkClaimRecord,
    _credit: Option<StartupCredit>,
}
impl PartialEq for FundedChunkClaimRecord {
    fn eq(&self, other: &Self) -> bool {
        self.record == other.record
    }
}
impl Eq for FundedChunkClaimRecord {}
impl FundedChunkClaimRecord {
    pub(in crate::operation_journal) fn unfunded(record: ChunkClaimRecord) -> Self {
        Self {
            record,
            _credit: None,
        }
    }
    pub(in crate::operation_journal) fn matches_request(
        &self,
        request: &ChunkClaimRequest,
    ) -> bool {
        self.record.matches_request(request)
    }
    pub(in crate::operation_journal::store) fn into_unfunded(self) -> ChunkClaimRecord {
        assert!(
            self._credit.is_none(),
            "funded chunk row cannot escape its keeper"
        );
        self.record
    }
    #[cfg(test)]
    pub(in crate::operation_journal) fn matches_original(&self, record: &ChunkClaimRecord) -> bool {
        self.record == *record
    }
    #[cfg(test)]
    pub(in crate::operation_journal) fn state(&self) -> ChunkClaimState {
        self.record.state()
    }
}

pub(super) fn read(
    connection: &Connection,
    chunk: ChunkId,
    input: &InputPermit,
) -> Result<Option<FundedChunkClaimRecord>, OperationJournalStoreError> {
    // The original codec owns only its bounded output key and error strings.
    // Its header and SQL/driver conversion envelope are prepaid before prepare.
    let credit = input.reserve_historical_scratch(point_read_bytes())?;
    read_prepaid(connection, chunk, input, credit)
}
pub(super) const fn point_read_bytes() -> usize {
    4096 + MAX_OUTPUT_KEY_BYTES
        + 2 * std::mem::size_of::<FundedChunkClaimRecord>()
        + std::mem::size_of::<Statement<'static>>()
}
pub(super) fn read_prepaid(
    connection: &Connection,
    chunk: ChunkId,
    input: &InputPermit,
    credit: InputPermit,
) -> Result<Option<FundedChunkClaimRecord>, OperationJournalStoreError> {
    if !credit.covers_original_pool(input, point_read_bytes()) {
        return Err(OperationJournalStoreError::HistoricalClaimResourceBound);
    }
    let native = RefCell::new(None);
    let result =
        (|| {
            let mut statement = Statement::new(connection, input, &native,
            c"SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
         logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
         protected_free_bytes, state, issue_id, release_id
         FROM operation_journal_chunk_claim WHERE chunk_id = ?1").map_err(sql_error)?;
            let mut key = [0; 36];
            statement
                .bind_text(1, chunk.0.hyphenated().encode_lower(&mut key))
                .map_err(sql_error)?;
            if statement.next().map_err(sql_error)? {
                decode_view(&statement).map(Some)
            } else {
                Ok(None)
            }
        })();
    if let Some(native) = native.into_inner() {
        input.retain_historical_error(native, None, None, None);
    }
    match result {
        Ok(Some(record)) => Ok(Some(FundedChunkClaimRecord {
            record,
            _credit: Some(credit.into_startup_credit()),
        })),
        Ok(None) => Ok(None),
        Err(error) => {
            input.retain_historical_error(credit, None, None, None);
            Err(error)
        }
    }
}

pub(super) fn exceeds_row_limit(
    connection: &Connection,
    limit: usize,
    input: &InputPermit,
) -> Result<bool, OperationJournalStoreError> {
    if limit > MAX_PAGE_ROWS {
        return Err(ChunkClaimReadError::PageLimitExceeded {
            requested: limit,
            maximum: MAX_PAGE_ROWS,
        }
        .into());
    }
    super::super::initial_settlement_backing::statement(
        connection,
        input,
        c"SELECT chunk_id FROM operation_journal_chunk_claim ORDER BY chunk_id ASC LIMIT ?1",
        |row| {
            row.bind_integer(1, (limit + 1) as i64)?;
            let mut seen = 0;
            while row.next()? {
                seen += 1;
                if seen > limit {
                    return Ok(true);
                }
            }
            Ok(false)
        },
    )
    .map_err(sql_error)
}

pub(super) fn insert(
    connection: &Connection,
    request: &ChunkClaimRequest,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    request
        .validate()
        .map_err(|error| OperationJournalStoreError::ChunkClaim {
            reason: error.to_string(),
        })?;
    super::super::initial_settlement_backing::statement(
        connection,
        input,
        c"INSERT INTO operation_journal_chunk_claim
             (chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
              logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
              protected_free_bytes, state, issue_id, release_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'Claimed', NULL, NULL)",
        |row| {
            row.bind_text(1, &request.chunk_id.to_string())?;
            row.bind_text(2, &request.operation_id.to_string())?;
            row.bind_text(3, &request.claim.claim_id().to_string())?;
            row.bind_text(4, &request.participant_id.to_string())?;
            row.bind_text(5, &request.output_key)?;
            row.bind_integer(6, request.logical_offset as i64)?;
            row.bind_integer(7, request.logical_length as i64)?;
            row.bind_text(8, &request.claim.volume().device.to_string())?;
            row.bind_text(9, allocation_class_text(request.claim.allocation_class()))?;
            row.bind_integer(10, request.claim.allocated_bytes() as i64)?;
            row.bind_integer(11, request.allocation_unit as i64)?;
            row.bind_integer(12, request.protected_free_bytes as i64)?;
            row.next().map(|_| ())
        },
    )
    .map_err(sql_error)
}
