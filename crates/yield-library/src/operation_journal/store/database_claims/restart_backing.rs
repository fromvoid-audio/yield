//! Fund original binding/slice restart SQL and whole UUID pages before ownership.
use super::*;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::{
    Statement, scalar,
};
use crate::operation_journal::{codec::historical_observation::FIXED_BYTES, owner::InputPermit};
use std::{cell::RefCell, ffi::CStr, mem::size_of};

#[cfg(test)]
thread_local! {
    pub(in crate::operation_journal::store) static PAGE_CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
struct Table {
    count: &'static CStr,
    invalid: &'static CStr,
    page: &'static CStr,
}
const TABLES:[Table;2]=[
    Table {
        count:c"SELECT COUNT(*) FROM (SELECT 1 FROM operation_journal_database_binding LIMIT ?1)",
        invalid:c"SELECT EXISTS(SELECT 1 FROM operation_journal_database_binding WHERE typeof(operation_id)!='text' OR length(operation_id)!=36)",
        page:c"SELECT DISTINCT operation_id FROM operation_journal_database_binding WHERE operation_id>?1 ORDER BY operation_id LIMIT ?2",
    },
    Table {
        count:c"SELECT COUNT(*) FROM (SELECT 1 FROM operation_journal_database_claim_slice LIMIT ?1)",
        invalid:c"SELECT EXISTS(SELECT 1 FROM operation_journal_database_claim_slice WHERE typeof(operation_id)!='text' OR length(operation_id)!=36)",
        page:c"SELECT DISTINCT operation_id FROM operation_journal_database_claim_slice WHERE operation_id>?1 ORDER BY operation_id LIMIT ?2",
    },
];
pub(in crate::operation_journal::store) fn working_bytes() -> usize {
    FIXED_BYTES
        + 2 * size_of::<Statement<'static>>()
        + 4 * size_of::<InputPermit>()
        + size_of::<Vec<Option<scalar_projection::UuidProjection>>>()
        + size_of::<RefCell<Option<InputPermit>>>()
        + 2 * (36 + size_of::<String>())
        + 2048
}
fn page_bytes() -> usize {
    (2 * READ_PAGE_ROWS + 4) * size_of::<Option<scalar_projection::UuidProjection>>()
}
fn count(
    store: &OperationJournalStore,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
    table: &Table,
) -> Result<(), OperationJournalStoreError> {
    let mut row = Statement::new(&store.connection, input, native, table.count)?;
    row.bind_integer(1, MAX_RELATION_ROWS as i64 + 1)?;
    if !row.next()? {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    let n = scalar::<i64>(&row, 0)?;
    if n < 0 || n > MAX_RELATION_ROWS as i64 {
        return Err(error("relation row ceiling"));
    }
    Ok(())
}
pub(in crate::operation_journal::store) fn validate(
    store: &OperationJournalStore,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let working = input.reserve_historical_scratch(working_bytes())?;
    let native = RefCell::new(None);
    let mut page_credit = None;
    let result = (|| {
        // Same two original ceilings before any key/page interpretation.
        for table in &TABLES {
            count(store, input, &native, table)?;
        }
        for table in &TABLES {
            let mut bad = Statement::new(&store.connection, input, &native, table.invalid)?;
            if !bad.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            if scalar::<bool>(&bad, 0)? {
                return Err(error("restart operation identity bound"));
            }
            drop(bad);
            let mut after = String::new();
            loop {
                page_credit = Some(input.reserve_historical_scratch(page_bytes())?);
                let mut statement = Statement::new(&store.connection, input, &native, table.page)?;
                statement.bind_text(1, &after)?;
                statement.bind_integer(2, READ_PAGE_ROWS as i64)?;
                let mut page = Vec::new();
                while statement.next()? {
                    page.push(scalar_projection::uuid_value(
                        statement.get_ref(0)?,
                        0,
                        statement.column_name(0)?,
                    )?);
                    #[cfg(test)]
                    PAGE_CELLS.with(|n| n.set(n.get() + 1));
                }
                // Original query_map resets before per-operation producers run.
                drop(statement);
                if page.is_empty() {
                    drop(page_credit.take());
                    break;
                }
                for text in page {
                    let text = text.ok_or_else(|| error("restart operation identity bound"))?;
                    let id = text.id.map_err(error)?;
                    if id.is_nil() || !text.canonical {
                        return Err(error("restart noncanonical operation identity"));
                    }
                    let operation = OperationId::from_uuid(id);
                    let inventory = read_with_input(&store.connection, operation, Some(input))?
                        .ok_or_else(|| error("missing restart inventory"))?;
                    store.validate_database_claim_operation_with_input(
                        operation,
                        &inventory,
                        Some(input),
                    )?;
                    after = id.to_string();
                }
                // Every owned page cell/Vec dies before its original loan refunds.
                drop(page_credit.take());
            }
        }
        Ok(())
    })();
    if result.is_err() {
        input.retain_historical_error(working, page_credit, None, native.into_inner());
    }
    result
}
