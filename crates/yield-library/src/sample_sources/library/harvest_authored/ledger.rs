//! Fixed-budget immutable receipt readback and retention; exact replay comes first.
use super::*;
use rusqlite::{Connection, params};
fn malformed() -> LibraryError {
    LibraryError::HarvestAuthoredMalformedReceipt
}
fn digest(payload: &[u8], result: &[u8], owner: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"yield.harvest-authored.receipt.v1\0");
    h.update(&(payload.len() as u64).to_le_bytes());
    h.update(payload);
    h.update(&(result.len() as u64).to_le_bytes());
    h.update(result);
    h.update(owner.as_bytes());
    *h.finalize().as_bytes()
}
const SHAPE: &str = "typeof(sequence)='integer' AND sequence>0 AND typeof(command_key)='text' AND length(CAST(command_key AS BLOB)) BETWEEN 1 AND 512 AND typeof(payload)='blob' AND length(payload) BETWEEN 1 AND 65536 AND typeof(result)='blob' AND length(result) BETWEEN 1 AND 65536 AND typeof(identity)='blob' AND length(identity)=32 AND typeof(owner)='text' AND length(CAST(owner AS BLOB))=36";
pub(super) fn read(
    c: &Connection,
    request: &HarvestAuthoredRequest,
) -> Result<Option<HarvestAuthoredReceipt>, LibraryError> {
    let key = request.payload.key()?;
    read_stored(c, &key, Some(&request.bytes)).map(|stored| stored.map(|(receipt, _)| receipt))
}
pub(super) fn read_stored(
    c: &Connection,
    key: &str,
    expected: Option<&[u8]>,
) -> Result<Option<(HarvestAuthoredReceipt, Vec<u8>)>, LibraryError> {
    let count: i64 = c.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM harvest_authored_receipts WHERE command_key=?1 LIMIT 2)",
        [key],
        |r| r.get(0),
    )?;
    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(malformed());
    }
    let valid: bool = c.query_row(
        &format!("SELECT ({SHAPE}) FROM harvest_authored_receipts WHERE command_key=?1"),
        [key],
        |r| r.get(0),
    )?;
    if !valid {
        return Err(malformed());
    }
    // The transaction holds the single writer; preflight and readback share one snapshot.
    let (sequence,payload,result,identity,owner):(i64,Vec<u8>,Vec<u8>,Vec<u8>,String)=c.query_row("SELECT sequence,payload,result,identity,owner FROM harvest_authored_receipts WHERE command_key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let unique: i64 = c.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM harvest_authored_receipts WHERE sequence=?1 LIMIT 2)",
        [sequence],
        |r| r.get(0),
    )?;
    if unique != 1 {
        return Err(malformed());
    }
    if identity != digest(&payload, &result, &owner) {
        return Err(malformed());
    }
    let decoded = Payload::decode(&payload).map_err(|_| malformed())?;
    // Original replay classifies valid changed intent before historical key correlation.
    if expected.is_some_and(|bytes| payload != bytes) {
        return Err(LibraryError::HarvestAuthoredReplayConflict);
    }
    if decoded.key().map_err(|_| malformed())? != key {
        return Err(malformed());
    }
    let owner = Uuid::parse_str(&owner)
        .ok()
        .filter(|o| !o.is_nil())
        .ok_or_else(malformed)?;
    let outcome: Outcome = serde_json::from_slice(&result).map_err(|_| malformed())?;
    if outcome.version != 1 || outcome.created < 0 || bounded(&outcome)? != result {
        return Err(malformed());
    }
    Ok(Some((
        HarvestAuthoredReceipt {
            payload: decoded,
            outcome,
            sequence: sequence as u64,
            owner,
        },
        payload,
    )))
}
pub(super) fn capacity(
    c: &Connection,
    request: &HarvestAuthoredRequest,
) -> Result<(), LibraryError> {
    let count: i64 = c.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM harvest_authored_receipts LIMIT 8192)",
        [],
        |r| r.get(0),
    )?;
    if count >= HARVEST_AUTHORED_MAX_RECEIPTS {
        return Err(LibraryError::HarvestAuthoredReceiptCapacity);
    }
    let invalid: bool = c.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM harvest_authored_receipts WHERE NOT ({SHAPE}))"),
        [],
        |r| r.get(0),
    )?;
    if invalid {
        return Err(malformed());
    }
    let bytes:i64=c.query_row("SELECT COALESCE(SUM(length(CAST(command_key AS BLOB))+length(payload)+length(result)+length(identity)+length(CAST(owner AS BLOB))+8),0) FROM harvest_authored_receipts",[],|r|r.get(0))?;
    // Outcome's fixed encoding fits 256 bytes; reserve it before any graph mutation.
    let additional = request.bytes.len() + request.payload.key()?.len() + 256 + 32 + 36 + 8;
    if bytes < 0 || bytes > HARVEST_AUTHORED_MAX_BYTES - additional as i64 {
        return Err(LibraryError::HarvestAuthoredReceiptCapacity);
    }
    Ok(())
}
pub(super) fn insert(
    c: &Connection,
    request: &HarvestAuthoredRequest,
    outcome: &Outcome,
) -> Result<HarvestAuthoredReceipt, LibraryError> {
    let result = bounded(outcome)?;
    let owner = request.owner.to_string();
    c.execute("INSERT INTO harvest_authored_receipts(command_key,payload,result,identity,owner) VALUES(?1,?2,?3,?4,?5)",params![request.payload.key()?,request.bytes,result,digest(&request.bytes,&result,&owner).as_slice(),owner])?;
    read(c, request)?.ok_or_else(malformed)
}
