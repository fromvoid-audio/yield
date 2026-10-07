//! Original finite journal claim observation; no settlement or live capacity grant.
use super::*;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::Statement;
use crate::operation_journal::{StoredOperationRecord, owner::InputPermit};
use database_claims::{
    comparison::{format_original, released, scalar_claim},
    scalar_projection,
};
use std::{cell::RefCell, mem::size_of};

pub(super) fn validate(
    store: &OperationJournalStore,
    record: &StoredOperationRecord,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let rows = record
        .full_capacity_plan
        .as_ref()
        .map_or(0, |p| p.allocations().len())
        .checked_add(record.capacity_plan.as_ref().map_or(0, |p| p.volumes.len()))
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    // Original LIMIT257 page, acceptance <=256 and whole-page conversion before
    // cardinality/UUID validation. This established fanout is not a new quota.
    const PAGE: usize = 257;
    let nodes = 12 * size_of::<u64>() + 16 * size_of::<usize>();
    let bytes = plan_backing(record)?
        .checked_add(
            rows.checked_mul(2 * size_of::<usize>() + nodes)
                .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
        )
        .and_then(|n| {
            n.checked_add((2 * PAGE + 4) * size_of::<Option<scalar_projection::UuidProjection>>())
        })
        .and_then(|n| {
            n.checked_add(
                8 * (36 + size_of::<String>())
                    + 3 * size_of::<Statement<'static>>()
                    + 4 * size_of::<usize>(),
            )
        })
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    let working = input.reserve_historical_scratch(bytes)?;
    let native = RefCell::new(None);
    let mut formatted = None;
    let result = (|| {
        let operation = record.operation_id;
        let plan = super::super::full_capacity_plan_for_record(record)?;
        let expected: Vec<_> = plan
            .allocations()
            .iter()
            .filter(|a| a.class() == FullCapacityAllocationClass::JournalRecord)
            .collect();
        let mut key = [0; 36];
        let key = operation.as_uuid().hyphenated().encode_lower(&mut key);
        let mut statement = Statement::new(&store.connection, input, &native,
            c"SELECT CASE WHEN typeof(claim_id)='text' AND length(claim_id)=36 THEN claim_id END FROM operation_journal_ordinary_claim WHERE operation_id=?1 AND allocation_class='JournalRecord' ORDER BY claim_id LIMIT 257")?;
        statement.bind_text(1, key)?;
        let mut ids = Vec::with_capacity(PAGE);
        while statement.next()? {
            ids.push(scalar_projection::uuid_value(
                statement.get_ref(0)?,
                0,
                statement.column_name(0)?,
            )?);
        }
        drop(statement);
        if ids.len() > 256 || ids.len() != expected.len() || ids.is_empty() {
            return Err(database_claims::error("attempt journal claim cardinality"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for projected in ids {
            let projected = projected
                .ok_or_else(|| database_claims::error("attempt journal claim key shape"))?;
            let uuid = projected
                .id
                .map_err(|error| format_original(error, input, &mut formatted))?;
            if uuid.is_nil() || !projected.canonical {
                return Err(database_claims::error("attempt journal claim key identity"));
            }
            let id = AdmissionClaimId::from_uuid(uuid)
                .map_err(|error| format_original(error, input, &mut formatted))?;
            let claim = scalar_claim(&store.connection, id, input, &native, &mut formatted)?
                .ok_or_else(|| database_claims::error("attempt journal claim missing"))?;
            if claim.operation_id() != operation
                || claim.allocation_class() != FullCapacityAllocationClass::JournalRecord
                || !seen.insert(claim.volume().device)
                || !expected.iter().any(|a| {
                    a.volume() == claim.volume() && a.allocated_bytes() == claim.allocated_bytes()
                })
                || released(&store.connection, id, input, &native, &mut formatted)?
            {
                return Err(database_claims::error(
                    "attempt journal claim differs from retained original plan",
                ));
            }
        }
        store.validate_namespace_with_input(Some(input), true)?;
        Ok(())
    })();
    if result.is_err() {
        input.retain_historical_error(working, None, formatted, native.into_inner());
    }
    result
}
