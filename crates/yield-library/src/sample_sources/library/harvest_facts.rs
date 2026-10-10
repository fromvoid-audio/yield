//! Bounded coherent owner reads; retained metadata never qualifies physical publication.
use super::{HarvestState, LibraryDatabase, LibraryError, ReferenceFileKey};
use crate::app_dirs::WritableProfileGuard;
use crate::operation_journal::StoredRelativePath;
use crate::sample_sources::db::{SourceFileObservation, SourceFileObservationKind, SourceRevision};
use serde::Serialize;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;
pub(super) mod budget;
mod client;
pub use client::HarvestFactsClient;
mod read;
#[cfg(test)]
mod tests;
/// Maximum exact audio paths in one request.
pub const HARVEST_FACTS_MAX_PATHS: usize = 128;
/// Maximum aggregate UTF-8 pathname bytes.
pub const HARVEST_FACTS_MAX_PATH_BYTES: usize = 32768;
/// Maximum complete captured semantic input and result encoding bytes.
pub const HARVEST_FACTS_MAX_BYTES: usize = 65536;
/// Maximum incident graph rows per path; excessive work fails closed.
pub const HARVEST_FACTS_MAX_INCIDENT_EDGES: usize = 64;

/// A cloneable one-way cancel token. Canceled requests/results cannot be revived.
#[derive(Clone, Debug, Default)]
pub struct HarvestFactsCancellation {
    flag: Arc<AtomicBool>,
}
impl HarvestFactsCancellation {
    /// Create a fresh uncanceled request token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Irreversibly cancel all requests/results sharing this token.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
    /// Inspect cancellation without I/O.
    pub fn is_canceled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}
/// Current owner-issued observations and explicit cancellation, never decoded capability bytes.
#[derive(Clone, Debug)]
pub struct HarvestFactsRequest {
    owner: Uuid,
    files: Vec<SourceFileObservation>,
    canceled: HarvestFactsCancellation,
}
impl HarvestFactsRequest {
    /// Bind a finite unique set of current supported audio rows from one audited source epoch.
    pub fn new(
        owner: &WritableProfileGuard,
        files: Vec<SourceFileObservation>,
        canceled: HarvestFactsCancellation,
    ) -> Result<Self, LibraryError> {
        owner.validate_current()?;
        if files.is_empty() || files.len() > HARVEST_FACTS_MAX_PATHS {
            return Err(input());
        }
        let owner = Uuid::parse_str(owner.owner_identity()).map_err(|_| input())?;
        let revision = files[0].source_revision();
        let mut paths = std::collections::BTreeSet::new();
        let mut path_bytes = 0usize;
        let mut bytes = 64usize;
        for f in &files {
            let r = f.source_revision();
            if f.kind() != SourceFileObservationKind::SupportedAudio
                || r != revision
                || !paths.insert(f.path().as_str())
            {
                return Err(input());
            }
            path_bytes = path_bytes
                .checked_add(f.path().as_str().len())
                .ok_or_else(input)?;
            // All captured semantic fields, including repeated source/root/database identity
            // strings, nullable content/object identity and fixed scalar/framing overhead.
            bytes = bytes
                .checked_add(
                    f.path().as_str().len()
                        + r.source_id().as_str().len()
                        + r.root_identity().as_bytes().len()
                        + r.database_identity().len()
                        + r.root_binding_identity().len()
                        + f.file_identity().map_or(0, str::len)
                        + f.content_hash().map_or(0, str::len)
                        + 256,
                )
                .ok_or_else(input)?;
            if path_bytes > HARVEST_FACTS_MAX_PATH_BYTES || bytes > HARVEST_FACTS_MAX_BYTES {
                return Err(input());
            }
        }
        let request = Self {
            owner,
            files,
            canceled,
        };
        request.validate_sources()?;
        Ok(request)
    }
    pub(super) fn validate_sources(&self) -> Result<(), LibraryError> {
        if self.canceled.is_canceled() {
            return Err(LibraryError::HarvestFactsCanceled);
        }
        for file in &self.files {
            let profile = file
                .profile_acquisition_identity()
                .ok_or(LibraryError::HarvestFactsSourceChanged)?;
            if profile != self.owner.to_string() {
                return Err(LibraryError::HarvestFactsStaleOwner);
            }
        }
        if !SourceFileObservation::validate_harvest_batch(&self.files, || {
            !self.canceled.is_canceled()
        })
        .map_err(|_| LibraryError::HarvestFactsSourceChanged)?
        {
            return Err(LibraryError::HarvestFactsCanceled);
        }
        if self.canceled.is_canceled() {
            return Err(LibraryError::HarvestFactsCanceled);
        }
        Ok(())
    }
}
/// One exact historical result. Untracked entries explicitly mean New, not an unknown state.
#[derive(Clone, Debug)]
pub struct HarvestFactsEntry {
    path: StoredRelativePath,
    state: HarvestState,
    tracked: bool,
    derivatives: u64,
}
impl HarvestFactsEntry {
    /// Exact requested source-relative audio path.
    pub fn path(&self) -> &StoredRelativePath {
        &self.path
    }
    /// Exact recognized state, or New for an untracked row.
    pub fn state(&self) -> HarvestState {
        self.state
    }
    /// Whether a matching verified Harvest file row was stored.
    pub fn is_tracked(&self) -> bool {
        self.tracked
    }
    /// Exact stored immediate derivative count within the finite query budget.
    pub fn immediate_derivative_count(&self) -> u64 {
        self.derivatives
    }
}
/// Coherent global read plus source epoch and a weak real-worker publication generation.
/// It keeps neither the profile lock nor the source/global writer alive.
#[derive(Clone, Debug)]
pub struct HarvestFactsBatch {
    entries: Vec<HarvestFactsEntry>,
    source: SourceRevision,
    owner: Uuid,
    generation_id: Uuid,
    generation: Weak<Generation>,
    canceled: HarvestFactsCancellation,
}
impl HarvestFactsBatch {
    /// Borrow bounded historical state/count pairs.
    pub fn entries(&self) -> &[HarvestFactsEntry] {
        &self.entries
    }
    /// Exact source/root/database/lifecycle/lease/audited revision of the request.
    pub fn source_revision(&self) -> &SourceRevision {
        &self.source
    }
    /// Historical receiving profile acquisition (correlation, not authority).
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
    /// Acquisition-local actual worker attempt-generation identity, not a durable DB revision.
    pub fn generation_identity(&self) -> Uuid {
        self.generation_id
    }
    /// Cheap global generation/cancellation check, with no SQLite/filesystem/mutex I/O.
    /// Callers must additionally fence their source/browser/directory/profile projection.
    pub fn is_current_global_generation(&self) -> bool {
        !self.canceled.is_canceled() && self.generation.strong_count() != 0
    }
}
/// Only run_owner retains a strong generation. Replacement precedes each Harvest write attempt.
#[derive(Debug)]
pub(super) struct Generation {
    identity: Uuid,
}
impl Generation {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            identity: Uuid::new_v4(),
        })
    }
}
fn input() -> LibraryError {
    LibraryError::HarvestFactsInput
}
