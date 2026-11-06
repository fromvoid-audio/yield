//! Held, bounded, content-verified source input for a feature job.

use crate::sample_sources::SourceRevision;
use crate::sample_sources::db::{
    SourcePublicationFence,
    file_ops_journal::{BoundDatabaseRoot, HeldIndexFile},
};
use std::{
    io::{self, BufReader, Read},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Encoded source input bound (decoded audio has an independent bound).
pub const FEATURE_INPUT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Encoded length limit for the dedicated held streaming input.
pub const TRANSIENT_STREAM_MAX_BYTES: u64 = 256 * 1024 * 1024;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
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

/// Failure while reading or processing a held Source audio stream.
#[derive(Debug)]
pub enum SourceAudioStreamError<E> {
    /// Source admission, cancellation, or verification failed.
    Input(SourceFeatureInputError),
    /// The consumer could not process the admitted audio bytes.
    Processing(E),
}

impl<E> From<SourceFeatureInputError> for SourceAudioStreamError<E> {
    fn from(error: SourceFeatureInputError) -> Self {
        Self::Input(error)
    }
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
        if self.size > FEATURE_INPUT_MAX_BYTES {
            return Err(SourceFeatureInputError::ByteLimit);
        }
        self.with_verified_audio_reader(cancel, |reader| {
            let mut bytes = Vec::with_capacity(self.size as usize);
            reader.take(self.size).read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        })
        .map_err(|error| match error {
            SourceAudioStreamError::Input(error) => error,
            SourceAudioStreamError::Processing(_) => SourceFeatureInputError::Changed,
        })
    }

    /// Process a held read-only stream on a compute worker, outside any Source transaction.
    ///
    /// A successful callback may stop early. Its result is returned only after consuming
    /// the remaining encoded bytes, verifying the full scanner digest and revalidating
    /// the held file. Callback work must remain private until this method succeeds.
    pub fn with_verified_audio_reader<T, E>(
        &self,
        cancel: &AtomicBool,
        process: impl FnOnce(&mut dyn Read) -> Result<T, E>,
    ) -> Result<T, SourceAudioStreamError<E>> {
        if cancel.load(Ordering::Acquire) {
            return Err(SourceFeatureInputError::Cancelled.into());
        }
        self.validate()?;
        let file = self
            .held
            .open_for_feature_read()
            .map_err(|_| SourceFeatureInputError::Changed)?;
        let mut reader = BufReader::with_capacity(
            STREAM_BUFFER_BYTES,
            VerifyingRead {
                file,
                remaining: self.size,
                digest: blake3::Hasher::new(),
                cancel,
                failure: None,
            },
        );
        let result = process(&mut reader);
        if let Some(failure) = reader.get_ref().failure {
            return Err(failure.into());
        }
        if cancel.load(Ordering::Acquire) {
            return Err(SourceFeatureInputError::Cancelled.into());
        }
        let output = result.map_err(SourceAudioStreamError::Processing)?;
        io::copy(&mut reader, &mut io::sink()).map_err(|_| {
            reader
                .get_ref()
                .failure
                .unwrap_or(SourceFeatureInputError::Changed)
        })?;
        let checked = reader.into_inner();
        self.validate()?;
        if cancel.load(Ordering::Acquire) {
            return Err(SourceFeatureInputError::Cancelled.into());
        }
        if checked.remaining != 0 || checked.digest.finalize().to_hex().as_str() != self.content {
            return Err(SourceFeatureInputError::ContentChanged.into());
        }
        Ok(output)
    }
}

struct VerifyingRead<'a> {
    file: std::fs::File,
    remaining: u64,
    digest: blake3::Hasher,
    cancel: &'a AtomicBool,
    failure: Option<SourceFeatureInputError>,
}

impl VerifyingRead<'_> {
    fn fail(&mut self, reason: SourceFeatureInputError) -> io::Error {
        self.failure = Some(reason);
        io::Error::other(reason)
    }
}

impl Read for VerifyingRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(failure) = self.failure {
            return Err(io::Error::other(failure));
        }
        if self.cancel.load(Ordering::Acquire) {
            return Err(self.fail(SourceFeatureInputError::Cancelled));
        }
        // Read at most one byte beyond the admitted length to detect growth.
        let limit = buffer.len().min((self.remaining + 1) as usize);
        let count = match self.file.read(&mut buffer[..limit]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
            Err(_) => return Err(self.fail(SourceFeatureInputError::Changed)),
        };
        if count as u64 > self.remaining {
            return Err(self.fail(SourceFeatureInputError::ContentChanged));
        }
        self.remaining -= count as u64;
        self.digest.update(&buffer[..count]);
        Ok(count)
    }
}
