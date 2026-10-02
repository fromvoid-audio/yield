//! Immutable bounded feature payload values; keys do not grant source authority.

use super::{ReadinessError, ReadinessScopeKind, ReadinessStage, ReadinessTarget};

/// A validated v1 vector, encoded as 183 little-endian finite f32 values.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureArtifactPayload(pub(super) Vec<u8>);

impl FeatureArtifactPayload {
    /// Validate and encode a v1 feature vector outside the source transaction.
    pub fn from_v1_vector(vector: &[f32]) -> Result<Self, ReadinessError> {
        if vector.len() != 183 || vector.iter().any(|value| !value.is_finite()) {
            return Err(ReadinessError::InvalidFeatureArtifact("invalid v1 vector"));
        }
        Ok(Self(
            vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        ))
    }

    /// Return the exact immutable validated bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub(super) fn decode(bytes: Vec<u8>) -> Option<Self> {
        (bytes.len() == 732
            && bytes
                .chunks_exact(4)
                .all(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")).is_finite()))
        .then_some(Self(bytes))
    }
}

/// An exact immutable artifact identity issued after an owner transaction.
/// This value is not a capability; lookup revalidates the current owner and target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureArtifactKey {
    pub(super) target: ReadinessTarget,
    pub(super) target_key: String,
    pub(super) digest: [u8; 32],
    pub(super) reference: String,
}

impl FeatureArtifactKey {
    pub(super) fn for_payload(
        target: &ReadinessTarget,
        payload: &FeatureArtifactPayload,
    ) -> Result<Self, ReadinessError> {
        if target.scope_kind != ReadinessScopeKind::File
            || target.stage != ReadinessStage::AnalysisFeatures
            || target.required_version != "features-v1"
        {
            return Err(ReadinessError::InvalidFeatureArtifact(
                "unsupported feature target",
            ));
        }
        if target
            .relative_path
            .as_ref()
            .is_some_and(|path| path.len() > 4096)
        {
            return Err(ReadinessError::InvalidFeatureArtifact(
                "oversized relative path",
            ));
        }
        let mut identity = Vec::new();
        for field in [
            &target.source_id,
            &target.scope_id,
            &target.content_generation,
            &target.required_version,
        ] {
            if field.trim().is_empty() || field.len() > 1024 {
                return Err(ReadinessError::InvalidFeatureArtifact(
                    "invalid identity field",
                ));
            }
            identity.extend_from_slice(&(field.len() as u32).to_le_bytes());
            identity.extend_from_slice(field.as_bytes());
        }
        let target_key = blake3::hash(&identity).to_hex().to_string();
        let digest = *blake3::hash(payload.bytes()).as_bytes();
        let reference = format!(
            "source-feature-v1:{target_key}:{}",
            blake3::Hash::from(digest).to_hex()
        );
        let mut identity_target = target.clone();
        identity_target.relative_path = None;
        identity_target.source_generation = 0;
        identity_target.eligibility = super::ReadinessEligibility::Eligible;
        Ok(Self {
            target: identity_target,
            target_key,
            digest,
            reference,
        })
    }

    pub(crate) fn target(&self) -> &ReadinessTarget {
        &self.target
    }

    /// Return the immutable persisted reference for diagnostics/readiness association.
    pub fn reference(&self) -> &str {
        &self.reference
    }
}

/// Result of the real payload/readiness transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureArtifactPublication {
    /// Payload and readiness completion were atomically recorded.
    Recorded(FeatureArtifactKey),
    /// Exact completed claim/payload retry returned the retained result.
    Duplicate(FeatureArtifactKey),
    /// Target or claim is stale; no payload or ready state was written.
    RejectedStale,
}

/// A feature job claim bound to the exact retained SourceWriter instance.
/// Reacquiring a source owner cannot authorize an old worker's completion.
#[derive(Clone, Debug)]
pub struct FeatureArtifactClaim {
    pub(crate) claim: super::ClaimedReadinessWork,
    pub(crate) owner: std::sync::Weak<()>,
}

impl FeatureArtifactClaim {
    /// Borrow the exact readiness claim for lease renewal, failure or cancellation.
    pub fn readiness_claim(&self) -> &super::ClaimedReadinessWork {
        &self.claim
    }
}

/// Result of one retained owner's exact current feature rebuild admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureArtifactRebuildAdmission {
    /// Verified bytes and matching completion already exist.
    Ready,
    /// An exact broken completion/work entry was made actionable atomically.
    Admitted {
        /// Whether a demonstrably corrupt current-key payload was removed.
        removed_corrupt_payload: bool,
    },
    /// Ordinary absent work retains its existing queue/retry policy.
    Unchanged,
    /// An exact unexpired running claim must finish or expire before rebuilding.
    Deferred,
    /// Ownership/control provenance is ambiguous; evidence was retained.
    Ambiguous,
    /// Current authoritative target/manifest no longer matches.
    RejectedStale,
}

/// Bounded fault injection on one retained owner's current feature target, for real integration tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug)]
pub enum FeatureArtifactFixtureFault {
    /// Simulate a cache row disappearing after completion.
    MissingPayload,
    /// Corrupt the payload while retaining its immutable reference and digest.
    CorruptPayload,
    /// Remove completion evidence while retaining healthy immutable bytes.
    MissingMarker,
}
