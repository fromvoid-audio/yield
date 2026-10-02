//! Bounded immutable reference receipt validation and observation types.
use super::*;
use rusqlite::Connection;
/// Retained terminal metadata disposition; no file absence or journal completion assertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceDisposition {
    /// Exact old state/endpoints transferred.
    Applied,
    /// Snapshot content changed.
    ReferenceChanged,
    /// No old state or endpoints.
    ReferenceMissing,
    /// Destination has independent state/endpoints.
    DestinationOccupied,
}
impl ReferenceDisposition {
    pub(super) fn spelling(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::ReferenceChanged => "changed",
            Self::ReferenceMissing => "missing",
            Self::DestinationOccupied => "occupied",
        }
    }
    fn parse(s: &str) -> Result<Self, LibraryError> {
        match s {
            "applied" => Ok(Self::Applied),
            "changed" => Ok(Self::ReferenceChanged),
            "missing" => Ok(Self::ReferenceMissing),
            "occupied" => Ok(Self::DestinationOccupied),
            _ => Err(LibraryError::ReferenceMalformedReceipt),
        }
    }
}
/// Immutable observation returned after commit/readback. Not VerifiedStepCompletion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceReceipt {
    payload: Payload,
    identity: [u8; 32],
    sequence: u64,
    owner: Uuid,
    disposition: ReferenceDisposition,
}
impl ReferenceReceipt {
    /// Correlated operation.
    pub fn operation_id(&self) -> OperationId {
        self.payload.operation
    }
    /// Original frozen work key.
    pub fn work_key(&self) -> WorkUnitKey {
        self.payload.work
    }
    /// Terminal metadata result.
    pub fn disposition(&self) -> ReferenceDisposition {
        self.disposition
    }
    /// Immutable ledger sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Historical profile acquisition.
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
    /// Canonical proposal fingerprint.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.identity
    }
}
/// Commit or exact replay of the original immutable result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferenceRekeyResult {
    /// A new disposition committed.
    Committed(ReferenceReceipt),
    /// Existing disposition observed without new metadata effect.
    Replayed(ReferenceReceipt),
}
impl ReferenceRekeyResult {
    /// Borrow the immutable disposition.
    pub fn receipt(&self) -> &ReferenceReceipt {
        match self {
            Self::Committed(r) | Self::Replayed(r) => r,
        }
    }
}
pub(super) fn read_receipt(
    connection: &Connection,
    request: &ReferenceRekeyRequest,
) -> Result<Option<ReferenceReceipt>, LibraryError> {
    let key = request.payload.key()?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM reference_receipts WHERE command_key=?1 LIMIT 2)",
        [&key],
        |r| r.get(0),
    )?;
    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(LibraryError::ReferenceMalformedReceipt);
    }
    let (seq,bytes,hash,owner,disposition):(i64,Option<Vec<u8>>,Option<Vec<u8>>,Option<String>,Option<String>)=connection.query_row("SELECT sequence,CASE WHEN typeof(payload)='blob' AND length(payload) BETWEEN 1 AND 65536 THEN payload END,CASE WHEN typeof(payload_identity)='blob' AND length(payload_identity)=32 THEN payload_identity END,CASE WHEN typeof(owner_identity)='text' AND length(CAST(owner_identity AS BLOB))=36 THEN owner_identity END,CASE WHEN typeof(disposition)='text' AND length(CAST(disposition AS BLOB))<=8 THEN disposition END FROM reference_receipts WHERE command_key=?1",[&key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let malformed = || LibraryError::ReferenceMalformedReceipt;
    let bytes = bytes.ok_or_else(malformed)?;
    let payload: Payload = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    payload.validate().map_err(|_| malformed())?;
    let owner = Uuid::parse_str(&owner.ok_or_else(malformed)?).map_err(|_| malformed())?;
    let identity = digest(&bytes);
    let disposition = ReferenceDisposition::parse(&disposition.ok_or_else(malformed)?)?;
    if seq <= 0
        || owner.is_nil()
        || hash.as_deref() != Some(result_digest(&bytes, disposition).as_slice())
        || payload.key()? != key
        || serde_json::to_vec(&payload)? != bytes
    {
        return Err(malformed());
    }
    if bytes != request.bytes {
        return Err(LibraryError::ReferenceReplayConflict);
    }
    Ok(Some(ReferenceReceipt {
        payload,
        identity,
        sequence: seq as u64,
        owner,
        disposition,
    }))
}
