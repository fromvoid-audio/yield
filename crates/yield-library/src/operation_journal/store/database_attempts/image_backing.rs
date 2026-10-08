//! Original borrowed images, with prepaid producers retained by their values.
use super::*;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::{
    Statement, scalar,
};
use crate::operation_journal::{codec::historical_observation, owner::InputPermit};
use rusqlite::types::ValueRef;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::{cell::RefCell, mem::size_of};

pub(in crate::operation_journal::store) struct Retained<T> {
    value: T,
    // Passive reconstruction counts only; no dispatch or settlement authority.
    reconstruction: Option<(usize, usize)>,
    // Original data must die before every producer loan.
    _working: Option<InputPermit>,
    _syntax: Option<InputPermit>,
    _producer: Option<InputPermit>,
}
impl<T> std::ops::Deref for Retained<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
fn checked(n: Option<usize>) -> Result<usize> {
    n.ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)
}
fn working_bytes() -> usize {
    historical_observation::FIXED_BYTES
        + size_of::<Retained<DatabaseAttemptStart>>()
        + size_of::<Retained<DatabaseAttemptOutcome>>()
        + crate::operation_journal::database_claims::MAX_INVENTORY_BYTES
        + 1
        + 2 * size_of::<Statement<'static>>()
        + 6 * size_of::<UuidProjection>()
        + 4 * size_of::<InputPermit>()
        + 2048
}
fn uuid(row: &Statement<'_>, index: i32) -> Result<UuidProjection> {
    if let ValueRef::Text(bytes) = row.get_ref(index)? {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(UuidProjection::new(text));
        }
    }
    scalar::<String>(row, index).map(|s| UuidProjection::new(&s))
}
fn blob<'a>(row: &'a Statement<'_>, index: i32) -> Result<Option<&'a [u8]>> {
    match row.get_ref(index)? {
        ValueRef::Blob(bytes) => Ok(Some(bytes)),
        ValueRef::Null => Ok(None),
        _ => scalar::<Option<Vec<u8>>>(row, index).map(|_| None),
    }
}

// Observe the original bounded byte stream on a fixed stack, before owning the
// typed start. Unknown fields are skipped; these counters never validate a start.
fn inventory_backing(bytes: &[u8]) -> Option<(usize, usize)> {
    struct KeyVisitor;
    impl<'de> Visitor<'de> for KeyVisitor {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("field name")
        }
        fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<bool, E> {
            Ok(s == "inventory")
        }
    }
    struct Bytes;
    impl<'de> DeserializeSeed<'de> for Bytes {
        type Value = (usize, usize);
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<(usize, usize), D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de> Visitor<'de> for Bytes {
        type Value = (usize, usize);
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("byte sequence")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut a: A,
        ) -> std::result::Result<(usize, usize), A::Error> {
            const CAP: usize = crate::operation_journal::database_claims::MAX_INVENTORY_BYTES;
            let mut data = [0; CAP + 1];
            let mut length = 0;
            while let Some(byte) = a.next_element::<u8>()? {
                if length < data.len() {
                    data[length] = byte;
                    length += 1;
                }
            }
            // Overflow cannot reach the original embedded inventory decoder.
            if length > CAP {
                return Ok((length, 0));
            }
            crate::operation_journal::database_claims::decode_backing_bytes(&data[..length])
                .or_else(|_| {
                    crate::operation_journal::database_claims::partial_decode_backing_bytes(length)
                })
                .map(|backing| (length, backing))
                .map_err(serde::de::Error::custom)
        }
    }
    struct Envelope;
    impl<'de> Visitor<'de> for Envelope {
        type Value = (usize, usize);
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("attempt envelope")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut a: A,
        ) -> std::result::Result<(usize, usize), A::Error> {
            struct Field(bool);
            impl<'de> serde::Deserialize<'de> for Field {
                fn deserialize<D: serde::Deserializer<'de>>(
                    d: D,
                ) -> std::result::Result<Self, D::Error> {
                    d.deserialize_identifier(KeyVisitor).map(Field)
                }
            }
            let mut backing = (0usize, 0usize);
            while let Some(Field(inventory)) = a.next_key()? {
                if inventory {
                    let (length, decode) = a.next_value_seed(Bytes)?;
                    backing.0 = backing.0.max(length);
                    backing.1 = backing.1.max(decode);
                } else {
                    a.next_value::<IgnoredAny>()?;
                }
            }
            Ok(backing)
        }
    }
    use serde::Deserializer;
    let mut d = serde_json::Deserializer::from_slice(bytes);
    d.deserialize_map(Envelope).ok()
}
fn output_bytes(wire: usize, cap: usize) -> Result<usize> {
    // JSON normalization can expand numeric spelling; six wire bytes cover
    // each original scalar spelling. The original capped serializer stays capped.
    checked(
        wire.checked_mul(6)
            .map(|n| n.min(cap).max(4096))
            .and_then(|n| n.checked_mul(2)),
    )
}

pub(in crate::operation_journal::store) fn read_start(
    c: &Connection,
    id: Uuid,
    input: Option<&InputPermit>,
) -> Result<Option<Retained<DatabaseAttemptStart>>> {
    let Some(input) = input else {
        return super::read_start(c, id).map(|v| {
            v.map(|value| Retained {
                value,
                reconstruction: None,
                _working: None,
                _syntax: None,
                _producer: None,
            })
        });
    };
    let working = input.reserve_historical_scratch(working_bytes())?;
    let native = RefCell::new(None);
    let mut syntax = None;
    let mut producer = None;
    let mut reconstruction = None;
    let result = (|| {
        let mut row = Statement::new(c, input, &native, c"SELECT CASE WHEN length(operation_id)=36 THEN operation_id END,
         CASE WHEN length(binding_id)=36 THEN binding_id END,CASE WHEN length(start_nonce)=36 THEN start_nonce END,
         CASE WHEN length(database_slice)=36 THEN database_slice END,CASE WHEN length(sidecar_slice)=36 THEN sidecar_slice END,
         database_bytes,wal_bytes,shm_bytes,
         CASE WHEN typeof(image)='blob' AND length(image) BETWEEN 1 AND ?2 THEN image END
         FROM operation_journal_database_attempt WHERE attempt_id=?1")?;
        let mut key = [0; 36];
        row.bind_text(1, id.hyphenated().encode_lower(&mut key))?;
        row.bind_integer(2, MAX_START_BYTES as i64)?;
        if !row.next()? {
            return Ok(None);
        }
        let op = uuid(&row, 0)?;
        let binding = uuid(&row, 1)?;
        let nonce = uuid(&row, 2)?;
        let db = uuid(&row, 3)?;
        let sidecar = uuid(&row, 4)?;
        let db_bytes = scalar::<u64>(&row, 5)?;
        let wal = scalar::<u64>(&row, 6)?;
        let shm = scalar::<u64>(&row, 7)?;
        let bytes = blob(&row, 8)?.ok_or_else(|| error("start image type/length"))?;
        // Includes parser growth and original unknown-field/type diagnostic
        // formatting in both the counter and unchanged typed serde decoder.
        syntax = Some(
            input.reserve_historical_scratch(checked(
                bytes
                    .len()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(historical_observation::FIXED_BYTES)),
            )?)?,
        );
        let (graph, strings, expansion) = match historical_observation::attempt_graph_backing(bytes)
        {
            Some(shape) => shape,
            None => (
                checked(bytes.len().checked_mul(
                    12 * size_of::<(String, serde_json::Value)>() + 32 * size_of::<usize>(),
                ))?,
                bytes.len(),
                bytes.len().saturating_mul(6),
            ),
        };
        // Prepay dynamic serde counter/type reasons before inspecting byte elements.
        let reasons = if strings == 0 {
            None
        } else {
            Some(input.reserve_historical_scratch(checked(strings.checked_mul(12))?)?)
        };
        let (inventory_length, decode) = match inventory_backing(bytes) {
            Some(shape) => shape,
            // This counter accepts all unknown fields and uses the original u8
            // element/depth rules. Failure cannot reach the original subsequent
            // embedded binary decoder: typed parse or canonical shape fails first.
            None => (
                bytes
                    .len()
                    .min(crate::operation_journal::database_claims::MAX_INVENTORY_BYTES + 1),
                0,
            ),
        };
        let output = checked(
            bytes
                .len()
                .checked_add(expansion)
                .map(|n| n.min(MAX_START_BYTES).max(4096))
                .and_then(|n| n.checked_mul(2)),
        )?;
        producer = Some(
            input.reserve_historical_scratch(checked(
                graph
                    .checked_add(decode)
                    .and_then(|n| {
                        inventory_length
                            .checked_mul(2)
                            .and_then(|b| n.checked_add(b + 16))
                    })
                    .and_then(|n| output.checked_mul(2).and_then(|b| n.checked_add(b))),
            )?)?,
        );
        // Keep the selected reason loan inside the original retained syntax owner,
        // without allocating a second carrier or separating it from the producer.
        let syntax_owner = syntax.as_mut().unwrap();
        if let Some(reasons) = reasons {
            syntax_owner.absorb_historical_scratch(reasons);
        }
        let start = DatabaseAttemptStart::decode(bytes).map_err(error)?;
        if start.attempt != id
            || !op.matches(start.operation)
            || !binding.matches(start.binding)
            || !nonce.matches(start.nonce)
            || !db.matches(start.database_slice)
            || !sidecar.matches(start.sidecar_slice)
            || start.database_bytes != db_bytes
            || start.wal_bytes != wal
            || start.shm_bytes != shm
        {
            return Err(error("start columns differ from canonical image"));
        }
        reconstruction = Some((output, decode));
        Ok(Some(start))
    })();
    match result {
        Ok(Some(value)) => Ok(Some(Retained {
            value,
            reconstruction,
            _working: Some(working),
            _syntax: syntax,
            _producer: producer,
        })),
        Ok(None) => Ok(None),
        Err(e) => {
            input.retain_historical_error(working, syntax, producer, native.into_inner());
            Err(e)
        }
    }
}

pub(in crate::operation_journal::store) fn read_outcome(
    c: &Connection,
    start: &Retained<DatabaseAttemptStart>,
    input: Option<&InputPermit>,
) -> Result<Option<Retained<DatabaseAttemptOutcome>>> {
    let Some(input) = input else {
        return super::read_outcome(c, start).map(|v| {
            v.map(|value| Retained {
                value,
                reconstruction: None,
                _working: None,
                _syntax: None,
                _producer: None,
            })
        });
    };
    let working = input.reserve_historical_scratch(working_bytes())?;
    let native = RefCell::new(None);
    let mut syntax = None;
    let mut producer = None;
    let result = (|| {
        let mut row = Statement::new(
            c,
            input,
            &native,
            c"SELECT CASE WHEN length(operation_id)=36 THEN operation_id END,
         CASE WHEN length(observation_id)=36 THEN observation_id END,
         CASE WHEN typeof(start_digest)='blob' AND length(start_digest)=32 THEN start_digest END,
         CASE WHEN typeof(image)='blob' AND length(image) BETWEEN 1 AND ?2 THEN image END
         FROM operation_journal_database_attempt_outcome WHERE attempt_id=?1",
        )?;
        let mut key = [0; 36];
        row.bind_text(1, start.attempt.hyphenated().encode_lower(&mut key))?;
        row.bind_integer(2, MAX_OUTCOME_BYTES as i64)?;
        if !row.next()? {
            return Ok(None);
        }
        let op = uuid(&row, 0)?;
        let observation = uuid(&row, 1)?;
        let digest = blob(&row, 2)?;
        let bytes = blob(&row, 3)?.ok_or_else(|| error("outcome image type/length"))?;
        syntax = Some(
            input.reserve_historical_scratch(checked(
                bytes
                    .len()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(historical_observation::FIXED_BYTES)),
            )?)?,
        );
        let strings = historical_observation::closed_streaming_shape(bytes)
            .map_or(bytes.len(), |shape| shape.0);
        let (start_output, start_decode) = start
            .reconstruction
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        producer = Some(
            input.reserve_historical_scratch(checked(
                output_bytes(bytes.len(), MAX_OUTCOME_BYTES)?
                    .checked_add(start_output)
                    .and_then(|n| n.checked_add(start_decode))
                    .and_then(|n| strings.checked_mul(12).and_then(|s| n.checked_add(s))),
            )?)?,
        );
        let outcome = DatabaseAttemptOutcome::decode(bytes).map_err(error)?;
        outcome.validate(start).map_err(error)?;
        if !op.matches(start.operation)
            || !observation.matches(outcome.observation)
            || digest != Some(outcome.start_digest.as_slice())
        {
            return Err(error("outcome columns differ from image"));
        }
        Ok(Some(outcome))
    })();
    match result {
        Ok(Some(value)) => Ok(Some(Retained {
            value,
            reconstruction: None,
            _working: Some(working),
            _syntax: syntax,
            _producer: producer,
        })),
        Ok(None) => Ok(None),
        Err(e) => {
            input.retain_historical_error(working, syntax, producer, native.into_inner());
            Err(e)
        }
    }
}
