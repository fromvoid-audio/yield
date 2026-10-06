//! Original ordinary collection backing. Other family/provider graphs remain separate.
use super::historical_claim_observation::borrowed_sql::Statement;
use super::*;
use crate::operation_journal::owner::InputPermit;
use std::cell::RefCell;
use std::mem::size_of;

/// Single-owner snapshot: output backing drops before its final original-pool credit.
/// No Clone or uncharged deep-copy API is supplied.
pub(in crate::operation_journal) struct FundedSnapshot {
    snapshot: OrdinaryClaimRebuildSnapshot,
    _collection: Option<InputPermit>,
}
impl FundedSnapshot {
    #[cfg(any(test, feature = "test-support"))]
    pub(in crate::operation_journal) fn collection_debit_for_test(&self) -> usize {
        self._collection
            .as_ref()
            .map_or(0, InputPermit::historical_test_funded_bytes)
    }
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
impl FundedSnapshot {
    pub(in crate::operation_journal) fn claims(&self) -> &[RebuildClaim] {
        self.snapshot.claims()
    }
    pub(in crate::operation_journal) fn readiness(&self) -> &CapacityRebuildReadiness {
        self.snapshot.readiness()
    }
    pub(in crate::operation_journal) fn proves_durable_zero(&self) -> bool {
        self.snapshot.proves_durable_zero()
    }
    pub(in crate::operation_journal) fn validate_provider(
        &self,
        provider: &super::super::capacity_provider::CapacityProvider,
        input: Option<&InputPermit>,
    ) -> Result<(), super::super::capacity_provider::CapacityProviderError> {
        match input {
            Some(input) => provider.validate_rebuilt_claims_for_input(&self.snapshot, input),
            None => provider.validate_rebuilt_claims(&self.snapshot),
        }
    }
    pub(in crate::operation_journal) fn install_provider_for_input(
        &self,
        provider: &super::super::capacity_provider::CapacityProvider,
        input: &InputPermit,
    ) -> Result<(), super::super::capacity_provider::CapacityProviderError> {
        provider.install_rebuilt_claims_for_input(&self.snapshot, input)
    }
    #[cfg(test)]
    pub(super) fn matches_original(&self, original: &OrdinaryClaimRebuildSnapshot) -> bool {
        self.snapshot == *original
    }
    #[cfg(test)]
    pub(in crate::operation_journal) fn install_provider_component(
        &self,
        provider: &super::super::capacity_provider::CapacityProvider,
    ) -> Result<(), super::super::capacity_provider::CapacityProviderError> {
        self.install_provider_component_compatibility(provider)
    }
    pub(in crate::operation_journal) fn install_provider_component_compatibility(
        &self,
        provider: &super::super::capacity_provider::CapacityProvider,
    ) -> Result<(), super::super::capacity_provider::CapacityProviderError> {
        provider.install_rebuilt_claims(&self.snapshot)
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
            + "ordinary claim rebuild count is not a nonnegative SQLite integer".len()
            + 2 * ("historical claim observation rejected: ".len()
                + "ordinary claim rebuild count is not a nonnegative SQLite integer".len());
        let sql = input
            .reserve_historical_scratch(sql_bytes)
            .map_err(resource_error)?;
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
                return Err(resource_error(error));
            }
        };
        // A full pinned internal BTree node per entry covers sparse occupancy,
        // with all claim/release/conflict maps live alongside input/output Vecs.
        let bytes = collection_bytes(claims, releases)
            .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        let credit = input
            .reserve_historical_scratch(bytes)
            .map_err(resource_error)?;
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
    usize::try_from(value).map_err(|_| OperationJournalStoreError::OrdinaryRebuildResourceBound)
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
    let diagnostics =
        2 * MAX_REBUILD_DIAGNOSTICS * (size_of::<String>() + 4 * MAX_REBUILD_DIAGNOSTIC_CHARS)
            + size_of::<Vec<String>>();
    rows.checked_add(release_maps)?
        .checked_add(page)?
        .checked_add(headers)?
        .checked_add(page_headers)?
        .checked_add(text)?
        .checked_add(diagnostics)
}

fn resource_error(error: OperationJournalStoreError) -> OperationJournalStoreError {
    match error {
        OperationJournalStoreError::HistoricalClaimResourceBound => {
            OperationJournalStoreError::OrdinaryRebuildResourceBound
        }
        other => other,
    }
}

#[cfg(any(test, feature = "test-support"))]
impl OperationJournalStore {
    /// Keep collection-pressure fixtures beyond the same original namespace
    /// phase on each platform; Windows formatter scratch is larger than Unix.
    #[cfg(test)]
    pub(in crate::operation_journal) fn ordinary_collection_pressure_rows_for_test(
        &self,
    ) -> Result<usize, OperationJournalStoreError> {
        let required = self
            .retained_namespace_working_bytes_for_test()
            // Funded reader controls retain their outgoing error Box loan while
            // this same original nested namespace guard runs.
            .checked_add(transaction_control::WORKING_BYTES)
            .and_then(|bytes| bytes.checked_add(2))
            .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        let mut rows = 32usize;
        loop {
            let debit = collection_bytes(rows, 0)
                .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
            if debit >= required {
                return Ok(rows);
            }
            rows = rows
                .checked_mul(2)
                .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        }
    }

    /// Original-connection collection allocation fixture, without an owner or effects.
    pub fn install_ordinary_collection_fixture_for_test(
        &self,
        operation: OperationId,
        rows: usize,
        released: bool,
    ) -> Result<(), OperationJournalStoreError> {
        self.install_ordinary_collection_volume_fixture_for_test(
            operation,
            rows,
            released,
            VolumeIdentity { device: 1 },
            1,
        )
    }

    /// Original SQLite fixture image; supplied scalar facts do not qualify or issue claims.
    pub(in crate::operation_journal) fn install_ordinary_collection_volume_fixture_for_test(
        &self,
        operation: OperationId,
        rows: usize,
        released: bool,
        volume: VolumeIdentity,
        allocated_bytes: u64,
    ) -> Result<(), OperationJournalStoreError> {
        self.connection.execute_batch("DELETE FROM operation_journal_ordinary_claim_release; DELETE FROM operation_journal_ordinary_claim")?;
        for index in 1..=rows {
            let claim = Uuid::from_u128(index as u128).to_string();
            self.connection.execute("INSERT INTO operation_journal_ordinary_claim VALUES (?1,?2,?3,'DestinationStaging',?4)", params![claim, operation.to_string(), volume.device.to_string(), i64::try_from(allocated_bytes).unwrap()])?;
            if released {
                self.connection.execute(
                    "INSERT INTO operation_journal_ordinary_claim_release VALUES (?1,?2,?1)",
                    params![claim, operation.to_string()],
                )?;
            }
        }
        Ok(())
    }

    /// Measure only original ordinary collection construction and custody.
    /// Full-family audits, provider graphs and native/RSS allocation are outside this probe.
    pub fn ordinary_collection_allocation_probe_for_test(
        &self,
        funded: bool,
    ) -> Result<(usize, bool, usize), OperationJournalStoreError> {
        // The same nested reader now funds its original precommit namespace.
        // Give this component probe the original reply keeper required by that
        // guard, just as genuine queued startup/admission already does.
        let input = InputPermit::historical_test_namespace_input(1);
        self.connection.execute_batch("BEGIN")?;
        let mut collection = None;
        let result = self.rebuild_ordinary_claim_snapshot_with_funding(
            funded.then_some(&input),
            &mut collection,
        );
        self.connection.execute_batch("ROLLBACK")?;
        let snapshot = result?;
        let bytes = collection
            .as_ref()
            .map_or(0, |backing| backing._credit.historical_test_funded_bytes());
        let scalar = (
            snapshot.claims().len(),
            snapshot.proves_durable_zero(),
            bytes,
        );
        drop(snapshot);
        drop(collection);
        Ok(scalar)
    }
}
