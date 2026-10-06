//! Same original page/decoder policy, with precharged backing and native errors.
use super::super::OperationJournalStoreError;
use super::super::historical_claim_observation::borrowed_sql::Statement;
use super::*;
use crate::operation_journal::owner::InputPermit;
use std::cell::RefCell;

pub(in crate::operation_journal::store) struct PageFailure {
    kind: FailureKind,
    // Original owned error must drop BEFORE its dynamic native-copy credit.
    _native: Option<InputPermit>,
}
enum FailureKind {
    Original(OrdinaryClaimReadError),
    Refused(OperationJournalStoreError),
}
impl PageFailure {
    pub(in crate::operation_journal::store) fn original(error: OrdinaryClaimReadError) -> Self {
        Self {
            kind: FailureKind::Original(error),
            _native: None,
        }
    }
    pub(in crate::operation_journal::store) fn is_refusal(&self) -> bool {
        matches!(self.kind, FailureKind::Refused(_))
    }
    pub(in crate::operation_journal::store) fn into_refusal(self) -> OperationJournalStoreError {
        match self.kind {
            FailureKind::Refused(error) => error,
            _ => unreachable!("checked refusal"),
        }
    }
}
impl std::fmt::Display for PageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            FailureKind::Original(error) => std::fmt::Display::fmt(error, f),
            FailureKind::Refused(error) => std::fmt::Display::fmt(error, f),
        }
    }
}
impl std::fmt::Debug for PageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl From<OrdinaryClaimReadError> for FailureKind {
    fn from(error: OrdinaryClaimReadError) -> Self {
        Self::Original(error)
    }
}
impl From<OperationJournalStoreError> for FailureKind {
    fn from(error: OperationJournalStoreError) -> Self {
        match error {
            OperationJournalStoreError::Sql(error) => {
                Self::Original(OrdinaryClaimReadError::Sql(error))
            }
            OperationJournalStoreError::HistoricalClaimResourceBound => {
                Self::Refused(OperationJournalStoreError::OrdinaryRebuildResourceBound)
            }
            error => Self::Refused(error),
        }
    }
}

pub(in crate::operation_journal::store) fn read_claim_page(
    connection: &Connection,
    input: &InputPermit,
    cursor: Option<OrdinaryClaimCursor>,
) -> Result<OrdinaryClaimPage, PageFailure> {
    let error_credit = RefCell::new(None);
    let result: Result<_, FailureKind> = (|| {
        let mut statement = Statement::new(
            connection,
            input,
            &error_credit,
            if cursor.is_some() {
                c"SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes FROM operation_journal_ordinary_claim WHERE claim_id > ?1 ORDER BY claim_id ASC LIMIT ?2"
            } else {
                c"SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes FROM operation_journal_ordinary_claim ORDER BY claim_id ASC LIMIT ?1"
            },
        )?;
        bind_page(&mut statement, cursor)?;
        let mut records = Vec::with_capacity(MAX_PAGE_ROWS);
        let mut has_more = false;
        while statement.next()? {
            if records.len() == MAX_PAGE_ROWS {
                has_more = true;
                break;
            }
            let values = [
                statement.get_ref(0)?,
                statement.get_ref(1)?,
                statement.get_ref(2)?,
                statement.get_ref(3)?,
                statement.get_ref(4)?,
            ];
            records.push(decode_claim_values(values)?);
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
    })();
    result.map_err(|kind| PageFailure {
        kind,
        _native: error_credit.into_inner(),
    })
}
pub(in crate::operation_journal::store) fn read_release_page(
    connection: &Connection,
    input: &InputPermit,
    cursor: Option<OrdinaryClaimCursor>,
) -> Result<OrdinaryClaimReleasePage, PageFailure> {
    let error_credit = RefCell::new(None);
    let result: Result<_, FailureKind> = (|| {
        let mut statement = Statement::new(
            connection,
            input,
            &error_credit,
            if cursor.is_some() {
                c"SELECT claim_id, operation_id, release_id FROM operation_journal_ordinary_claim_release WHERE claim_id > ?1 ORDER BY claim_id ASC LIMIT ?2"
            } else {
                c"SELECT claim_id, operation_id, release_id FROM operation_journal_ordinary_claim_release ORDER BY claim_id ASC LIMIT ?1"
            },
        )?;
        bind_page(&mut statement, cursor)?;
        let mut records = Vec::with_capacity(MAX_PAGE_ROWS);
        let mut has_more = false;
        while statement.next()? {
            if records.len() == MAX_PAGE_ROWS {
                has_more = true;
                break;
            }
            records.push(decode_release_values([
                statement.get_ref(0)?,
                statement.get_ref(1)?,
                statement.get_ref(2)?,
            ])?);
        }
        let next_cursor = has_more
            .then(|| {
                records
                    .last()
                    .map(|record| OrdinaryClaimCursor::from_claim_id(record.claim_id))
            })
            .flatten();
        Ok(OrdinaryClaimReleasePage {
            records,
            next_cursor,
        })
    })();
    result.map_err(|kind| PageFailure {
        kind,
        _native: error_credit.into_inner(),
    })
}
fn bind_page(
    statement: &mut Statement<'_>,
    cursor: Option<OrdinaryClaimCursor>,
) -> Result<(), FailureKind> {
    let limit = i64::try_from(MAX_PAGE_ROWS + 1).expect("original bounded page limit");
    if let Some(cursor) = cursor {
        validate_cursor(cursor)?;
        statement.bind_text(1, &cursor.as_text())?;
        statement.bind_integer(2, limit)?;
    } else {
        statement.bind_integer(1, limit)?;
    }
    Ok(())
}
