//! Original outer audit producers, prepaid without changing observation rules.
use super::*;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::Statement;
use crate::operation_journal::{codec::historical_observation::FIXED_BYTES, owner::InputPermit};
use rusqlite::types::ValueRef;
use std::{cell::RefCell, ffi::CStr, mem::size_of};

#[cfg(test)]
thread_local! {
    pub(in crate::operation_journal::store) static PAGE_CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(in crate::operation_journal::store) static PHYSICAL_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
struct Table {
    starts: bool,
    count: &'static CStr,
    invalid: &'static CStr,
    page: &'static CStr,
}
const TABLES: [Table;2] = [
    Table {
        starts:true,
        count:c"SELECT COUNT(*) FROM (SELECT 1 FROM operation_journal_database_attempt LIMIT ?1)",
        invalid:c"SELECT EXISTS(SELECT 1 FROM operation_journal_database_attempt WHERE typeof(attempt_id)!='text' OR length(attempt_id)!=36)",
        page:c"SELECT CASE WHEN typeof(attempt_id)='text' AND length(attempt_id)=36 THEN attempt_id END FROM operation_journal_database_attempt WHERE attempt_id>?1 ORDER BY attempt_id LIMIT ?2",
    },
    Table {
        starts:false,
        count:c"SELECT COUNT(*) FROM (SELECT 1 FROM operation_journal_database_attempt_outcome LIMIT ?1)",
        invalid:c"SELECT EXISTS(SELECT 1 FROM operation_journal_database_attempt_outcome WHERE typeof(attempt_id)!='text' OR length(attempt_id)!=36)",
        page:c"SELECT CASE WHEN typeof(attempt_id)='text' AND length(attempt_id)=36 THEN attempt_id END FROM operation_journal_database_attempt_outcome WHERE attempt_id>?1 ORDER BY attempt_id LIMIT ?2",
    },
];
fn checked(n: Option<usize>) -> Result<usize> {
    n.ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}
fn node<K, V>() -> usize {
    12 * size_of::<(K, V)>() + 16 * size_of::<usize>()
}
pub(super) fn collection_bytes(text: usize) -> Result<usize> {
    // Each insertion can grow a pinned full internal node independently.
    // Both nominal slice charges are conserved even for no-commit observations.
    checked(
        node::<(u64, String), ()>()
            .checked_add(node::<Uuid, usize>())
            .and_then(|n| n.checked_add(2 * node::<(Uuid, Uuid), u64>()))
            .and_then(|n| text.checked_mul(2).and_then(|s| n.checked_add(s))),
    )
}
pub(in crate::operation_journal::store) fn working_bytes() -> usize {
    FIXED_BYTES
        + size_of::<Audit>()
        + 2 * size_of::<Statement<'static>>()
        + size_of::<Vec<Option<UuidProjection>>>()
        + 4 * size_of::<InputPermit>()
        + 2 * (36 + size_of::<String>())
        + 2048
}
fn page_bytes() -> usize {
    (2 * AUDIT_PAGE_ROWS + 4) * size_of::<Option<UuidProjection>>()
}
fn uuid(row: &Statement<'_>) -> Result<Option<UuidProjection>> {
    if let ValueRef::Text(bytes) = row.get_ref(0)? {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(Some(UuidProjection::new(text)));
        }
    }
    // Original nullable String fallback succeeds only for NULL. Preserve its
    // original UTF8/type diagnostics before interpreting any page UUID.
    image_backing::scalar::<Option<String>>(row, 0)
        .map(|text| text.map(|s| UuidProjection::new(&s)))
}
fn count(
    store: &OperationJournalStore,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
    table: &Table,
) -> Result<()> {
    let mut row = Statement::new(&store.connection, input, native, table.count)?;
    row.bind_integer(1, MAX_ATTEMPTS as i64 + 1)?;
    if !row.next()? {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    let count = image_backing::scalar::<i64>(&row, 0)?;
    if count < 0 || count > MAX_ATTEMPTS as i64 {
        return Err(error("retained row ceiling"));
    }
    Ok(())
}

pub(in crate::operation_journal::store) fn read(
    store: &OperationJournalStore,
    input: &InputPermit,
) -> Result<Audit> {
    let working = input.reserve_historical_scratch(working_bytes())?;
    let mut audit = Audit {
        _working: Some(working),
        ..Audit::default()
    };
    let native = RefCell::new(None);
    let mut page_credit = None;
    let result = (|| {
        // Same original order: both row ceilings before key shape/pages.
        for table in &TABLES {
            count(store, input, &native, table)?;
        }
        for table in &TABLES {
            let mut bad = Statement::new(&store.connection, input, &native, table.invalid)?;
            if !bad.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            if image_backing::scalar::<bool>(&bad, 0)? {
                return Err(error("audit key type/length"));
            }
            drop(bad);
            let mut after = String::new();
            loop {
                page_credit = Some(input.reserve_historical_scratch(page_bytes())?);
                let mut statement = Statement::new(&store.connection, input, &native, table.page)?;
                statement.bind_text(1, &after)?;
                statement.bind_integer(2, AUDIT_PAGE_ROWS as i64)?;
                let mut page = Vec::new();
                while statement.next()? {
                    page.push(uuid(&statement)?);
                    #[cfg(test)]
                    PAGE_CELLS.with(|n| n.set(n.get() + 1));
                }
                // The original query_map collection resets its cursor before any
                // per-image validation. No borrowed cell or active cursor escapes.
                drop(statement);
                if page.is_empty() {
                    drop(page_credit.take());
                    break;
                }
                for text in page {
                    let text = text.ok_or_else(|| error("audit key shape"))?;
                    let id = text.id.map_err(error)?;
                    if id.is_nil() || !text.canonical {
                        return Err(error("audit key identity"));
                    }
                    let start = image_backing::read_start(&store.connection, id, Some(input))?
                        .ok_or_else(|| error("orphan outcome"))?;
                    let inventory =
                        store.attempt_inventory_with_input(&start, false, Some(input))?;
                    image_backing::read_outcome(&store.connection, &start, Some(input))?;
                    if table.starts {
                        audit.observe_start(&start, &inventory, Some(input))?;
                    }
                    after = id.to_string();
                }
                // All original page storage is destroyed before its debit refunds.
                drop(page_credit.take());
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(audit),
        Err(error) => {
            let working = audit._working.take().expect("original audit working loan");
            let collections = audit._collections.take();
            // Free all partial map/key allocations before transferring their loans.
            drop(audit);
            input.retain_historical_error(working, collections, page_credit, native.into_inner());
            Err(error)
        }
    }
}
