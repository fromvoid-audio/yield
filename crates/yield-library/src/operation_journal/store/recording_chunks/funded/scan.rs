//! Original whole operation/slot pages and shared prefix classifier.
use super::*;

pub(in crate::operation_journal::store) fn validate(
    store: &OperationJournalStore,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let working = input.reserve_historical_scratch(working_bytes(store)?)?;
    let native = RefCell::new(None);
    let mut page_credit = None;
    let result = (|| {
        totals(store, input, &native)?;
        let mut after = String::new();
        loop {
            page_credit =
                Some(input.reserve_historical_scratch((2 * 8 + 4) * size_of::<UuidProjection>())?);
            let mut statement = Statement::new(
                &store.connection,
                input,
                &native,
                c"SELECT DISTINCT operation_id FROM operation_journal_recording_chunks
                 WHERE operation_id>?1 ORDER BY operation_id LIMIT 8",
            )?;
            statement.bind_text(1, &after)?;
            let mut page = Vec::new();
            while statement.next()? {
                page.push(key(&statement, 0)?);
                #[cfg(test)]
                PAGE_CELLS.with(|n| n.set(n.get() + 1));
            }
            drop(statement);
            if page.is_empty() {
                drop(page_credit.take());
                break;
            }
            for text in page {
                let id = text.id.map_err(error)?;
                if id.is_nil() || !text.canonical {
                    return Err(error(
                        "restart recording operation identity is not canonical",
                    ));
                }
                let present = store.inspect_complete_claim_operation(
                    OperationId::from_uuid(id),
                    input,
                    |record, _| {
                        let Some(UniversalCommandArgs::Recording(proposal)) =
                            record.universal_command_args()
                        else {
                            return Err(error("operation has no recording proposal"));
                        };
                        if record.schema_version != SCHEMA_V17 {
                            return Err(error("recording schema mismatch"));
                        }
                        store.verify_recording_reservation_with_input(
                            proposal,
                            false,
                            Some(input),
                        )?;
                        operation(store, proposal, input, &native)
                    },
                )?;
                if !present {
                    return Err(error("recording operation absent"));
                }
                after = id.to_string();
            }
            drop(page_credit.take());
        }
        Ok(())
    })();
    if result.is_err() {
        input.retain_historical_error(working, page_credit, None, native.into_inner());
    }
    result
}
fn operation(
    store: &OperationJournalStore,
    proposal: &RecordingProposal,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
) -> Result<(), OperationJournalStoreError> {
    let mut page_credit = None;
    let result = (|| {
        let mut after = -1;
        let mut state = prefix::State::new();
        let mut key_buffer = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut key_buffer);
        loop {
            page_credit = Some(input.reserve_historical_scratch(
                (2 * 8 + 4) * size_of::<(UuidProjection, i64, i64)>(),
            )?);
            let mut statement = Statement::new(
                &store.connection,
                input,
                native,
                c"SELECT chunk_id,slot,sequence FROM operation_journal_recording_chunks
                 WHERE operation_id=?1 AND slot>?2 ORDER BY slot LIMIT 8",
            )?;
            statement.bind_text(1, operation)?;
            statement.bind_integer(2, after)?;
            let mut keys = Vec::new();
            while statement.next()? {
                keys.push((
                    key(&statement, 0)?,
                    scalar::<i64>(&statement, 1)?,
                    scalar::<i64>(&statement, 2)?,
                ));
                #[cfg(test)]
                PAGE_CELLS.with(|n| n.set(n.get() + 1));
            }
            drop(statement);
            if keys.is_empty() {
                drop(page_credit.take());
                break;
            }
            for (text, stored_slot, sequence) in keys {
                let id = text.id.map_err(error)?;
                if id.is_nil() || !text.canonical {
                    return Err(error("restart recording chunk identity is not canonical"));
                }
                sequence_unique(store, sequence, input, native)?;
                after = stored_slot;
                if (20000..=20003).contains(&stored_slot) {
                    continue;
                }
                let retained = read(store, id, proposal, input)?
                    .ok_or_else(|| error("restart recording chunk disappeared"))?;
                if retained.0 != sequence {
                    return Err(error("restart recording chunk sequence changed"));
                }
                state.observe(sequence, &retained.1, proposal)?;
            }
            drop(page_credit.take());
        }
        state.finish()
    })();
    if result.is_err() {
        if let Some(page) = page_credit {
            input.retain_historical_error(page, None, None, None);
        }
    }
    result
}
