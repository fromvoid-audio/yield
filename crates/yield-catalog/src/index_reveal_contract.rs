//! Bounded index-only reveal proposals. These observations never authorize native dispatch.
use std::path::Path;
use yield_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use yield_library::sample_sources::{SourceId, SourceIndexClassification, SourceIndexEntry};

/// Maximum lossless relative-path bytes in one reveal proposal.
pub const INDEX_REVEAL_MAX_PATH_BYTES: usize = 1024;
/// Maximum source/root observation bytes each, checked before cloning.
pub const INDEX_REVEAL_MAX_BINDING_BYTES: usize = 4096;
/// Maximum retained object-identity bytes in a proposal.
pub const INDEX_REVEAL_MAX_OBJECT_BYTES: usize = 512;

/// Observed page identity, not a source lease, filesystem capability or delivery grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRevealPageIdentity {
    /// Registered stable source.
    pub source_id: SourceId,
    /// Registered source lifecycle.
    pub lifecycle_generation: u64,
    /// Retained registered root observation; production admission revalidates its custody.
    pub root_identity: RootIdentity,
    /// Independent retained index revision.
    pub index_revision: u64,
    /// Current caller query identity.
    pub query_id: u64,
    /// Owner-issued page delivery identity proposed for production entry handles.
    pub delivery_id: u64,
}

/// A bounded request for owner inspection, with no absolute path or dispatch method.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRevealProposal {
    page: IndexRevealPageIdentity,
    entry: SourceIndexEntry,
    request_id: u64,
}

impl IndexRevealProposal {
    /// Borrow the observations the production owner must independently validate.
    pub fn page(&self) -> &IndexRevealPageIdentity {
        &self.page
    }
    /// Borrow exact retained facts; these do not establish current object identity.
    pub fn entry(&self) -> &SourceIndexEntry {
        &self.entry
    }
    /// Caller request identity used to discard late/canceled inspection completions.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }
}

/// Typed rejection before owner inspection. No failure implies an empty/current scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IndexRevealRejection {
    /// The modal/request was canceled or superseded.
    #[error("index reveal request canceled")]
    Canceled,
    /// Stable source, lifecycle, root, query, page delivery or index revision changed.
    #[error("index reveal page identity changed")]
    StalePage,
    /// A variable field exceeds the proposal's fixed input bound.
    #[error("index reveal proposal exceeds its byte bound")]
    InputTooLarge,
    /// Root or retained object identity/file facts are unknown.
    #[error("index reveal identity is unavailable")]
    IdentityUnavailable,
    /// An inaccessible/ambiguous retained entry is displayable but not eligible for reveal.
    #[error("retained entry cannot be safely inspected for reveal")]
    Inaccessible,
    /// The path is not an exact validated lossless source-relative locator.
    #[error("invalid index reveal relative path")]
    InvalidPath,
}

/// Draft a bounded proposal against the currently displayed observations.
///
/// This performs no filesystem/SQLite/native work and does not trust `current` as a live
/// capability. A successful proposal still requires the registered owner's private delivery
/// handle, exact database re-read, retained lease/root validation and held-object inspection.
/// Production native dispatch cannot be authorized by this function or a fixture calling it.
pub fn propose_index_reveal(
    displayed: &IndexRevealPageIdentity,
    current: &IndexRevealPageIdentity,
    entry: &SourceIndexEntry,
    request_id: u64,
    canceled: bool,
) -> Result<IndexRevealProposal, IndexRevealRejection> {
    if canceled {
        return Err(IndexRevealRejection::Canceled);
    }
    for page in [displayed, current] {
        if page.source_id.as_str().len() > INDEX_REVEAL_MAX_BINDING_BYTES
            || page.root_identity.as_bytes().len() > INDEX_REVEAL_MAX_BINDING_BYTES
        {
            return Err(IndexRevealRejection::InputTooLarge);
        }
    }
    if displayed != current {
        return Err(IndexRevealRejection::StalePage);
    }
    let path = entry.relative_path.as_os_str().as_encoded_bytes();
    if path.len() > INDEX_REVEAL_MAX_PATH_BYTES
        || entry
            .file_identity
            .as_ref()
            .is_some_and(|identity| identity.len() > INDEX_REVEAL_MAX_OBJECT_BYTES)
    {
        return Err(IndexRevealRejection::InputTooLarge);
    }
    if !valid_relative_path(&entry.relative_path) {
        return Err(IndexRevealRejection::InvalidPath);
    }
    if entry.classification == SourceIndexClassification::Inaccessible {
        return Err(IndexRevealRejection::Inaccessible);
    }
    if displayed.root_identity.as_bytes().is_empty()
        || displayed.source_id.as_str().is_empty()
        || entry.file_identity.as_ref().is_none_or(String::is_empty)
        || entry.file_size.is_none()
        || entry.modified_ns.is_none()
    {
        return Err(IndexRevealRejection::IdentityUnavailable);
    }
    Ok(IndexRevealProposal {
        page: displayed.clone(),
        entry: entry.clone(),
        request_id,
    })
}

fn valid_relative_path(path: &Path) -> bool {
    !path.as_os_str().as_encoded_bytes().contains(&0)
        && RootRelativePath::try_from_path(path.to_path_buf()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use yield_library::sample_sources::SourceIndexDiagnostic;

    fn fixtures() -> (IndexRevealPageIdentity, SourceIndexEntry) {
        (
            IndexRevealPageIdentity {
                source_id: SourceId::from_string("source"),
                lifecycle_generation: 2,
                root_identity: RootIdentity::from_bytes(vec![1, 2]),
                index_revision: 3,
                query_id: 4,
                delivery_id: 5,
            },
            SourceIndexEntry {
                relative_path: PathBuf::from("日本語.txt"),
                classification: SourceIndexClassification::UnsupportedNonAudio,
                file_size: Some(10),
                modified_ns: Some(12),
                file_identity: Some("object-a".into()),
                diagnostic: None,
                format_policy_version: 1,
            },
        )
    }

    #[test]
    fn reveal_proposal_preserves_exact_facts_without_a_native_target() {
        let (page, entry) = fixtures();
        let proposal = propose_index_reveal(&page, &page, &entry, 9, false).unwrap();
        assert_eq!(proposal.page(), &page);
        assert_eq!(proposal.entry(), &entry);
        assert_eq!(proposal.request_id(), 9);
        assert!(!proposal.entry().relative_path.is_absolute());
    }

    #[test]
    fn every_page_binding_and_cancellation_rejects_stale_proposals() {
        let (page, entry) = fixtures();
        let mut changed = vec![page.clone(); 6];
        changed[0].source_id = SourceId::from_string("replacement");
        changed[1].lifecycle_generation += 1;
        changed[2].root_identity = RootIdentity::from_bytes(vec![9]);
        changed[3].index_revision += 1;
        changed[4].query_id += 1;
        changed[5].delivery_id += 1;
        for current in changed {
            assert_eq!(
                propose_index_reveal(&page, &current, &entry, 9, false),
                Err(IndexRevealRejection::StalePage)
            );
        }
        assert_eq!(
            propose_index_reveal(&page, &page, &entry, 9, true),
            Err(IndexRevealRejection::Canceled)
        );
    }

    #[test]
    fn inaccessible_unknown_escaping_and_oversized_facts_cannot_be_proposals() {
        let (page, entry) = fixtures();
        let mut inaccessible = entry.clone();
        inaccessible.classification = SourceIndexClassification::Inaccessible;
        inaccessible.diagnostic = Some(SourceIndexDiagnostic::OpenUnavailable);
        assert_eq!(
            propose_index_reveal(&page, &page, &inaccessible, 1, false),
            Err(IndexRevealRejection::Inaccessible)
        );
        let mut missing = entry.clone();
        missing.file_identity = None;
        assert_eq!(
            propose_index_reveal(&page, &page, &missing, 1, false),
            Err(IndexRevealRejection::IdentityUnavailable)
        );
        for path in ["../escape", "/absolute", "", "bad\0path"] {
            let mut bad = entry.clone();
            bad.relative_path = path.into();
            assert_eq!(
                propose_index_reveal(&page, &page, &bad, 1, false),
                Err(IndexRevealRejection::InvalidPath)
            );
        }
        let mut huge = entry.clone();
        huge.relative_path = "x".repeat(INDEX_REVEAL_MAX_PATH_BYTES + 1).into();
        assert_eq!(
            propose_index_reveal(&page, &page, &huge, 1, false),
            Err(IndexRevealRejection::InputTooLarge)
        );
        huge = entry;
        huge.file_identity = Some("x".repeat(INDEX_REVEAL_MAX_OBJECT_BYTES + 1));
        assert_eq!(
            propose_index_reveal(&page, &page, &huge, 1, false),
            Err(IndexRevealRejection::InputTooLarge)
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_path_proposal_keeps_bytes_and_never_substitutes_lossy_unicode() {
        use std::os::unix::ffi::OsStringExt;
        let (page, mut entry) = fixtures();
        entry.relative_path = std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec()).into();
        let proposal = propose_index_reveal(&page, &page, &entry, 1, false).unwrap();
        assert_eq!(
            proposal
                .entry()
                .relative_path
                .as_os_str()
                .as_encoded_bytes(),
            b"raw-\xff.txt"
        );
    }
}
