//! Owner-held, read-only index inspection. No native target or filesystem handle is public.
use super::file_ops_journal::{BoundDatabaseRoot, HeldIndexFile};
use super::{SourceDatabase, SourceIndexClassification, SourceIndexEntry, SourcePublicationFence};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Global bound including queued and consumer-retained results (clones share one permit).
pub const MAX_HELD_INDEX_INSPECTIONS: usize = 8;
static HELD: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Permit {
    fn acquire() -> Result<Self, SourceIndexInspectionError> {
        HELD.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_HELD_INDEX_INSPECTIONS).then_some(n + 1)
        })
        .map(|_| Self)
        .map_err(|_| SourceIndexInspectionError::Busy)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Opaque exact retained row proposal, minted only by a bounded database page.
#[derive(Clone, Debug)]
pub struct SourceIndexEntryHandle {
    binding: Arc<()>,
    _delivery: Arc<()>,
    revision: u64,
    entry: Arc<SourceIndexEntry>,
}
impl SourceIndexEntryHandle {
    pub(super) fn new(
        binding: Arc<()>,
        delivery: Arc<()>,
        revision: u64,
        entry: SourceIndexEntry,
    ) -> Self {
        Self {
            binding,
            _delivery: delivery,
            revision,
            entry: Arc::new(entry),
        }
    }
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }
    pub(super) fn entry(&self) -> &SourceIndexEntry {
        &self.entry
    }
    /// Bounded variable input retained by this opaque proposal.
    pub fn input_bytes(&self) -> usize {
        self.entry
            .relative_path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .saturating_add(self.entry.file_identity.as_ref().map_or(0, String::len))
    }
}

/// Explicit inspection failure; diagnostic text never grows with filesystem/database errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SourceIndexInspectionError {
    /// The handle belongs to another or reopened connection.
    #[error("foreign index entry handle")]
    ForeignHandle,
    /// Retained index revision changed.
    #[error("index revision changed")]
    StaleRevision,
    /// Exact retained row or held filesystem object changed.
    #[error("index entry changed")]
    ChangedEntry,
    /// Variable facts exceed fixed admission limits.
    #[error("index inspection byte limit exceeded")]
    ByteLimit,
    /// Locator escapes, is malformed or has too many components.
    #[error("invalid index entry path")]
    InvalidPath,
    /// Inaccessible/unknown facts cannot authorize inspection.
    #[error("index entry identity unavailable")]
    Unavailable,
    /// Root, source lease or profile no longer validates.
    #[error("index inspection owner unavailable")]
    OwnerUnavailable,
    /// No free permit for a held inspection.
    #[error("held index inspection limit reached")]
    Busy,
    /// Primitive is not implemented on this platform.
    #[error("descriptor inspection unsupported on this platform")]
    Unsupported,
    /// The bounded exact database read failed.
    #[error("index inspection database read failed")]
    Database,
}

/// Private descriptor/root custody, with observations only. This is not a reveal grant.
pub struct SourceIndexInspection {
    held: HeldIndexFile,
    root: Arc<BoundDatabaseRoot>,
    fence: SourcePublicationFence,
    handle: SourceIndexEntryHandle,
    _permit: Permit,
}
impl std::fmt::Debug for SourceIndexInspection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceIndexInspection")
            .field("index_revision", &self.handle.revision)
            .finish_non_exhaustive()
    }
}
impl SourceIndexInspection {
    /// Descriptor-validated retained facts, without exposing a target or dispatch method.
    pub fn entry(&self) -> &SourceIndexEntry {
        self.handle.entry()
    }
    /// Independent revision admitted by the registered database owner.
    pub fn index_revision(&self) -> u64 {
        self.handle.revision
    }
    /// Cheap delivery cancellation check only; not current filesystem or native authority.
    pub fn is_live(&self) -> bool {
        self.fence.is_live()
    }
}
impl SourceDatabase {
    /// Inspect an owner-minted exact row through the retained media-root descriptor.
    /// No data is read, no directory is enumerated, and no absolute/native target is returned.
    pub fn inspect_index_entry(
        &self,
        handle: &SourceIndexEntryHandle,
    ) -> Result<SourceIndexInspection, SourceIndexInspectionError> {
        use SourceIndexInspectionError as E;
        if !Arc::ptr_eq(&handle.binding, &self.index_page_binding) {
            return Err(E::ForeignHandle);
        }
        if handle
            .entry
            .relative_path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            > 1024
            || handle
                .entry
                .file_identity
                .as_ref()
                .is_some_and(|id| id.len() > 512)
        {
            return Err(E::ByteLimit);
        }
        let fence = self
            .source_publication_fence()
            .map_err(|_| E::OwnerUnavailable)?;
        fence.validate().map_err(|_| E::OwnerUnavailable)?;
        let entry = self.reread_inspection_entry(handle)?;
        if entry.classification == SourceIndexClassification::Inaccessible
            || entry.file_size.is_none()
            || entry.modified_ns.is_none()
            || entry.file_identity.as_ref().is_none_or(String::is_empty)
        {
            return Err(E::Unavailable);
        }
        let permit = Permit::acquire()?;
        let root = self
            .source_lease
            .as_ref()
            .ok_or(E::OwnerUnavailable)?
            .media_root_binding();
        let held = root.inspect_index_file(&entry.relative_path)?;
        let inspection = SourceIndexInspection {
            held,
            root,
            fence,
            handle: handle.clone(),
            _permit: permit,
        };
        self.validate_index_inspection(&inspection)?;
        Ok(inspection)
    }
    /// Revalidate retained descriptor edges, exact row/revision and source custody at owner publication.
    /// This still does not authorize native dispatch.
    pub fn validate_index_inspection(
        &self,
        inspection: &SourceIndexInspection,
    ) -> Result<(), SourceIndexInspectionError> {
        use SourceIndexInspectionError as E;
        if !Arc::ptr_eq(&inspection.handle.binding, &self.index_page_binding) {
            return Err(E::ForeignHandle);
        }
        self.source_publication_fence()
            .map_err(|_| E::OwnerUnavailable)?;
        inspection
            .fence
            .validate()
            .map_err(|_| E::OwnerUnavailable)?;
        let root = self
            .source_lease
            .as_ref()
            .ok_or(E::OwnerUnavailable)?
            .media_root_binding();
        if !Arc::ptr_eq(&root, &inspection.root) {
            return Err(E::OwnerUnavailable);
        }
        let entry = self.reread_inspection_entry(&inspection.handle)?;
        inspection.held.validate(&entry)?;
        inspection
            .fence
            .validate()
            .map_err(|_| E::OwnerUnavailable)?;
        Ok(())
    }
}
