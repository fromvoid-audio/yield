//! Same immutable original inventory, borrowed SQL cells and producer-owned loans.
use super::*;
use crate::operation_journal::codec::historical_observation::FIXED_BYTES;
use crate::operation_journal::owner::InputPermit;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::Statement;
use rusqlite::types::ValueRef;
use std::{cell::RefCell, mem::size_of};

pub(in crate::operation_journal::store) struct Inventory {
    value: DatabaseClaimInventory,
    reconstruction: Option<(usize, usize)>,
    // Last: all original decoded allocations die before their exact loans.
    _sql: Option<InputPermit>,
    _syntax: Option<InputPermit>,
    _decode: Option<InputPermit>,
}
impl Inventory {
    // Original canonical root wire and transient decode/syntax bounds travel
    // with this same retained value. Scalars confer no source or grant authority.
    pub(in crate::operation_journal::store) fn reconstruction_backing(
        &self,
    ) -> Option<(usize, usize)> {
        self.reconstruction
    }
}

impl std::ops::Deref for Inventory {
    type Target = DatabaseClaimInventory;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

pub(in crate::operation_journal::store) fn read_with_input(
    connection: &Connection,
    operation: OperationId,
    input: Option<&InputPermit>,
) -> Result<Option<Inventory>, OperationJournalStoreError> {
    match input {
        Some(input) => read(connection, operation, input),
        None => super::read(connection, operation).map(|value| {
            value.map(|value| Inventory {
                value,
                reconstruction: None,
                _sql: None,
                _syntax: None,
                _decode: None,
            })
        }),
    }
}

fn working_bytes() -> usize {
    FIXED_BYTES
        + size_of::<Inventory>()
        + 3 * size_of::<Statement<'static>>()
        + size_of::<RefCell<Option<InputPermit>>>()
        + 4 * size_of::<InputPermit>()
        + 8 * (36 + size_of::<String>())
        + 2 * 32
}

pub(in crate::operation_journal::store) fn read(
    connection: &Connection,
    operation: OperationId,
    input: &InputPermit,
) -> Result<Option<Inventory>, OperationJournalStoreError> {
    let sql = input.reserve_historical_scratch(working_bytes())?;
    let mut syntax = None;
    let mut decoding = None;
    let mut reconstruction = None;
    let native = RefCell::new(None);
    let result = (|| {
        let mut key = [0; 36];
        let key = operation.as_uuid().hyphenated().encode_lower(&mut key);
        let mut statement = Statement::new(connection, input, &native,
            c"SELECT CASE WHEN typeof(inventory)='blob' AND length(inventory) BETWEEN 1 AND ?2 THEN inventory END FROM operation_journal_database_binding WHERE operation_id=?1 AND ordinal=0")?;
        statement.bind_text(1, key)?;
        statement.bind_integer(2, MAX_INVENTORY_BYTES as i64)?;
        if !statement.next()? {
            drop(statement);
            let mut present = Statement::new(connection, input, &native,
                c"SELECT EXISTS(SELECT 1 FROM operation_journal_database_binding WHERE operation_id=?1) OR EXISTS(SELECT 1 FROM operation_journal_database_claim_slice WHERE operation_id=?1)")?;
            present.bind_text(1, key)?;
            if !present.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            match present.get_ref(0)? {
                ValueRef::Integer(0) => return Ok(None),
                ValueRef::Integer(_) => {
                    return Err(error("missing root binding retains unsupported evidence"));
                }
                _ => return Err(error("binding presence is not a SQLite integer")),
            }
        }
        let ValueRef::Blob(bytes) = statement.get_ref(0)? else {
            return Err(error("root encoding type/length"));
        };
        // Before syntax validation or any owned binary/JSON decoder. These are
        // original borrowed bytes, with no second SELECT or Vec BLOB copy.
        let syntax_bytes = bytes
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(FIXED_BYTES))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        syntax = Some(input.reserve_historical_scratch(syntax_bytes)?);
        let bytes_needed = crate::operation_journal::database_claims::decode_backing_bytes(bytes)
            .or_else(|_| {
                crate::operation_journal::database_claims::partial_decode_backing_bytes(bytes.len())
            })
            .map_err(error)?;
        decoding = Some(input.reserve_historical_scratch(bytes_needed)?);
        reconstruction = Some((
            bytes.len(),
            syntax_bytes
                .checked_add(bytes_needed)
                .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
        ));
        let value = DatabaseClaimInventory::decode(bytes).map_err(error)?;
        drop(statement);
        verify(connection, operation, &value, input, &native)?;
        Ok(Some(value))
    })();
    match result {
        Ok(Some(value)) => Ok(Some(Inventory {
            value,
            reconstruction,
            _sql: Some(sql),
            _syntax: syntax,
            _decode: decoding,
        })),
        Ok(None) => Ok(None),
        Err(error) => {
            input.retain_historical_error(sql, syntax, decoding, native.into_inner());
            Err(error)
        }
    }
}

fn uuid_text(value: ValueRef<'_>, expected: Uuid) -> bool {
    let mut buffer = [0; 36];
    value == ValueRef::Text(expected.hyphenated().encode_lower(&mut buffer).as_bytes())
}

fn verify(
    connection: &Connection,
    operation: OperationId,
    inventory: &DatabaseClaimInventory,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
) -> Result<(), OperationJournalStoreError> {
    inventory.validate().map_err(error)?;
    let expected = inventory.encode().map_err(error)?;
    let digest = inventory.digest().map_err(error)?;
    let mut key = [0; 36];
    let key = operation.as_uuid().hyphenated().encode_lower(&mut key);
    let mut bindings = Statement::new(connection, input, native,
        c"SELECT CASE WHEN length(binding_id)=36 THEN binding_id END,ordinal,role,CASE WHEN length(volume)<=20 THEN volume END,CASE WHEN length(CAST(main_file AS BLOB))<=256 THEN main_file END,CASE WHEN length(decision_nonce)=36 THEN decision_nonce END,CASE WHEN typeof(inventory_digest)='blob' AND length(inventory_digest)=32 THEN inventory_digest END,CASE WHEN typeof(inventory)='blob' AND length(inventory)<=65536 THEN inventory END FROM operation_journal_database_binding WHERE operation_id=?1 ORDER BY ordinal LIMIT 65")?;
    bindings.bind_text(1, key)?;
    for (ordinal, binding) in inventory.bindings().iter().enumerate() {
        if !bindings.next()? {
            return Err(error("missing binding row"));
        }
        let volume = binding.volume.to_string();
        let same = uuid_text(bindings.get_ref(0)?, binding.id)
            && bindings.get_ref(1)? == ValueRef::Integer(ordinal as i64)
            && bindings.get_ref(2)? == ValueRef::Integer(binding.role as i64)
            && bindings.get_ref(3)? == ValueRef::Text(volume.as_bytes())
            && bindings.get_ref(4)? == ValueRef::Text(binding.main_file.as_bytes())
            && uuid_text(bindings.get_ref(5)?, inventory.header().decision_nonce)
            && bindings.get_ref(6)? == ValueRef::Blob(digest.as_slice())
            && bindings.get_ref(7)?
                == if ordinal == 0 {
                    ValueRef::Blob(&expected)
                } else {
                    ValueRef::Null
                };
        if !same {
            return Err(error("binding identity conflict"));
        }
    }
    if bindings.next()? {
        return Err(error("extra binding row"));
    }
    drop(bindings);
    let mut slices = Statement::new(connection, input, native,
        c"SELECT CASE WHEN length(slice_id)=36 THEN slice_id END,CASE WHEN length(binding_id)=36 THEN binding_id END,CASE WHEN length(claim_id)=36 THEN claim_id END,CASE WHEN length(participant_id)=36 THEN participant_id END,direction,class,ceiling,wal_ceiling,shm_ceiling FROM operation_journal_database_claim_slice WHERE operation_id=?1 ORDER BY slice_id LIMIT 257")?;
    slices.bind_text(1, key)?;
    // Original persisted order is slice_id; the immutable inventory has its own
    // canonical claim/binding/participant ordering. Keep that exact comparison.
    let mut ordered = inventory.slices().iter().collect::<Vec<_>>();
    ordered.sort_by_key(|slice| slice.id);
    for slice in ordered {
        if !slices.next()? {
            return Err(error("missing slice row"));
        }
        for (index, id) in [slice.id, slice.binding, slice.claim, slice.participant]
            .into_iter()
            .enumerate()
        {
            if !uuid_text(slices.get_ref(index as i32)?, id) {
                return Err(error("slice identity conflict"));
            }
        }
        for (index, value) in [
            slice.direction as i64,
            class_tag(slice.class),
            slice.ceiling as i64,
            slice.wal_ceiling as i64,
            slice.shm_ceiling as i64,
        ]
        .into_iter()
        .enumerate()
        {
            if slices.get_ref((index + 4) as i32)? != ValueRef::Integer(value) {
                return Err(error("slice allocation/direction conflict"));
            }
        }
    }
    if slices.next()? {
        return Err(error("extra slice row"));
    }
    Ok(())
}
