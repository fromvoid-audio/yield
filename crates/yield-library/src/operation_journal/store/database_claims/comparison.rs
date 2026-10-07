//! Original finite DB ordinary-claim comparison, prepaid before owned projections/maps.
use super::*;
use crate::operation_journal::store::{
    historical_claim_observation::borrowed_sql::Statement, ordinary_claims,
};
use crate::operation_journal::{
    AdmissionClaim, AdmissionClaimId, StoredOperationRecord,
    codec::historical_observation::FIXED_BYTES,
    full_capacity::{FullCapacityAllocation, FullCapacityPlan},
    owner::InputPermit,
};
use rusqlite::types::ValueRef;
use std::{
    cell::RefCell,
    fmt::{self, Write},
    mem::size_of,
};

struct Count(usize);
impl Write for Count {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.0 = self.0.checked_add(value.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}
fn format_original(
    original: impl fmt::Display,
    input: &InputPermit,
    formatted: &mut Option<InputPermit>,
) -> OperationJournalStoreError {
    // Display the same original error into a scalar counter, before constructing
    // either the original to_string or the outer DB comparison reason String.
    let mut length = Count(0);
    if write!(&mut length, "{original}").is_err() {
        return OperationJournalStoreError::HistoricalClaimResourceBound;
    }
    let Some(bytes) = length
        .0
        .checked_mul(4)
        .and_then(|n| n.checked_add(2 * "database claim inventory: ".len()))
    else {
        return OperationJournalStoreError::HistoricalClaimResourceBound;
    };
    match input.reserve_historical_scratch(bytes) {
        Ok(credit) => {
            *formatted = Some(credit);
            error(original)
        }
        Err(error) => error,
    }
}

fn scalar_claim(
    connection: &Connection,
    id: AdmissionClaimId,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
    formatted: &mut Option<InputPermit>,
) -> Result<Option<ordinary_claims::OrdinaryClaimRecord>, OperationJournalStoreError> {
    let result = (|| {
        let mut key = [0; 36];
        let key = id.as_uuid().hyphenated().encode_lower(&mut key);
        let mut row = Statement::new(connection, input, native,
            c"SELECT claim_id,operation_id,volume_device,allocation_class,allocated_bytes FROM operation_journal_ordinary_claim WHERE claim_id=?1")?;
        row.bind_text(1, key)?;
        if !row.next()? {
            return Ok(None);
        }
        let values = [
            row.get_ref(0)?,
            row.get_ref(1)?,
            row.get_ref(2)?,
            row.get_ref(3)?,
            row.get_ref(4)?,
        ];
        ordinary_claims::decode_claim_values(values)
            .map(Some)
            .map_err(|error| format_original(error, input, formatted))
    })();
    map_native_scalar_error(result, input, formatted)
}
fn released(
    connection: &Connection,
    id: AdmissionClaimId,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
    formatted: &mut Option<InputPermit>,
) -> Result<bool, OperationJournalStoreError> {
    let result = (|| {
        let mut key = [0; 36];
        let key = id.as_uuid().hyphenated().encode_lower(&mut key);
        let mut row = Statement::new(connection, input, native,
            c"SELECT claim_id,operation_id,release_id FROM operation_journal_ordinary_claim_release WHERE claim_id=?1")?;
        row.bind_text(1, key)?;
        if !row.next()? {
            return Ok(false);
        }
        ordinary_claims::decode_release_values([row.get_ref(0)?, row.get_ref(1)?, row.get_ref(2)?])
            .map(|_| true)
            .map_err(|error| format_original(error, input, formatted))
    })();
    map_native_scalar_error(result, input, formatted)
}
fn map_native_scalar_error<T>(
    result: Result<T, OperationJournalStoreError>,
    input: &InputPermit,
    formatted: &mut Option<InputPermit>,
) -> Result<T, OperationJournalStoreError> {
    match result {
        Err(OperationJournalStoreError::Sql(error)) => Err(format_original(
            ordinary_claims::OrdinaryClaimReadError::Sql(error),
            input,
            formatted,
        )),
        other => other,
    }
}
fn plan_count(record: &StoredOperationRecord) -> usize {
    record
        .full_capacity_plan
        .as_ref()
        .map_or(0, |p| p.allocations().len())
        + record.capacity_plan.as_ref().map_or(0, |p| p.volumes.len())
}
fn backing(
    record: &StoredOperationRecord,
    claims: usize,
) -> Result<usize, OperationJournalStoreError> {
    let n = plan_count(record);
    // Original clone/filter vectors can coexist. Full plan validation builds a
    // pinned BTreeMap<VolumeIdentity,(unit,bytes,floor)>; charge a full internal
    // node per possible volume, independent of dense occupancy.
    let node = 12 * size_of::<(crate::durable_capacity::VolumeIdentity, (u64, u64, u64))>()
        + 16 * size_of::<usize>();
    n.checked_mul(4 * size_of::<FullCapacityAllocation>() + node)
        .and_then(|n| {
            claims
                .checked_mul(2 * size_of::<AdmissionClaim>())
                .and_then(|c| n.checked_add(c))
        })
        .and_then(|n| {
            n.checked_add(
                4 * size_of::<AdmissionClaim>()
                    + size_of::<FullCapacityPlan>()
                    + size_of::<AdmissionClaimInventory>(),
            )
        })
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}

pub(super) fn validate(
    store: &OperationJournalStore,
    record: &StoredOperationRecord,
    inventory: &DatabaseClaimInventory,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    // Original scalar decoders own only fixed capped UUID/device/class strings,
    // fixed malformed-row reasons and headers. Every dynamic native reason is
    // separately funded by the shared original borrowed statement bridge.
    let working = input.reserve_historical_scratch(
        FIXED_BYTES
            + 3 * size_of::<Statement<'static>>()
            + 8 * (36 + size_of::<String>())
            + 4 * size_of::<InputPermit>(),
    )?;
    let native = RefCell::new(None);
    let mut comparison = None;
    let mut formatted = None;
    let result = (|| {
        let mut key = [0; 36];
        let key = record
            .operation_id
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut key);
        let mut count = Statement::new(&store.connection, input, &native,
            c"SELECT COUNT(*) FROM (SELECT claim_id FROM operation_journal_ordinary_claim WHERE operation_id=?1 AND allocation_class IN('SourceDatabaseCommit','SourceWalShm','GlobalDatabaseCommit','GlobalWalShm') ORDER BY claim_id LIMIT 257)")?;
        count.bind_text(1, key)?;
        if !count.next()? {
            return Err(rusqlite::Error::QueryReturnedNoRows.into());
        }
        let ValueRef::Integer(n) = count.get_ref(0)? else {
            return Err(error("operation database claim bound"));
        };
        if !(0..=257).contains(&n) {
            return Err(error("operation database claim bound"));
        }
        drop(count);
        comparison = Some(
            input.reserve_historical_scratch(
                backing(record, n.min(256) as usize)?
                    .checked_add(
                        (2 * n as usize + 4)
                            * size_of::<Option<scalar_projection::UuidProjection>>(),
                    )
                    .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
            )?,
        );
        let plan = super::super::full_capacity_plan_for_record(record)?;
        let plan = FullCapacityPlan::new(
            plan.allocations()
                .iter()
                .filter(|a| class_tag(a.class()) != 0)
                .cloned()
                .collect(),
        )
        .map_err(|error| format_original(error, input, &mut formatted))?;
        let mut claims = Vec::with_capacity(n.min(256) as usize);
        let mut ids = Statement::new(&store.connection, input, &native,
            c"SELECT CASE WHEN length(claim_id)=36 THEN claim_id END FROM operation_journal_ordinary_claim WHERE operation_id=?1 AND allocation_class IN('SourceDatabaseCommit','SourceWalShm','GlobalDatabaseCommit','GlobalWalShm') ORDER BY claim_id LIMIT 257")?;
        ids.bind_text(1, key)?;
        // Preserve the original page semantics: all nullable type/UTF-8/SQL
        // observations precede row UUID and coverage errors. These fixed-size
        // projections are prepaid; no original key String is copied.
        let mut projections = Vec::with_capacity(n as usize);
        while ids.next()? {
            projections.push(scalar_projection::uuid_value(
                ids.get_ref(0)?,
                0,
                ids.column_name(0)?,
            )?);
        }
        drop(ids);
        if projections.len() > 256 {
            return Err(error("operation database claim bound"));
        }
        for projected in projections {
            let projected = projected.ok_or_else(|| error("ordinary claim identity bound"))?;
            let uuid = projected
                .id
                .map_err(|error| format_original(error, input, &mut formatted))?;
            let id = AdmissionClaimId::from_uuid(uuid)
                .map_err(|error| format_original(error, input, &mut formatted))?;
            let claim = scalar_claim(&store.connection, id, input, &native, &mut formatted)?
                .ok_or_else(|| error("missing ordinary claim"))?;
            claims.push(
                AdmissionClaim::new(
                    claim.claim_id(),
                    claim.volume().clone(),
                    claim.allocation_class(),
                    claim.allocated_bytes(),
                )
                .map_err(|error| format_original(error, input, &mut formatted))?,
            );
            if released(&store.connection, id, input, &native, &mut formatted)? {
                return Err(error(
                    "slice-bound ordinary claim was released without attempt settlement",
                ));
            }
        }
        let claims = AdmissionClaimInventory::new(claims)
            .map_err(|error| format_original(error, input, &mut formatted))?;
        inventory
            .validate_against(&plan, &claims)
            .map_err(|error| format_original(error, input, &mut formatted))?;
        Ok(())
    })();
    if result.is_err() {
        input.retain_historical_error(working, comparison, formatted, native.into_inner());
    }
    result
}
