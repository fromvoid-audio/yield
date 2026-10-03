//! Actual signed-byte verification. All trust substitution is private and compiled out of builds.
use super::{PayloadIdentity, SelectedPayload, SelectionError, catalog::File};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
mod archive;
pub(super) mod owner;

const PIN: &str = "8Z7dQJBRMbxCFkFMeBYa1FMSWOUm6nePFgoK5c43jT4=";
const ARCHIVE_CAP: usize = 1024 * 1024 * 1024;
const WORKING_CAP: usize = 64 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

/// Privately verified immutable original ZIP bytes. This is neither a native qualification
/// nor application-scoped staged owner, filesystem capability or installation receipt.
///
/// ```compile_fail
/// use yield_platform::payload_selection::VerifiedPayload;
/// fn copy(value: VerifiedPayload) { let _ = value.clone(); }
/// ```
/// ```compile_fail
/// use yield_platform::payload_selection::VerifiedPayload;
/// fn inspect(value: VerifiedPayload) { let _ = value.bytes; }
/// ```
/// ```compile_fail
/// use yield_platform::payload_selection::VerifiedPayload;
/// fn move_to_ui(value: VerifiedPayload) { std::thread::spawn(move || drop(value)); }
/// ```
/// ```compile_fail
/// use yield_platform::payload_selection::VerifiedPayload;
/// fn forge() { let _: VerifiedPayload = serde_json::from_slice(b"{}").unwrap(); }
/// ```
pub struct VerifiedPayload {
    selection: SelectedPayload,
    bytes: Vec<u8>,
    hash: [u8; 32],
    build_date: String,
    // Verification/staging worker retains the only buffer; UI receives observations, not this
    // allocation. The future staged owner retires it on that same worker (OPT-2227).
    worker_only: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl VerifiedPayload {
    /// Exact authenticated full identity, while the original selection remains live.
    pub fn identity(&self) -> Result<PayloadIdentity<'_>, SelectionError> {
        self.selection.identity()
    }
    /// Retained byte length; callers cannot mutate, reconstruct or extract the archive.
    pub fn archive_len(&self) -> Result<usize, SelectionError> {
        self.selection.ensure_live()?;
        Ok(self.bytes.len())
    }
    /// Complete hash of the retained immutable ZIP, agreeing with signed/catalog declarations.
    pub fn archive_sha256(&self) -> Result<[u8; 32], SelectionError> {
        self.selection.ensure_live()?;
        Ok(self.hash)
    }
    /// Authenticated manifest calendar date. Native compiled-date reconciliation remains separate.
    pub fn build_date(&self) -> Result<&str, SelectionError> {
        self.selection.ensure_live()?;
        Ok(&self.build_date)
    }
}

struct Check<'a> {
    selection: &'a SelectedPayload,
    deadline: Instant,
}
impl Check<'_> {
    fn live(&self) -> Result<(), SelectionError> {
        self.selection.ensure_live()?;
        if Instant::now() >= self.deadline {
            Err(SelectionError::Retired)
        } else {
            Ok(())
        }
    }
}
struct Ledger(usize);
impl Ledger {
    fn charge(&mut self, bytes: usize) -> Result<(), SelectionError> {
        self.0 = self
            .0
            .checked_add(bytes)
            .filter(|n| *n <= WORKING_CAP)
            .ok_or(SelectionError::Invalid)?;
        Ok(())
    }
}
#[derive(Clone)]
enum Key {
    Compiled,
    #[cfg(test)]
    Fixture(VerifyingKey),
}
impl Key {
    fn get(self) -> Result<VerifyingKey, SelectionError> {
        match self {
            Self::Compiled => {
                let mut raw = [0; 32];
                if STANDARD
                    .decode_slice(PIN, &mut raw)
                    .map_err(|_| SelectionError::Invalid)?
                    != 32
                {
                    return Err(SelectionError::Invalid);
                }
                VerifyingKey::from_bytes(&raw).map_err(|_| SelectionError::Invalid)
            }
            #[cfg(test)]
            Self::Fixture(key) => Ok(key),
        }
    }
}

pub(super) fn verify(
    selection: SelectedPayload,
    bytes: Vec<u8>,
    checksums: Vec<u8>,
    signature: Vec<u8>,
) -> Result<VerifiedPayload, SelectionError> {
    verify_before(
        selection,
        bytes,
        checksums,
        signature,
        Instant::now() + Duration::from_secs(300),
    )
}
/// Internal owner composition preserves the original whole-attempt deadline across HTTP/CPU work.
pub(super) fn verify_before(
    selection: SelectedPayload,
    bytes: Vec<u8>,
    checksums: Vec<u8>,
    signature: Vec<u8>,
    deadline: Instant,
) -> Result<VerifiedPayload, SelectionError> {
    verify_inner(
        selection,
        bytes,
        checksums,
        signature,
        Key::Compiled,
        deadline.min(Instant::now() + Duration::from_secs(300)),
    )
}
fn verify_inner(
    selection: SelectedPayload,
    bytes: Vec<u8>,
    checksums: Vec<u8>,
    signature: Vec<u8>,
    key: Key,
    deadline: Instant,
) -> Result<VerifiedPayload, SelectionError> {
    let invalid = SelectionError::Invalid;
    let check = Check {
        selection: &selection,
        deadline,
    };
    check.live()?;
    if bytes.is_empty()
        || bytes.len() > ARCHIVE_CAP
        || bytes.capacity() > ARCHIVE_CAP
        || checksums.is_empty()
        || checksums.len() > 1024 * 1024
        || checksums.capacity() > 1024 * 1024
        || signature.is_empty()
        || signature.len() > 8192
        || signature.capacity() > 8192
    {
        return Err(invalid);
    }
    // Fixed conservative credit covers manifest raw/decoded text/list, decompressor/hash/signature
    // work and bounded selection/proof overhead. Index and normalized-name capacities add below.
    let mut ledger = Ledger(super::protocol::staging::OWNER_WORKING_CREDIT);
    ledger.charge(checksums.capacity())?;
    ledger.charge(signature.capacity())?;
    let version = semver::Version::parse(&selection.release.version).map_err(|_| invalid)?;
    let target_name = crate::update_catalog::asset_name(
        &version,
        selection.channel,
        selection.installed.platform,
    )
    .ok_or(invalid)?;
    let target = selection
        .release
        .files
        .iter()
        .find(|f| f.name == target_name)
        .ok_or(invalid)?;
    let checksum_file = selection
        .release
        .files
        .iter()
        .find(|f| f.name.ends_with(".txt"))
        .ok_or(invalid)?;
    let signature_file = selection
        .release
        .files
        .iter()
        .find(|f| f.name.ends_with(".txt.sig"))
        .ok_or(invalid)?;
    require_file(&checksums, checksum_file, &check)?;
    require_file(&signature, signature_file, &check)?;
    // Strict base64 and Ed25519: never normalize signed message bytes or permit weak/noncanonical
    // points/scalars. No environment/catalog/test key is accepted by the production entry point.
    let mut raw = [0; 66];
    let decoded = STANDARD
        .decode_slice(&signature, &mut raw)
        .map_err(|_| invalid)?;
    if decoded != 64 {
        return Err(invalid);
    }
    let sig = Signature::from_slice(&raw[..64]).map_err(|_| invalid)?;
    check.live()?;
    key.get()?
        .verify_strict(&checksums, &sig)
        .map_err(|_| invalid)?;
    check.live()?;
    checksum_inventory(&checksums, &selection, &check)?;
    let hash = require_file(&bytes, target, &check)?;
    let date = archive::verify(&bytes, &selection, &check, &mut ledger)?;
    check.live()?;
    // Move the sole allocation; no archive-sized clone, box conversion or shrinking copy.
    drop(checksums);
    drop(signature);
    Ok(VerifiedPayload {
        selection,
        bytes,
        hash,
        build_date: date,
        worker_only: std::marker::PhantomData,
    })
}
fn require_file(bytes: &[u8], file: &File, check: &Check<'_>) -> Result<[u8; 32], SelectionError> {
    if u64::try_from(bytes.len()).ok() != Some(file.size_bytes) {
        return Err(SelectionError::Invalid);
    }
    let mut hash = Sha256::new();
    for chunk in bytes.chunks(CHUNK) {
        check.live()?;
        hash.update(chunk);
    }
    let hash: [u8; 32] = hash.finalize().into();
    if hash != file.sha256 {
        return Err(SelectionError::Invalid);
    }
    Ok(hash)
}
fn checksum_inventory(
    bytes: &[u8],
    selection: &SelectedPayload,
    check: &Check<'_>,
) -> Result<(), SelectionError> {
    let invalid = SelectionError::Invalid;
    let text = std::str::from_utf8(bytes).map_err(|_| invalid)?;
    let mut seen = [false; 5];
    let mut count = 0;
    for line in text.split_terminator('\n') {
        check.live()?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() < 67 || &line.as_bytes()[64..66] != b"  " {
            return Err(invalid);
        }
        let name = &line[66..];
        let (index, file) = selection
            .release
            .files
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name && f.name.ends_with(".zip"))
            .ok_or(invalid)?;
        if seen.get(index).copied().unwrap_or(true) {
            return Err(invalid);
        }
        let mut digest = [0; 32];
        for (out, hex) in digest.iter_mut().zip(line.as_bytes()[..64].chunks_exact(2)) {
            let digit = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            *out = digit(hex[0]).ok_or(invalid)? * 16 + digit(hex[1]).ok_or(invalid)?;
        }
        if digest != file.sha256 {
            return Err(invalid);
        }
        seen[index] = true;
        count += 1;
    }
    if count != 3
        || selection
            .release
            .files
            .iter()
            .enumerate()
            .any(|(i, f)| f.name.ends_with(".zip") && !seen[i])
    {
        return Err(invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
