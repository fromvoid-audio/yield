//! Original ordinary collection backing. Other family/provider graphs remain separate.
use super::historical_claim_observation::borrowed_sql::Statement;
use super::*;
use crate::operation_journal::owner::InputPermit;
use std::cell::RefCell;
use std::mem::size_of;
use std::ops::Deref;

/// Single-owner snapshot: output backing drops before its final original-pool credit.
/// No Clone or uncharged deep-copy API is supplied.
pub(in crate::operation_journal) struct FundedSnapshot {
    snapshot: OrdinaryClaimRebuildSnapshot,
    _collection: Option<InputPermit>,
}
impl FundedSnapshot {
    pub(super) fn new(snapshot: OrdinaryClaimRebuildSnapshot, credit: Option<InputPermit>) -> Self {
        Self {
            snapshot,
            _collection: credit,
        }
    }
    pub(in crate::operation_journal) fn unfunded(snapshot: OrdinaryClaimRebuildSnapshot) -> Self {
        Self::new(snapshot, None)
    }
}
impl FundedSnapshot {
    pub(super) fn into_unfunded(self) -> OrdinaryClaimRebuildSnapshot {
        assert!(
            self._collection.is_none(),
            "funded snapshot cannot escape its backing"
        );
        self.snapshot
    }
}
impl Deref for FundedSnapshot {
    type Target = OrdinaryClaimRebuildSnapshot;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

pub(super) struct Collection {
    pub(super) claims: usize,
    _credit: InputPermit,
}
impl Collection {
    pub(super) fn new(
        connection: &Connection,
        input: &InputPermit,
    ) -> Result<Self, OperationJournalStoreError> {
        // Before count SQL: compiled statement/credit headers and bounded error formatting.
        let sql_bytes = 2 * size_of::<Statement<'_>>()
            + size_of::<RefCell<Option<InputPermit>>>()
            + 3 * size_of::<InputPermit>()
            + 2 * size_of::<String>()
            + 2 * "ordinary claim rebuild count is not a nonnegative SQLite integer".len();
        let sql = input.reserve_historical_scratch(sql_bytes)?;
        let native_error = RefCell::new(None);
        let result = (|| {
            let claims = count(
                connection,
                input,
                &native_error,
                c"SELECT COUNT(*) FROM operation_journal_ordinary_claim",
            )?;
            let releases = count(
                connection,
                input,
                &native_error,
                c"SELECT COUNT(*) FROM operation_journal_ordinary_claim_release",
            )?;
            Ok((claims, releases))
        })();
        let (claims, releases) = match result {
            Ok(counts) => counts,
            Err(error) => {
                input.retain_historical_error(sql, None, None, native_error.into_inner());
                return Err(error);
            }
        };
        // A full pinned internal BTree node per entry covers sparse occupancy,
        // with all claim/release/conflict maps live alongside input/output Vecs.
        let bytes = collection_bytes(claims, releases)
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let credit = input.reserve_historical_scratch(bytes)?;
        Ok(Self {
            claims,
            _credit: credit,
        })
    }
    pub(super) fn into_credit(self) -> InputPermit {
        self._credit
    }
}
fn count(
    connection: &Connection,
    input: &InputPermit,
    error: &RefCell<Option<InputPermit>>,
    sql: &'static std::ffi::CStr,
) -> Result<usize, OperationJournalStoreError> {
    let mut statement = Statement::new(connection, input, error, sql)?;
    if !statement.next()? {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    let ValueRef::Integer(value) = statement.get_ref(0)? else {
        return Err(OperationJournalStoreError::HistoricalClaimObservation {
            reason: "ordinary claim rebuild count is not a nonnegative SQLite integer".into(),
        });
    };
    usize::try_from(value).map_err(|_| OperationJournalStoreError::HistoricalClaimResourceBound)
}
fn node<K, V>() -> usize {
    12 * size_of::<(K, V)>() + 12 * size_of::<usize>() + 4 * size_of::<usize>()
}
fn collection_bytes(claims: usize, releases: usize) -> Option<usize> {
    let rows = claims.checked_mul(
        size_of::<ordinary_claims::OrdinaryClaimRecord>()
            + size_of::<RebuildClaim>()
            + node::<AdmissionClaimId, ()>(),
    )?;
    let release_maps = releases.checked_mul(
        node::<AdmissionClaimId, ordinary_claims::OrdinaryClaimReleaseRecord>()
            + node::<Uuid, AdmissionClaimId>()
            + 2 * node::<AdmissionClaimId, ()>(),
    )?;
    let page = ordinary_claims::MAX_PAGE_ROWS.checked_mul(
        size_of::<ordinary_claims::OrdinaryClaimRecord>()
            .max(size_of::<ordinary_claims::OrdinaryClaimReleaseRecord>()),
    )?;
    let headers = size_of::<FundedSnapshot>()
        + 3 * size_of::<Vec<RebuildClaim>>()
        + 2 * size_of::<BTreeMap<AdmissionClaimId, Uuid>>()
        + 2 * size_of::<BTreeSet<AdmissionClaimId>>();
    // Original row parser simultaneously owns two UUIDs, volume/class text and
    // bounded UUID/device reformatting; arbitrary stored text is rejected before copy.
    let parser_text = 2 * (2 * 36 + 20 + 26) + 2 * 36 + 2 * 20 + 2 * 36;
    // Unsupported class Debug can escape EACH bounded byte, alongside the original
    // owned reason/claim strings and the page diagnostic before capped consumption.
    let reason = 6 * 26 + 2 + "allocation_class  is unsupported".len();
    let claim_context = " for ".len() + 36;
    let diagnostic = "ordinary-claim release page was retained but could not be decoded: ordinary-claim row is malformed: ".len() + claim_context + reason;
    let text = parser_text + 2 * reason + 2 * claim_context + 2 * diagnostic;
    let page_headers = size_of::<Statement<'_>>()
        + size_of::<RefCell<Option<InputPermit>>>()
        + 4 * size_of::<String>();
    rows.checked_add(release_maps)?
        .checked_add(page)?
        .checked_add(headers)?
        .checked_add(page_headers)?
        .checked_add(text)
}
