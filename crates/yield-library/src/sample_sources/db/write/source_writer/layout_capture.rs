//! Complete, bounded, read-only feature capture on the retained source connection.
use super::{SnapshotProgressGuard, SourceRevision, SourceWriter, SourceWriterError};
use crate::sample_sources::db::{SourceReferenceFence, SourceReferenceFenceError};
use crate::sample_sources::readiness::{
    FeatureArtifactKey, FeatureArtifactPayload, ReadinessError, ReadinessStage, ReadinessTarget,
};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;

/// Fixed existing-engine policy bound into every captured input digest.
pub const SOURCE_LAYOUT_ALGORITHM_V1: &str =
    "features_v1__len183__l2/pca-tsne-v1/seed0/hdbscan-v1:min5,samples=none,single=false";
const MAX_ROWS: usize = 256;
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// An exact observed file and its immutable validated feature bytes; no write authority.
#[derive(Debug)]
pub struct SourceLayoutFeature {
    path: String,
    identity: String,
    content_hash: String,
    size: u64,
    modified_ns: i64,
    sample_identity: String,
    target: ReadinessTarget,
    key: FeatureArtifactKey,
    payload: FeatureArtifactPayload,
}
impl SourceLayoutFeature {
    /// Exact source-relative database key.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Audited browser file identity, separate from the sample identity.
    pub fn file_identity(&self) -> &str {
        &self.identity
    }
    /// Complete lowercase BLAKE3 content identity.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
    /// Observed manifest byte length.
    pub fn size(&self) -> u64 {
        self.size
    }
    /// Observed manifest timestamp.
    pub fn modified_ns(&self) -> i64 {
        self.modified_ns
    }
    /// Exact existing source_id::relative_path sample identity.
    pub fn sample_identity(&self) -> &str {
        &self.sample_identity
    }
    /// Current owner-derived AnalysisFeatures v1 target.
    pub fn target(&self) -> &ReadinessTarget {
        &self.target
    }
    /// Validated persisted feature key, not mutation authority.
    pub fn key(&self) -> &FeatureArtifactKey {
        &self.key
    }
    /// Exactly 732 validated bytes for existing engine input.
    pub fn payload(&self) -> &FeatureArtifactPayload {
        &self.payload
    }
}

/// Private owner-produced capture, retained until a CPU result is accepted or rejected.
/// It is deliberately neither publicly constructible nor deserializable.
#[derive(Debug)]
pub struct SourceLayoutCapture {
    fence: SourceReferenceFence,
    owner: Weak<()>,
    features: Vec<SourceLayoutFeature>,
    digest: [u8; 32],
    owned_bytes: usize,
}
impl SourceLayoutCapture {
    /// Initial complete-source row budget shared with the bounded computation consumer.
    pub const MAX_FILES: usize = MAX_ROWS;
    /// Maximum retained captured input charge.
    pub const MAX_INPUT_BYTES: usize = MAX_CAPTURE_BYTES;
    /// Historical exact source/root/database/lifecycle/lease/revision scope.
    pub fn revision(&self) -> &SourceRevision {
        self.fence.source_revision()
    }
    /// Complete ordered membership; never a silently truncated source layout.
    pub fn features(&self) -> &[SourceLayoutFeature] {
        &self.features
    }
    /// Versioned length-prefixed source/membership/feature/model identity.
    pub fn input_digest(&self) -> &[u8; 32] {
        &self.digest
    }
    /// Conservative charge for all owned vectors, strings, targets, keys and scope.
    pub fn owned_bytes(&self) -> usize {
        self.owned_bytes
    }
    /// Recheck writer lifetime and exact audited scope without renewal or mutation.
    /// Current membership/feature bytes require SourceWriter::validate_layout_capture.
    pub fn validate_read_fence(&self) -> Result<(), SourceLayoutCaptureError> {
        self.fence.validate()?;
        Ok(())
    }
}

/// A retained prerequisite or rejection, never fabricated layout readiness.
#[derive(Debug, Error)]
pub enum SourceLayoutCaptureError {
    /// No supported non-missing audio rows exist.
    #[error("layout manifest is empty")]
    Empty,
    /// A complete result exceeds its initial finite budget.
    #[error("layout capture exceeds {0}")]
    Limit(&'static str),
    /// Manifest values or exact path/identity spelling are malformed or aliased.
    #[error("invalid layout manifest: {0}")]
    InvalidManifest(&'static str),
    /// The current v1 feature producer does not support this exact format.
    #[error("layout feature format unsupported: {0}")]
    UnsupportedFormat(String),
    /// Actual persisted feature/completion/provenance cannot supply this input.
    #[error("layout features unavailable for {path}: {reason}")]
    FeatureUnavailable {
        /// Exact bounded path whose prerequisite is unavailable.
        path: String,
        /// Actual missing, corrupt, ambiguous, unsupported or unconfirmed evidence reason.
        reason: &'static str,
    },
    /// Cancellation prevents a successful capture or validation.
    #[error("layout capture cancelled")]
    Cancelled,
    /// The two-second cooperative budget, including caller queue wait, expired.
    #[error("layout capture deadline expired")]
    Deadline,
    /// The issuing owner, membership or feature identities changed.
    #[error("layout capture is stale")]
    Stale,
    /// Public CPU input cannot observe an uncommitted owner transaction.
    #[error("layout capture requires transaction quiescence")]
    TransactionActive,
    /// Real retained owner validation failed.
    #[error(transparent)]
    Owner(#[from] SourceWriterError),
    /// Exact read fence failed; the original evidence is retained.
    #[error(transparent)]
    Fence(#[from] SourceReferenceFenceError),
}

fn checkpoint(cancel: &AtomicBool, deadline: Instant) -> Result<(), SourceLayoutCaptureError> {
    if cancel.load(Ordering::Acquire) {
        return Err(SourceLayoutCaptureError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(SourceLayoutCaptureError::Deadline);
    }
    Ok(())
}
fn charge(total: &mut usize, bytes: usize) -> Result<(), SourceLayoutCaptureError> {
    *total = total
        .checked_add(bytes)
        .filter(|v| *v <= MAX_CAPTURE_BYTES)
        .ok_or(SourceLayoutCaptureError::Limit("owned input bytes"))?;
    Ok(())
}
fn hash_field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
pub(super) fn layout_input_hasher(scope: [&[u8]; 4], count: usize) -> blake3::Hasher {
    let mut hash = blake3::Hasher::new();
    for field in [
        b"yield-source-layout-input-v1".as_slice(),
        SOURCE_LAYOUT_ALGORITHM_V1.as_bytes(),
    ]
    .into_iter()
    .chain(scope)
    {
        hash_field(&mut hash, field);
    }
    hash_field(&mut hash, &(count as u64).to_le_bytes());
    hash
}
fn sql_error(
    error: rusqlite::Error,
    cancel: &AtomicBool,
    deadline: Instant,
) -> SourceLayoutCaptureError {
    checkpoint(cancel, deadline).err().unwrap_or_else(|| {
        SourceLayoutCaptureError::Owner(
            crate::sample_sources::db::util::map_sql_error(error).into(),
        )
    })
}

struct ManifestRow {
    path: String,
    identity: String,
    content: String,
    size: u64,
    modified: i64,
}

impl SourceWriter {
    /// Capture complete audited feature inputs on the current retained owner only.
    /// Caller queue wait is included in `deadline`; execution is capped at two seconds.
    /// No target publication, job admission, lease renewal, repair or SQL writes occur.
    pub fn capture_layout_features(
        &self,
        expected: &SourceRevision,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<SourceLayoutCapture, SourceLayoutCaptureError> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        checkpoint(cancel, deadline)?;
        if !self.database.connection.is_autocommit() {
            return Err(SourceLayoutCaptureError::TransactionActive);
        }
        self.capture_layout_features_inner(expected, cancel, deadline)
            .map_err(|error| checkpoint(cancel, deadline).err().unwrap_or(error))
    }

    fn capture_layout_features_inner(
        &self,
        expected: &SourceRevision,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<SourceLayoutCapture, SourceLayoutCaptureError> {
        checkpoint(cancel, deadline)?;
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        self.validate_readiness_revision(expected)?;
        let fence = self.reference_read_fence()?;
        if fence.source_revision() != expected {
            return Err(SourceLayoutCaptureError::Stale);
        }
        let manifest = self.layout_manifest(cancel, deadline)?;
        if manifest.is_empty() {
            return Err(SourceLayoutCaptureError::Empty);
        }
        let generation = i64::try_from(expected.revision())
            .map_err(|_| SourceLayoutCaptureError::InvalidManifest("revision overflow"))?;
        // Reserve the entire fixed row budget. Charge that capacity before allocation,
        // including the private key's target and immutable reference representation.
        let mut owned_bytes = std::mem::size_of::<SourceLayoutCapture>();
        charge(
            &mut owned_bytes,
            MAX_ROWS * std::mem::size_of::<SourceLayoutFeature>(),
        )?;
        charge(
            &mut owned_bytes,
            2 * (expected.source_id().as_str().len()
                + expected.root_identity().as_bytes().len()
                + expected.root_binding_identity().len()
                + expected.database_identity().len())
                + 256,
        )?;
        let mut features = Vec::with_capacity(MAX_ROWS);
        let mut hash = layout_input_hasher(
            [
                expected.source_id().as_str().as_bytes(),
                expected.root_identity().as_bytes(),
                expected.root_binding_identity().as_bytes(),
                expected.database_identity().as_bytes(),
            ],
            manifest.len(),
        );
        for row in manifest {
            checkpoint(cancel, deadline)?;
            if !Path::new(&row.path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
            {
                return Err(SourceLayoutCaptureError::UnsupportedFormat(row.path));
            }
            // Count the row, sample identity, public target, key's private target/key/reference
            // and payload before any target/key/payload copies. Fields are already bounded.
            charge(
                &mut owned_bytes,
                732 + 64
                    + 160
                    + 3 * (row.path.len()
                        + row.identity.len()
                        + row.content.len()
                        + self.source_id.as_str().len()
                        + "features-v1".len())
                    + 2,
            )?;
            let target = ReadinessTarget::file(
                self.source_id.as_str(),
                &row.identity,
                &row.path,
                ReadinessStage::AnalysisFeatures,
                "features-v1",
                generation,
                &row.content,
            );
            self.validate_readiness_command(expected, &target)?;
            let ownership = crate::sample_sources::db::ReadinessOwnership::new(
                self.database.profile_guard.as_ref(),
                self.database.database_root_binding.as_deref(),
                self.database.database_file_binding.as_deref(),
                self.database.source_lease.as_ref(),
            );
            let (key, payload) = crate::sample_sources::readiness::ReadinessStore::lookup_layout_feature_target_owned(
                &self.database.connection, &ownership, &target)
                .map_err(|error| match error {
                    ReadinessError::InvalidFeatureArtifact(reason) => {
                        SourceLayoutCaptureError::FeatureUnavailable {
                            path: row.path.clone(),
                            reason,
                        }
                    }
                    other => SourceLayoutCaptureError::Owner(other.into()),
                })?;
            checkpoint(cancel, deadline)?;
            for field in [
                row.path.as_bytes(),
                row.identity.as_bytes(),
                row.content.as_bytes(),
                target.required_version.as_bytes(),
                key.reference().as_bytes(),
                &row.size.to_le_bytes(),
                &row.modified.to_le_bytes(),
            ] {
                hash_field(&mut hash, field);
            }
            features.push(SourceLayoutFeature {
                sample_identity: format!("{}::{}", self.source_id.as_str(), row.path),
                path: row.path,
                identity: row.identity,
                content_hash: row.content,
                size: row.size,
                modified_ns: row.modified,
                target,
                key,
                payload,
            });
        }
        self.validate_readiness_revision(expected)?;
        fence.validate()?;
        checkpoint(cancel, deadline)?;
        Ok(SourceLayoutCapture {
            fence,
            owner: std::sync::Arc::downgrade(&self.reference_liveness),
            features,
            digest: *hash.finalize().as_bytes(),
            owned_bytes,
        })
    }

    /// Recheck issuer custody and recapture exact membership/keys before consuming a result.
    /// This is a read observation only; publication still requires its own admission/transaction.
    pub fn validate_layout_capture(
        &self,
        capture: &SourceLayoutCapture,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<(), SourceLayoutCaptureError> {
        checkpoint(cancel, deadline)?;
        if !capture
            .owner
            .ptr_eq(&std::sync::Arc::downgrade(&self.reference_liveness))
        {
            return Err(SourceLayoutCaptureError::Stale);
        }
        capture.validate_read_fence()?;
        let current = self.capture_layout_features(capture.revision(), cancel, deadline)?;
        if current.digest != capture.digest {
            return Err(SourceLayoutCaptureError::Stale);
        }
        Ok(())
    }

    fn layout_manifest(
        &self,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Vec<ManifestRow>, SourceLayoutCaptureError> {
        let filter = crate::sample_sources::supported_audio_where_clause();
        let mut stmt = self.database.connection.prepare(&format!("SELECT
            CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB)) BETWEEN 1 AND 1024 THEN path END,
            CASE WHEN typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512 THEN file_identity END,
            CASE WHEN typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB))=64 THEN content_hash END,
            CASE WHEN typeof(file_size)='integer' AND file_size>=0 THEN file_size END,
            CASE WHEN typeof(modified_ns)='integer' THEN modified_ns END,
            CASE WHEN typeof(missing)='integer' AND missing=0 THEN 1 END,
            CASE WHEN typeof(extension)='text' AND length(CAST(extension AS BLOB))<=16 THEN extension END
            FROM wav_files WHERE (typeof(missing)<>'integer' OR missing<>1) AND
            (({filter}) OR typeof(path)<>'text' OR typeof(extension)<>'text' OR
              (lower(substr(path,-4))='.wav' AND path NOT GLOB '._*' AND path NOT GLOB '*/._*'))
            ORDER BY path LIMIT 257"))
            .map_err(|e| sql_error(e, cancel, deadline))?;
        let mut rows = stmt.query([]).map_err(|e| sql_error(e, cancel, deadline))?;
        let mut manifest = Vec::new();
        let mut identities = BTreeSet::new();
        let mut bytes = 0usize;
        while let Some(row) = rows.next().map_err(|e| sql_error(e, cancel, deadline))? {
            checkpoint(cancel, deadline)?;
            if manifest.len() == MAX_ROWS {
                return Err(SourceLayoutCaptureError::Limit("manifest rows"));
            }
            let values = (|| -> rusqlite::Result<_> {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })()
            .map_err(|e| sql_error(e, cancel, deadline))?;
            let (
                Some(path),
                Some(identity),
                Some(content),
                Some(size),
                Some(modified),
                Some(1),
                extension,
            ) = values
            else {
                return Err(SourceLayoutCaptureError::InvalidManifest(
                    "bounded text, hash or scalar facts unavailable",
                ));
            };
            if extension.as_deref().is_none_or(|value| {
                crate::sample_sources::is_supported_audio(Path::new(&path)) && value != "wav"
            }) {
                return Err(SourceLayoutCaptureError::InvalidManifest(
                    "audio classification mismatch",
                ));
            }
            if identity.trim().is_empty()
                || !identities.insert(identity.clone())
                || !content
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(SourceLayoutCaptureError::InvalidManifest(
                    "duplicate identity or invalid full hash",
                ));
            }
            if crate::sample_sources::db::util::normalize_relative_path(Path::new(&path))
                .ok()
                .as_deref()
                != Some(path.as_str())
                || path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || path.contains('\0')
            {
                return Err(SourceLayoutCaptureError::InvalidManifest(
                    "noncanonical path or alias",
                ));
            }
            bytes = bytes
                .checked_add(path.len() + identity.len() + content.len())
                .filter(|n| *n <= MAX_MANIFEST_BYTES)
                .ok_or(SourceLayoutCaptureError::Limit("manifest bytes"))?;
            manifest.push(ManifestRow {
                path,
                identity,
                content,
                size: size as u64,
                modified,
            });
        }
        Ok(manifest)
    }
}
