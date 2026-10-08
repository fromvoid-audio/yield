//! Original scalar/payload boundary and closed captured byte decoder.
use super::*;
use crate::operation_journal::codec::historical_observation;

pub(in crate::operation_journal::store) struct Chunk {
    value: (i64, RecordingCapturedChunk),
    // Last: actual decoded byte allocations precede every refund.
    _working: InputPermit,
    _syntax: Option<InputPermit>,
    _producer: Option<InputPermit>,
}
impl std::ops::Deref for Chunk {
    type Target = (i64, RecordingCapturedChunk);
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
pub(in crate::operation_journal::store) fn read(
    store: &OperationJournalStore,
    id: Uuid,
    proposal: &RecordingProposal,
    input: &InputPermit,
) -> Result<Option<Chunk>, OperationJournalStoreError> {
    let working = input.reserve_historical_scratch(working_bytes(store)?)?;
    let native = RefCell::new(None);
    let mut syntax = None;
    let mut producer = None;
    let result = (|| {
        let mut key_buffer = [0; 36];
        let id_text = id.hyphenated().encode_lower(&mut key_buffer);
        let mut row=Statement::new(&store.connection,input,&native,c"SELECT CASE WHEN typeof(operation_id)='text' AND length(operation_id)=36 THEN operation_id ELSE NULL END, slot, sequence, typeof(record), length(record), length(chunk_id) FROM operation_journal_recording_chunks WHERE chunk_id=?1")?;
        row.bind_text(1, id_text)?;
        if !row.next()? {
            return Ok(None);
        }
        let operation = key(&row, 0)?;
        let stored_slot = scalar::<i64>(&row, 1)?;
        let sequence = scalar::<i64>(&row, 2)?;
        let kind = text(&row, 3)?;
        let length = scalar::<i64>(&row, 4)?;
        let key_length = scalar::<i64>(&row, 5)?;
        if !operation.canonical
            || !operation
                .id
                .as_ref()
                .is_ok_and(|id| *id == proposal.operation_id().as_uuid())
            || sequence <= 0
            || kind != "blob"
            || !(1..=131072).contains(&length)
            || key_length != 36
        {
            return Err(error("retained recording row shape/binding"));
        }
        drop(row);
        #[cfg(test)]
        payload_read_boundary(PayloadReader::Chunk, &store.connection);
        // Preserve the original second query and its independently bounded projection.
        let mut payload=Statement::new(&store.connection,input,&native,c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 0 AND 131072 THEN record ELSE NULL END AS record, typeof(record) FROM operation_journal_recording_chunks WHERE chunk_id=?1")?;
        payload.bind_text(1, id_text)?;
        if !payload.next()? {
            return Err(rusqlite::Error::QueryReturnedNoRows.into());
        }
        let bytes = match payload.get_ref(0)? {
            ValueRef::Blob(bytes) => Some(bytes),
            ValueRef::Null => None,
            _ => {
                scalar::<Option<Vec<u8>>>(&payload, 0)?;
                unreachable!("projection only yields BLOB or NULL")
            }
        };
        let kind = text(&payload, 1)?;
        if kind != "blob" {
            return Err(recording_payload_type_error(kind));
        }
        let bytes = bytes
            .ok_or_else(|| error("invalid recording observation: encoded value exceeds bound"))?;
        syntax = Some(
            input.reserve_historical_scratch(checked(
                bytes
                    .len()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(FIXED_BYTES)),
            )?)?,
        );
        let (strings, entries, arrays) = historical_observation::closed_streaming_shape(bytes)
            .unwrap_or((bytes.len(), bytes.len(), 1));
        let encoded = command_backing::serialized_length(proposal)?;
        // No Value/Content tree or chunk canonicalization. The unchanged typed
        // byte Vec, serde error strings and original proposal hash encoder coexist.
        let required = checked(
            entries
                .checked_mul(2)
                .and_then(|n| arrays.checked_mul(8).and_then(|a| n.checked_add(a)))
                .and_then(|n| strings.checked_mul(12).and_then(|s| n.checked_add(s)))
                .and_then(|n| {
                    encoded
                        .max(4096)
                        .checked_mul(2)
                        .and_then(|e| n.checked_add(e))
                })
                .and_then(|n| n.checked_add(size_of::<RecordingCapturedChunk>())),
        )?;
        producer = Some(input.reserve_historical_scratch(required)?);
        let chunk = RecordingCapturedChunk::decode(bytes, proposal).map_err(error)?;
        if chunk.chunk_id() != id || slot(chunk.role()) != stored_slot {
            return Err(error("recording row slot/key differs from immutable bytes"));
        }
        Ok(Some((sequence, chunk)))
    })();
    match result {
        Ok(Some(value)) => Ok(Some(Chunk {
            value,
            _working: working,
            _syntax: syntax,
            _producer: producer,
        })),
        Ok(None) => Ok(None),
        Err(error) => {
            input.retain_historical_error(working, syntax, producer, native.into_inner());
            Err(error)
        }
    }
}
