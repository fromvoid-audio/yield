//! Fixed-budget evidence readback and exact typed row checks, before allocation.
use super::*;
use rusqlite::{Connection, params};
const TABLE: &str = "source_manifest_enrollment_receipts";
const MAX_ROWS: i64 = 8192;
const MAX_BYTES: i64 = 16 * 1024 * 1024;
fn hash(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(domain);
    h.update(bytes);
    *h.finalize().as_bytes()
}
fn payload_hash(bytes: &[u8]) -> [u8; 32] {
    hash(b"yield.source.enrollment.proposal.v1\0", bytes)
}
fn result_hash(payload: &[u8], result: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"yield.source.enrollment.result.v1\0");
    h.update(payload);
    h.update(result);
    *h.finalize().as_bytes()
}
const BOUNDED: &str =
    "typeof(command_key)='text' AND length(CAST(command_key AS BLOB)) BETWEEN 1 AND 512
 AND typeof(payload)='blob' AND length(payload) BETWEEN 1 AND 65536
 AND typeof(payload_hash)='blob' AND length(payload_hash)=32
 AND typeof(result)='blob' AND length(result) BETWEEN 1 AND 65536
 AND typeof(result_hash)='blob' AND length(result_hash)=32";
pub(super) fn read(
    c: &Connection,
    r: &ManifestEnrollmentRequest,
) -> Result<Option<ManifestCommitResult>, ManifestEnrollmentError> {
    let key = r.payload.key()?;
    let count: i64 = c.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {TABLE} WHERE command_key=?1 LIMIT 2)"),
        [&key],
        |row| row.get(0),
    )?;
    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(ManifestEnrollmentError::Evidence);
    }
    let mut statement=c.prepare(&format!("SELECT CASE WHEN {BOUNDED} THEN payload END, CASE WHEN {BOUNDED} THEN payload_hash END, CASE WHEN {BOUNDED} THEN result END, CASE WHEN {BOUNDED} THEN result_hash END FROM {TABLE} WHERE command_key=?1 LIMIT 2"))?;
    let mut rows = statement.query([key])?;
    let row = rows.next()?.ok_or(ManifestEnrollmentError::Evidence)?;
    let bytes: Option<Vec<u8>> = row.get(0)?;
    let input_hash: Option<Vec<u8>> = row.get(1)?;
    let result: Option<Vec<u8>> = row.get(2)?;
    let output_hash: Option<Vec<u8>> = row.get(3)?;
    if rows.next()?.is_some() {
        return Err(ManifestEnrollmentError::Evidence);
    }
    let (Some(bytes), Some(input_hash), Some(result), Some(output_hash)) =
        (bytes, input_hash, result, output_hash)
    else {
        return Err(ManifestEnrollmentError::Evidence);
    };
    if input_hash != payload_hash(&bytes) || output_hash != result_hash(&bytes, &result) {
        return Err(ManifestEnrollmentError::Evidence);
    }
    // Validate stored custody before reporting conflict; corrupt evidence is never a disposition.
    let stored: Payload =
        serde_json::from_slice(&bytes).map_err(|_| ManifestEnrollmentError::Evidence)?;
    stored
        .validate()
        .map_err(|_| ManifestEnrollmentError::Evidence)?;
    if serde_json::to_vec(&stored)? != bytes || stored.key()? != r.payload.key()? {
        return Err(ManifestEnrollmentError::Evidence);
    }
    if bytes != r.bytes {
        return Err(ManifestEnrollmentError::Conflict);
    }
    let result_value: ManifestCommitResult =
        serde_json::from_slice(&result).map_err(|_| ManifestEnrollmentError::Evidence)?;
    if serde_json::to_vec(&result_value)? != result || !valid_result(r, &result_value) {
        return Err(ManifestEnrollmentError::Evidence);
    }
    Ok(Some(result_value))
}
fn valid_result(r: &ManifestEnrollmentRequest, m: &ManifestCommitResult) -> bool {
    if r.payload.source.revision.checked_add(1) != Some(m.revision)
        || m.authoritative_snapshot.is_some()
        || m.touched_path_changes.len() > 64
    {
        return false;
    }
    let mut paths = BTreeSet::new();
    for (path, facts) in &m.touched_path_changes {
        let Some(expected) = r
            .entries()
            .iter()
            .find(|e| path.to_str() == Some(e.path.as_str()))
        else {
            return false;
        };
        if !paths.insert(path) {
            return false;
        }
        match (&expected.kind, facts) {
            (ManifestEnrollmentKind::Audio, Some(f))
                if f.relative_path.as_os_str() == path.as_os_str()
                    && f.file_size == expected.size
                    && f.modified_ns == expected.modified_ns
                    && f.file_identity.as_deref() == Some(&expected.file_identity)
                    && f.content_hash == expected.content_hash => {}
            (ManifestEnrollmentKind::Index { .. }, None) => {}
            _ => return false,
        }
    }
    if let Some(index) = &m.source_index_commit {
        if index.source_revision != m.revision
            || index.index_revision == 0
            || index.index_revision > i64::MAX as u64
            || index.upserted_entries.len() + index.removed_paths.len() > 64
        {
            return false;
        }
        let mut seen = BTreeSet::new();
        for entry in &index.upserted_entries {
            if !seen.insert(&entry.relative_path) {
                return false;
            }
            let Some(e) = r
                .entries()
                .iter()
                .find(|e| entry.relative_path.to_str() == Some(e.path.as_str()))
            else {
                return false;
            };
            let ManifestEnrollmentKind::Index {
                classification,
                diagnostic,
                policy,
            } = e.kind
            else {
                return false;
            };
            if entry.classification != classification
                || entry.diagnostic != diagnostic
                || entry.format_policy_version != policy
                || entry.file_size != Some(e.size)
                || entry.modified_ns != Some(e.modified_ns)
                || entry.file_identity.as_deref() != Some(&e.file_identity)
            {
                return false;
            }
        }
        for path in &index.removed_paths {
            if !seen.insert(path)
                || !r.entries().iter().any(|e| {
                    matches!(e.kind, ManifestEnrollmentKind::Audio)
                        && path.to_str() == Some(e.path.as_str())
                })
            {
                return false;
            }
        }
    }
    true
}
pub(super) fn capacity(c: &Connection, additional: usize) -> Result<(), ManifestEnrollmentError> {
    let malformed: bool = c.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {TABLE} WHERE NOT ({BOUNDED}) LIMIT 1)"),
        [],
        |r| r.get(0),
    )?;
    if malformed {
        return Err(ManifestEnrollmentError::Evidence);
    }
    let (rows,bytes):(i64,i64)=c.query_row(&format!("SELECT COUNT(*),COALESCE(SUM(length(CAST(command_key AS BLOB))+length(payload)+length(payload_hash)+length(result)+length(result_hash)+32),0) FROM (SELECT * FROM {TABLE} LIMIT 8193)"),[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if rows >= MAX_ROWS
        || bytes
            .checked_add(additional as i64)
            .is_none_or(|n| n > MAX_BYTES)
    {
        return Err(ManifestEnrollmentError::Capacity);
    }
    Ok(())
}
pub(super) fn insert(
    c: &Connection,
    r: &ManifestEnrollmentRequest,
    m: &ManifestCommitResult,
) -> Result<(), ManifestEnrollmentError> {
    if !valid_result(r, m) {
        return Err(ManifestEnrollmentError::Evidence);
    }
    let result = serde_json::to_vec(m)?;
    let key = r.payload.key()?;
    if result.is_empty() || result.len() > 65536 || key.len() > 512 {
        return Err(ManifestEnrollmentError::Input);
    }
    capacity(c, key.len() + r.bytes.len() + result.len() + 64 + 32)?;
    c.execute(&format!("INSERT INTO {TABLE}(command_key,payload,payload_hash,result,result_hash) VALUES(?1,?2,?3,?4,?5)"),params![key,r.bytes,payload_hash(&r.bytes).as_slice(),result,result_hash(&r.bytes,&result).as_slice()])?;
    Ok(())
}
pub(super) fn validate_entry(
    c: &Connection,
    e: &ManifestEnrollmentEntry,
) -> Result<(), ManifestEnrollmentError> {
    let path = e.path.as_str();
    let count:i64=c.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM wav_files WHERE path=?1 AND missing=0 UNION ALL SELECT 1 FROM source_index_entries WHERE path=?1 LIMIT 2)",[path],|r|r.get(0))?;
    if count != 1 {
        return Err(ManifestEnrollmentError::Input);
    }
    let matches:bool=match &e.kind{
        ManifestEnrollmentKind::Audio=>c.query_row("SELECT EXISTS(SELECT 1 FROM wav_files WHERE path=?1 AND typeof(missing)='integer' AND missing=0 AND typeof(file_size)='integer' AND file_size=?2 AND typeof(modified_ns)='integer' AND modified_ns=?3 AND typeof(file_identity)='text' AND file_identity=?4 AND typeof(content_hash)='text' AND content_hash=?5)",params![path,e.size as i64,e.modified_ns,e.file_identity,e.content_hash],|r|r.get(0))?,
        ManifestEnrollmentKind::Index{classification,diagnostic,policy}=>c.query_row("SELECT EXISTS(SELECT 1 FROM source_index_entries WHERE path=?1 AND typeof(path_encoding)='integer' AND path_encoding=0 AND typeof(file_size)='integer' AND file_size=?2 AND typeof(modified_ns)='integer' AND modified_ns=?3 AND typeof(file_identity)='text' AND file_identity=?4 AND typeof(classification)='text' AND classification=?5 AND diagnostic IS ?6 AND typeof(format_policy_version)='integer' AND format_policy_version=?7)",params![path,e.size as i64,e.modified_ns,e.file_identity,classification.token(),diagnostic.map(|v|v.token()),*policy as i64],|r|r.get(0))?,
    };
    if !matches {
        return Err(ManifestEnrollmentError::Input);
    }
    Ok(())
}
