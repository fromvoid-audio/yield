//! Held, bounded, content-verified source input for a feature job.

use crate::sample_sources::SourceRevision;
use crate::sample_sources::db::{
    SourcePublicationFence,
    file_ops_journal::{BoundDatabaseRoot, HeldIndexFile},
};
use std::{
    io::Read,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Encoded source input bound (decoded audio has an independent bound).
pub const FEATURE_INPUT_MAX_BYTES: u64 = 8 * 1024 * 1024;
static HELD: AtomicUsize = AtomicUsize::new(0);
pub(crate) struct InputPermit;
impl InputPermit {
    pub(crate) fn acquire() -> Result<Self, SourceFeatureInputError> {
        HELD.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map(|_| Self)
        .map_err(|_| SourceFeatureInputError::Busy)
    }
}
impl Drop for InputPermit {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bounded source input failure; no pathname or unbounded backend diagnostics escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceFeatureInputError {
    /// No free held input permit.
    #[error("feature input busy")]
    Busy,
    /// Source facts or root/owner no longer match.
    #[error("feature input changed or unavailable")]
    Changed,
    /// This platform has no implemented held-read primitive.
    #[error("feature input primitive unsupported on this platform")]
    UnsupportedPlatform,
    /// The encoded bytes exceed the admitted input limit.
    #[error("feature input byte limit")]
    ByteLimit,
    /// Work was cancelled before completion.
    #[error("feature input cancelled")]
    Cancelled,
    /// Current committed content hash differs from the held file bytes.
    #[error("feature input content changed")]
    ContentChanged,
}

/// Private root/descriptor custody, without a pathname or writable handle.
pub struct SourceFeatureInput {
    pub(crate) held: HeldIndexFile,
    pub(crate) root: Arc<BoundDatabaseRoot>,
    pub(crate) fence: SourcePublicationFence,
    pub(crate) owner: Weak<()>,
    pub(crate) revision: SourceRevision,
    pub(crate) identity: String,
    pub(crate) size: u64,
    pub(crate) modified: i64,
    pub(crate) content: String,
    pub(crate) _permit: InputPermit,
}
impl SourceFeatureInput {
    /// Revalidate owner, root and every named descriptor edge before publication.
    pub fn validate(&self) -> Result<(), SourceFeatureInputError> {
        if self.owner.upgrade().is_none() {
            return Err(SourceFeatureInputError::Changed);
        }
        self.fence
            .validate()
            .map_err(|_| SourceFeatureInputError::Changed)?;
        // Keep the retained media root alive for the whole read/validation boundary.
        let _ = &self.root;
        self.held
            .validate_facts(Some(&self.identity), Some(self.size), Some(self.modified))
            .map_err(|e| match e {
                crate::sample_sources::db::SourceIndexInspectionError::Unsupported => {
                    SourceFeatureInputError::UnsupportedPlatform
                }
                _ => SourceFeatureInputError::Changed,
            })
    }

    /// Read bounded immutable bytes and verify their whole-file scanner BLAKE3 identity.
    /// Call this from a compute worker with no source SQL transaction held.
    pub fn read_verified_bytes(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, SourceFeatureInputError> {
        self.validate()?;
        let mut file = self
            .held
            .open_for_feature_read()
            .map_err(|_| SourceFeatureInputError::Changed)?;
        let mut bytes = Vec::with_capacity(self.size as usize);
        let mut buffer = [0u8; 32 * 1024];
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(SourceFeatureInputError::Cancelled);
            }
            let count = file
                .read(&mut buffer)
                .map_err(|_| SourceFeatureInputError::Changed)?;
            if count == 0 {
                break;
            }
            if bytes.len().saturating_add(count) as u64 > FEATURE_INPUT_MAX_BYTES {
                return Err(SourceFeatureInputError::ByteLimit);
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        self.validate()?;
        if bytes.len() as u64 != self.size || blake3::hash(&bytes).to_hex().as_str() != self.content
        {
            return Err(SourceFeatureInputError::ContentChanged);
        }
        Ok(bytes)
    }
}
