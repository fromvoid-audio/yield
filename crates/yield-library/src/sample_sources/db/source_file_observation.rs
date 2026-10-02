//! Source-owner-issued bounded file metadata observations; never physical effect authority.
use super::{
    SourceIndexClassification, SourceIndexDiagnostic, SourcePublicationFenceError,
    SourceReferenceFence, SourceReferenceFenceError, SourceRevision, SourceWriter,
};
use crate::operation_journal::StoredRelativePath;
use rusqlite::Connection;
use thiserror::Error;

/// Fail-closed exact file observation failures.
#[derive(Debug, Error)]
pub enum SourceFileObservationError {
    /// The input is not a bounded portable source-relative path.
    #[error("invalid source file observation path")]
    InvalidPath,
    /// No authoritative audio/index row names this exact key.
    #[error("source file observation missing; authoritative audit required")]
    Missing,
    /// More than one row/classification names the exact key.
    #[error("ambiguous source file observation; authoritative audit required")]
    Ambiguous,
    /// Stored row types, lengths, encoding or content identity are invalid.
    #[error("malformed source file observation; authoritative audit required")]
    Malformed,
    /// The row is missing/inaccessible or has incomplete metadata.
    #[error("source file metadata unavailable; authoritative audit required")]
    Unavailable,
    /// Exact row facts changed even if an auxiliary writer kept the source revision.
    #[error("source file observation changed")]
    IdentityChanged,
    /// Current metadata has no full content hash; it cannot supply a derivation parent.
    #[error("full content identity unavailable; owner-bound content audit/backfill required")]
    ContentAuditRequired,
    /// Issuing writer, source revision or retained source ownership failed.
    #[error(transparent)]
    Fence(#[from] SourceReferenceFenceError),
    /// Retained control ownership failed its read-only validation.
    #[error(transparent)]
    Publication(#[from] SourcePublicationFenceError),
    /// The bounded retained-connection query failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

/// Authoritative classification, without any physical publication/absence assertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceFileObservationKind {
    /// A live supported sample manifest row.
    SupportedAudio,
    /// A regular file intentionally excluded from supported sample state.
    IndexOnly(SourceIndexClassification),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FileFacts {
    kind: SourceFileObservationKind,
    size: u64,
    modified: i64,
    file_identity: Option<String>,
    content_hash: Option<String>,
    diagnostic: Option<SourceIndexDiagnostic>,
    format_policy: Option<u32>,
}

/// Exact bounded metadata privately issued by a retained source writer.
/// No public constructor/decoder exists. It retains weak owner liveness, not a lease.
#[derive(Clone, Debug)]
pub struct SourceFileObservation {
    path: StoredRelativePath,
    facts: FileFacts,
    fence: SourceReferenceFence,
}
impl SourceFileObservation {
    /// Exact validated UTF-8 key; no lossy pathname conversion.
    pub fn path(&self) -> &StoredRelativePath {
        &self.path
    }
    /// Historical source/root/database/lifecycle/lease/audited revision of issuance.
    pub fn source_revision(&self) -> &SourceRevision {
        self.fence.source_revision()
    }
    /// Actual stored audio/index classification.
    pub fn kind(&self) -> SourceFileObservationKind {
        self.facts.kind
    }
    /// Authoritative stored file size.
    pub fn file_size(&self) -> u64 {
        self.facts.size
    }
    /// Authoritative stored modified timestamp.
    pub fn modified_ns(&self) -> i64 {
        self.facts.modified
    }
    /// Stored object identity, when the source owner recorded one.
    pub fn file_identity(&self) -> Option<&str> {
        self.facts.file_identity.as_deref()
    }
    /// Full stored content hash when available; size/mtime never substitute for it.
    pub fn content_hash(&self) -> Option<&str> {
        self.facts.content_hash.as_deref()
    }
    /// Exact index diagnostic, when present.
    pub fn diagnostic(&self) -> Option<SourceIndexDiagnostic> {
        self.facts.diagnostic
    }
    /// Actual index classification policy version, when applicable.
    pub fn format_policy_version(&self) -> Option<u32> {
        self.facts.format_policy
    }
    /// Recheck issuing writer, source owner/revision and every exact stored identity field.
    /// Point-in-time metadata validation grants no physical authority or cross-owner atomicity.
    pub fn validate(&self) -> Result<(), SourceFileObservationError> {
        if self.fence.file_facts(&self.path)? != self.facts {
            return Err(SourceFileObservationError::IdentityChanged);
        }
        Ok(())
    }
    /// Require actual full content identity before forming a Harvest derivation proposal.
    /// Index-only or not-yet-hashed metadata explicitly requires content audit/backfill.
    pub fn into_content_identity(
        self,
    ) -> Result<SourceContentIdentityObservation, SourceFileObservationError> {
        self.validate()?;
        if self.facts.content_hash.is_none() {
            return Err(SourceFileObservationError::ContentAuditRequired);
        }
        Ok(SourceContentIdentityObservation { observation: self })
    }
}
/// Private full-content-ready observation for metadata consumers, not publication authority.
#[derive(Clone, Debug)]
pub struct SourceContentIdentityObservation {
    observation: SourceFileObservation,
}
impl SourceContentIdentityObservation {
    /// Borrow the exact privately issued metadata observation.
    pub fn file(&self) -> &SourceFileObservation {
        &self.observation
    }
    /// Full owner-recorded content identity; it is not a newly verified physical hash read.
    pub fn content_hash(&self) -> &str {
        self.observation
            .facts
            .content_hash
            .as_deref()
            .expect("private full-content invariant")
    }
    /// Recheck exact owner/row observations immediately before consuming this metadata result.
    pub fn validate(&self) -> Result<(), SourceFileObservationError> {
        self.observation.validate()
    }
}
impl SourceWriter {
    /// Observe one exact supported/index-only row through retained source ownership.
    /// No mutation, traversal, lease renewal or pathname/database reopen occurs.
    pub fn observe_file_identity(
        &self,
        path: StoredRelativePath,
    ) -> Result<SourceFileObservation, SourceFileObservationError> {
        path.validate()
            .map_err(|_| SourceFileObservationError::InvalidPath)?;
        let fence = self.reference_read_fence()?;
        let facts = fence.file_facts(&path)?;
        let revision = fence.source_revision();
        let accounted = path.as_str().len()
            + revision.source_id().as_str().len()
            + revision.root_identity().as_bytes().len()
            + revision.database_identity().len()
            + revision.root_binding_identity().len()
            + facts.file_identity.as_ref().map_or(0, String::len)
            + facts.content_hash.as_ref().map_or(0, String::len)
            + 256;
        if accounted > 65536 {
            return Err(SourceFileObservationError::Malformed);
        }
        let observation = SourceFileObservation { path, facts, fence };
        observation.validate()?;
        Ok(observation)
    }
}

// All values are bounded by SQLite CASE predicates before Rust allocation. One SELECT
// observes both namespaces consistently and LIMIT 3 diagnoses ambiguity before readback.
const SELECT_FACTS: &str = "SELECT * FROM (
 SELECT 'audio',
 CASE WHEN typeof(file_size)='integer' AND file_size>=0 THEN file_size END,
 CASE WHEN typeof(modified_ns)='integer' THEN modified_ns END,
 CASE WHEN typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512 THEN file_identity END,
 CASE WHEN typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB)) BETWEEN 1 AND 256 THEN content_hash END,
 NULL, NULL,
 CASE WHEN (file_identity IS NULL OR (typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512))
 AND (content_hash IS NULL OR (typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB)) BETWEEN 1 AND 256))
 AND typeof(missing)='integer' AND missing IN (0,1) THEN 1 ELSE 0 END,
 CASE WHEN missing=0 THEN 1 ELSE 0 END, 1
 FROM wav_files WHERE path=?1
 UNION ALL
 SELECT CASE WHEN typeof(classification)='text' AND length(CAST(classification AS BLOB)) BETWEEN 1 AND 64 THEN classification END,
 CASE WHEN typeof(file_size)='integer' AND file_size>=0 THEN file_size END,
 CASE WHEN typeof(modified_ns)='integer' THEN modified_ns END,
 CASE WHEN typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512 THEN file_identity END,
 NULL,
 CASE WHEN typeof(diagnostic)='text' AND length(CAST(diagnostic AS BLOB)) BETWEEN 1 AND 64 THEN diagnostic END,
 CASE WHEN typeof(format_policy_version)='integer' AND format_policy_version BETWEEN 0 AND 4294967295 THEN format_policy_version END,
 CASE WHEN typeof(path_encoding)='integer' AND path_encoding=0
 AND (file_identity IS NULL OR (typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512))
 AND (diagnostic IS NULL OR (typeof(diagnostic)='text' AND length(CAST(diagnostic AS BLOB)) BETWEEN 1 AND 64))
 AND (file_size IS NULL OR (typeof(file_size)='integer' AND file_size>=0))
 AND (modified_ns IS NULL OR typeof(modified_ns)='integer') THEN 1 ELSE 0 END,
 1, 0 FROM source_index_entries WHERE path=?1
) LIMIT 3";

pub(super) fn read_facts(
    connection: &Connection,
    path: &StoredRelativePath,
) -> Result<FileFacts, SourceFileObservationError> {
    use SourceFileObservationError as E;
    let count:i64=connection.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM wav_files WHERE path=?1 UNION ALL SELECT 1 FROM source_index_entries WHERE path=?1 LIMIT 3)",[path.as_str()],|r|r.get(0))?;
    if count == 0 {
        return Err(E::Missing);
    }
    if count != 1 {
        return Err(E::Ambiguous);
    }
    let mut statement = connection.prepare(SELECT_FACTS)?;
    let mut rows = statement.query([path.as_str()])?;
    let row = rows.next()?.ok_or(E::Missing)?;
    let kind: Option<String> = row.get(0)?;
    let size: Option<i64> = row.get(1)?;
    let modified: Option<i64> = row.get(2)?;
    let file_identity: Option<String> = row.get(3)?;
    let content_hash: Option<String> = row.get(4)?;
    let diagnostic: Option<String> = row.get(5)?;
    let policy: Option<i64> = row.get(6)?;
    let valid: bool = row.get(7)?;
    let live: bool = row.get(8)?;
    let audio_namespace: bool = row.get(9)?;
    if rows.next()?.is_some() {
        return Err(E::Ambiguous);
    }
    if !valid || file_identity.as_ref().is_some_and(|v| v.contains('\0')) {
        return Err(E::Malformed);
    }
    let kind = if audio_namespace {
        SourceFileObservationKind::SupportedAudio
    } else {
        SourceFileObservationKind::IndexOnly(
            SourceIndexClassification::from_token(kind.as_deref().ok_or(E::Malformed)?)
                .ok_or(E::Malformed)?,
        )
    };
    let diagnostic = diagnostic
        .as_deref()
        .map(|v| SourceIndexDiagnostic::from_token(v).ok_or(E::Malformed))
        .transpose()?;
    let format_policy = match kind {
        SourceFileObservationKind::SupportedAudio => None,
        SourceFileObservationKind::IndexOnly(_) => Some(policy.ok_or(E::Malformed)? as u32),
    };
    if !live
        || matches!(
            kind,
            SourceFileObservationKind::IndexOnly(SourceIndexClassification::Inaccessible)
        )
    {
        return Err(E::Unavailable);
    }
    // Current scanner full-content hashes are canonical 256-bit lowercase hexadecimal.
    // Unsupported spellings fail closed; they are not inferred from filesystem hints.
    if content_hash.as_ref().is_some_and(|h| {
        h.len() != 64
            || !h
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(E::Malformed);
    }
    Ok(FileFacts {
        kind,
        size: size.ok_or(E::Unavailable)? as u64,
        modified: modified.ok_or(E::Unavailable)?,
        file_identity,
        content_hash,
        diagnostic,
        format_policy,
    })
}
#[cfg(test)]
mod tests;
