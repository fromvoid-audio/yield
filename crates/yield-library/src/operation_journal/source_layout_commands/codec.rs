//! Fixed-order bounded binary transport; no JSON byte-array amplification or live tokens.
use super::*;
const MAGIC: &[u8; 4] = b"YSLI";
const CHECKSUM_BYTES: usize = 32;
const DIGEST_DOMAIN: &[u8] = b"yield.source-layout-intent.v1\0";
fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(DIGEST_DOMAIN);
    h.update(bytes);
    *h.finalize().as_bytes()
}
pub(super) fn encode(value: &SourceLayoutIntentArgs) -> Result<Vec<u8>, SourceLayoutIntentError> {
    value.validate()?;
    let r = &value.scope;
    let a = &value.allocation;
    let length = 4usize + 8 + 16 * 3 + 8 * 11 + 1 + CHECKSUM_BYTES + 4;
    let length = [
        r.source.len(),
        r.root.len(),
        r.root_binding.len(),
        r.database.len(),
    ]
    .into_iter()
    .try_fold(length, |n, len| n.checked_add(2)?.checked_add(len))
    .ok_or(SourceLayoutIntentError::Bytes)?;
    let length = length
        .checked_add(if a.prior_digest.is_some() { 32 } else { 0 })
        .and_then(|n| n.checked_add(value.generation.len()))
        .ok_or(SourceLayoutIntentError::Bytes)?;
    if length > MAX_SOURCE_LAYOUT_INTENT_BYTES {
        return Err(SourceLayoutIntentError::Bytes);
    }
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&SOURCE_LAYOUT_INTENT_VERSION.to_le_bytes());
    for id in [
        value.participant.as_uuid(),
        value.command.as_uuid(),
        r.profile,
    ] {
        out.extend_from_slice(id.as_bytes());
    }
    for bytes in [
        r.source.as_bytes(),
        r.root.as_slice(),
        r.root_binding.as_bytes(),
        r.database.as_bytes(),
    ] {
        out.extend_from_slice(
            &u16::try_from(bytes.len())
                .map_err(|_| SourceLayoutIntentError::Bytes)?
                .to_le_bytes(),
        );
        out.extend_from_slice(bytes);
    }
    for n in [
        r.lifecycle,
        r.lease,
        r.revision,
        a.page_size,
        a.page_count,
        a.database_bytes,
        a.maximum_pages,
        a.wal_bytes,
        a.shm_bytes,
        a.prior_bytes,
        a.peak,
    ] {
        out.extend_from_slice(&n.to_le_bytes());
    }
    out.push(u8::from(a.prior_digest.is_some()));
    if let Some(hash) = a.prior_digest {
        out.extend_from_slice(&hash);
    }
    out.extend_from_slice(
        &u32::try_from(value.generation.len())
            .map_err(|_| SourceLayoutIntentError::Bytes)?
            .to_le_bytes(),
    );
    out.extend_from_slice(&value.generation);
    let hash = digest(&out);
    out.extend_from_slice(&hash);
    if out.len() != length {
        return Err(SourceLayoutIntentError::Wire);
    }
    Ok(out)
}
/// Decode bounded binary historical input. Unknown versions are preserved without inspection.
pub fn decode_source_layout_intent(
    bytes: &[u8],
) -> Result<StoredSourceLayoutIntent, SourceLayoutIntentError> {
    use SourceLayoutIntentError as E;
    if bytes.len() > MAX_SOURCE_LAYOUT_INTENT_BYTES {
        return Err(E::Bytes);
    }
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4)? != MAGIC {
        return Err(E::Wire);
    }
    let version = reader.u64()?;
    if version == 0 {
        return Err(E::Wire);
    }
    if version != SOURCE_LAYOUT_INTENT_VERSION {
        return Ok(StoredSourceLayoutIntent::Unsupported {
            version,
            bytes: bytes.to_vec(),
        });
    }
    let data_end = bytes.len().checked_sub(CHECKSUM_BYTES).ok_or(E::Wire)?;
    if data_end < reader.at || digest(&bytes[..data_end]) != bytes[data_end..] {
        return Err(E::Wire);
    }
    reader.bytes = &bytes[..data_end];
    let participant = ParticipantId::from_uuid(reader.uuid()?);
    let command = SourceCommandId::from_uuid(reader.uuid()?);
    let profile = reader.uuid()?;
    let scope = SourceLayoutHistoricalScope {
        profile,
        source: reader.text(256)?,
        root: reader.field(4096)?.to_vec(),
        root_binding: reader.text(512)?,
        database: reader.text(512)?,
        lifecycle: reader.u64()?,
        lease: reader.u64()?,
        revision: reader.u64()?,
    };
    let mut allocation = SourceLayoutAllocationObservation {
        page_size: reader.u64()?,
        page_count: reader.u64()?,
        database_bytes: reader.u64()?,
        maximum_pages: reader.u64()?,
        wal_bytes: reader.u64()?,
        shm_bytes: reader.u64()?,
        prior_bytes: reader.u64()?,
        peak: reader.u64()?,
        prior_digest: None,
    };
    allocation.prior_digest = match reader.take(1)?[0] {
        0 => None,
        1 => Some(reader.take(32)?.try_into().map_err(|_| E::Wire)?),
        _ => return Err(E::Wire),
    };
    let size = u32::from_le_bytes(reader.take(4)?.try_into().map_err(|_| E::Wire)?) as usize;
    if size > 512 * 1024 {
        return Err(E::Bytes);
    }
    let generation = reader.take(size)?;
    // Validate the borrowed large field before making its single bounded owned copy.
    SourceLayoutGeneration::decode(generation).map_err(|_| E::Generation)?;
    if reader.at != data_end {
        return Err(E::Wire);
    }
    let value = SourceLayoutIntentArgs {
        participant,
        command,
        scope,
        allocation,
        generation: generation.to_vec(),
    };
    value.validate()?;
    if encode(&value)?.as_slice() != bytes {
        return Err(E::Wire);
    }
    Ok(StoredSourceLayoutIntent::Supported(value))
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], SourceLayoutIntentError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(SourceLayoutIntentError::Bytes)?;
        let data = self
            .bytes
            .get(self.at..end)
            .ok_or(SourceLayoutIntentError::Wire)?;
        self.at = end;
        Ok(data)
    }
    fn u64(&mut self) -> Result<u64, SourceLayoutIntentError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| SourceLayoutIntentError::Wire)?,
        ))
    }
    fn uuid(&mut self) -> Result<Uuid, SourceLayoutIntentError> {
        Uuid::from_slice(self.take(16)?).map_err(|_| SourceLayoutIntentError::Wire)
    }
    fn field(&mut self, max: usize) -> Result<&'a [u8], SourceLayoutIntentError> {
        let len = u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| SourceLayoutIntentError::Wire)?,
        ) as usize;
        if len > max {
            return Err(SourceLayoutIntentError::Bytes);
        }
        self.take(len)
    }
    fn text(&mut self, max: usize) -> Result<String, SourceLayoutIntentError> {
        std::str::from_utf8(self.field(max)?)
            .map(str::to_owned)
            .map_err(|_| SourceLayoutIntentError::Identity)
    }
}
