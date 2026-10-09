//! Complete original chunk relations, using bounded key pages and one funded typed row.
use super::*;
use crate::operation_journal::store::{
    OperationJournalStore, initial_settlement_backing::statement,
};
use rusqlite::types::ValueRef;

fn invalid(reason: &'static str) -> OperationJournalStoreError {
    OperationJournalStoreError::ChunkClaim {
        reason: reason.into(),
    }
}

pub(in crate::operation_journal::store) fn validate(
    store: &OperationJournalStore,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let working = input.reserve_historical_scratch(32 * 1024)?;
    let result = (|| {
        if exceeds_row_limit(
            &store.connection,
            super::super::super::ordinary_claims::MAX_PAGE_ROWS,
            input,
        )? {
            return Err(invalid(
                "chunk-claim relation exceeds original bounded audit",
            ));
        }
        let keys_valid = statement(&store.connection, input,
            c"SELECT COALESCE(MIN(typeof(chunk_id)='text' AND length(CAST(chunk_id AS BLOB))=36),1) FROM operation_journal_chunk_claim",
            |row| {
                if !row.next()? { return Err(invalid("chunk audit shape observation absent")); }
                scalar::<bool>(row, 0)
            })?;
        if !keys_valid {
            return Err(invalid("chunk audit contains an unsupported key shape"));
        }
        let mut after = String::new();
        loop {
            let (keys, length) = statement(&store.connection, input,
                c"SELECT CASE WHEN typeof(chunk_id)='text' AND length(CAST(chunk_id AS BLOB))=36 THEN chunk_id ELSE NULL END FROM operation_journal_chunk_claim WHERE chunk_id>?1 ORDER BY chunk_id LIMIT 8",
                |row| {
                    row.bind_text(1, &after)?;
                    let mut keys = [None; 8];
                    let mut length = 0;
                    while row.next()? {
                        let ValueRef::Text(bytes) = row.get_ref(0)? else { return Err(invalid("chunk audit key shape changed")); };
                        let text = std::str::from_utf8(bytes).map_err(|_| invalid("chunk audit key is not UTF-8"))?;
                        let id = Uuid::parse_str(text).map_err(|_| invalid("chunk audit key is not a UUID"))?;
                        let mut canonical = [0; 36];
                        if id.is_nil() || id.hyphenated().encode_lower(&mut canonical) != text {
                            return Err(invalid("chunk audit key is not canonical"));
                        }
                        keys[length] = Some(ChunkId(id));
                        length += 1;
                    }
                    Ok((keys, length))
                })?;
            if length == 0 {
                break;
            }
            for key in keys.into_iter().take(length).flatten() {
                let funded = read(&store.connection, key, input)?
                    .ok_or_else(|| invalid("chunk disappeared from original audit snapshot"))?;
                let chunk = &funded.record;
                let claim = chunk.claim_id().to_string();
                let operation = chunk.operation_id().to_string();
                let device = chunk.volume().device.to_string();
                let bytes = i64::try_from(chunk.allocated_bytes())
                    .map_err(|_| invalid("chunk charge exceeds SQLite range"))?;
                let valid = statement(&store.connection, input,
                    c"SELECT EXISTS(SELECT 1 FROM operation_journal_ordinary_claim WHERE claim_id=?1 AND operation_id=?2 AND volume_device=?3 AND allocation_class=?4 AND allocated_bytes=?5)",
                    |row| {
                        row.bind_text(1, &claim)?; row.bind_text(2, &operation)?;
                        row.bind_text(3, &device)?; row.bind_text(4, allocation_class_text(chunk.allocation_class()))?;
                        row.bind_integer(5, bytes)?;
                        if !row.next()? { return Err(invalid("chunk ordinary comparison absent")); }
                        scalar::<bool>(row, 0)
                    })?;
                if !valid {
                    return Err(invalid("chunk differs from original ordinary claim"));
                }
                let present = store.inspect_complete_claim_operation(
                    chunk.operation_id(),
                    input,
                    |record, _| {
                        let present = record.universal_intent.as_ref().is_some_and(|intent| {
                            intent
                                .participants()
                                .descriptors()
                                .iter()
                                .any(|participant| participant.id() == chunk.participant_id())
                        });
                        if !present {
                            return Err(invalid(
                                "chunk participant differs from original operation",
                            ));
                        }
                        Ok(())
                    },
                )?;
                if !present {
                    return Err(invalid("chunk operation absent"));
                }
                let release = chunk.release_id().unwrap_or_default().to_string();
                let (released, matches) = statement(&store.connection, input,
                    c"SELECT EXISTS(SELECT 1 FROM operation_journal_ordinary_claim_release WHERE claim_id=?1), EXISTS(SELECT 1 FROM operation_journal_ordinary_claim_release WHERE claim_id=?1 AND operation_id=?2 AND release_id=?3)",
                    |row| {
                        row.bind_text(1, &claim)?; row.bind_text(2, &operation)?; row.bind_text(3, &release)?;
                        if !row.next()? { return Err(invalid("chunk release comparison absent")); }
                        Ok((scalar::<bool>(row, 0)?, scalar::<bool>(row, 1)?))
                    })?;
                match chunk.state() {
                    ChunkClaimState::Released if !matches => {
                        return Err(invalid("released chunk lacks exact ordinary release"));
                    }
                    ChunkClaimState::Claimed | ChunkClaimState::Issued if released => {
                        return Err(invalid("active chunk has ordinary release evidence"));
                    }
                    _ => {}
                }
                after = key.0.to_string();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        input.retain_historical_error(working, None, None, None);
    }
    result
}
