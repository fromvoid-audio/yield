//! Durable source-local curation commands and their bounded cache patches.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use thiserror::Error;
use yield_library::app_dirs::WritableProfileGuard;
use yield_library::sample_sources::db::BrowserMetadataCursor;
use yield_library::sample_sources::reconciliation::RootIdentity;
use yield_library::sample_sources::{
    BrowserFileMetadata, SampleCollection, SourceCurationCommitReceipt, SourceCurationUpdate,
    SourceDatabase, SourceManifestEntry, SourceWriterError,
};

use super::{
    AdmissionState, CatalogCommand, CatalogError, CatalogEvent, EventReservation, RegisteredSource,
    SourceId, source_root_identity,
};

/// Maximum number of updates admitted by one durable curation command.
pub const MAX_CURATION_UPDATES: usize = yield_library::sample_sources::SOURCE_CURATION_BATCH_LIMIT;

/// Maximum number of library-sized curation batches admitted by one operation.
pub const MAX_CURATION_OPERATION_BATCHES: usize = 512;

/// Maximum number of metadata updates admitted by one compound curation operation.
pub const MAX_CURATION_OPERATION_UPDATES: usize = 65_536;

/// Maximum aggregate source-relative path bytes retained by one compound operation.
pub const MAX_CURATION_OPERATION_PATH_BYTES: usize = 16 * 1024 * 1024;

/// Maximum estimated metadata bytes retained by one compound operation outcome.
pub const MAX_CURATION_OPERATION_RECEIPT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum UTF-8 byte length of a curation tag label before identity normalization.
pub const MAX_CURATION_TAG_LABEL_BYTES: usize =
    yield_library::sample_sources::SOURCE_CURATION_MAX_TAG_LABEL_BYTES;

// This is deliberately conservative: it covers the fixed metadata row plus vector and batch
// bookkeeping while the input batches and aggregate receipt coexist in the worker. Variable tag
// label bytes are accounted separately from this fixed allowance.
const ESTIMATED_RECEIPT_ROW_BYTES: usize = 256;
const ESTIMATED_OPERATION_BATCH_BYTES: usize = 64;
const ESTIMATED_EXISTING_TAG_ID_BYTES: usize = std::mem::size_of::<i64>();
// Path payloads and tag labels are accounted separately; tag labels remain in input updates while
// collection membership is the only variable receipt-row payload and has six fixed slots. Keep
// scalar command additions within the row allowance.
const _: () = assert!(
    std::mem::size_of::<SourceCurationUpdate>()
        + std::mem::size_of::<BrowserFileMetadata>()
        + SampleCollection::COUNT * std::mem::size_of::<SampleCollection>()
        <= ESTIMATED_RECEIPT_ROW_BYTES
);

/// Revision fence for a compound curation operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceCurationOperationFence {
    /// Start a new curation lineage at this exact source revision.
    ExactRevision(u64),
    /// Continue only the exact latest fully successful operation in this source lifecycle.
    AfterOperation {
        /// Request identifier of the immediately preceding successful operation.
        request_id: u64,
    },
}

/// Failure detail for one batch in a compound curation operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCurationOperationFailure {
    batch_index: usize,
    error: CatalogError,
}

impl SourceCurationOperationFailure {
    /// Return the zero-based batch index that failed.
    pub const fn batch_index(&self) -> usize {
        self.batch_index
    }

    /// Borrow the typed failure returned for the failed batch.
    pub const fn error(&self) -> &CatalogError {
        &self.error
    }
}

/// Bounded terminal result for one compound curation operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCurationOperationOutcome {
    initial_revision: Option<u64>,
    final_revision: Option<u64>,
    committed_batches: Vec<SourceCurationCommitReceipt>,
    failure: Option<SourceCurationOperationFailure>,
}

impl SourceCurationOperationOutcome {
    /// Return the exact starting revision when the operation could resolve its fence.
    pub const fn initial_revision(&self) -> Option<u64> {
        self.initial_revision
    }

    /// Return the last committed batch revision, or the initial fence if no batch committed.
    ///
    /// This is not a database readback. On a zero-commit failure the initial fence may be stale;
    /// only revisions carried by [`Self::committed_batches`] are durable commit receipts.
    pub const fn final_revision(&self) -> Option<u64> {
        self.final_revision
    }

    /// Borrow the durable receipts for every successfully committed prefix batch.
    pub fn committed_batches(&self) -> &[SourceCurationCommitReceipt] {
        &self.committed_batches
    }

    /// Borrow the failed batch detail, if the operation did not complete fully.
    pub const fn failure(&self) -> Option<&SourceCurationOperationFailure> {
        self.failure.as_ref()
    }

    /// Return whether every admitted batch committed successfully.
    pub const fn is_success(&self) -> bool {
        self.failure.is_none()
    }
}

/// A typed failure raised before a durable curation command enters the worker queue.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogCurationRequestError {
    /// No metadata updates were supplied.
    #[error("curation request is empty")]
    Empty,
    /// The request exceeded the bounded update admission limit.
    #[error("curation request contains {actual} updates; maximum is {max}")]
    TooManyUpdates {
        /// Maximum admitted update count.
        max: usize,
        /// Supplied update count.
        actual: usize,
    },
    /// The operation contained more library-sized batches than admission allows.
    #[error("curation operation contains {actual} batches; maximum is {max}")]
    TooManyOperationBatches {
        /// Maximum admitted batch count.
        max: usize,
        /// Supplied batch count.
        actual: usize,
    },
    /// The operation contained more updates than admission allows.
    #[error("curation operation contains {actual} updates; maximum is {max}")]
    TooManyOperationUpdates {
        /// Maximum admitted update count.
        max: usize,
        /// Supplied update count.
        actual: usize,
    },
    /// The operation's path and estimated receipt payload exceeded admission bounds.
    #[error("curation operation payload is {actual} bytes; maximum is {max}")]
    OperationPayloadTooLarge {
        /// Maximum admitted estimated payload size.
        max: usize,
        /// Estimated payload size supplied by the caller.
        actual: usize,
    },
    /// The operation request identifier was already used in this source lifecycle.
    #[error(
        "curation operation request id {received} is not greater than the last admitted id {last}"
    )]
    NonMonotonicRequestId {
        /// Last request identifier admitted for this source lifecycle.
        last: u64,
        /// Request identifier supplied by the caller.
        received: u64,
    },
    /// A predecessor request identifier of zero is not a valid chain link.
    #[error("curation operation predecessor request id must be nonzero")]
    InvalidPredecessorRequestId,
    /// A rating carried a value outside the supported range.
    #[error("curation request contains invalid rating value {value}")]
    InvalidRating {
        /// Rejected raw rating value.
        value: i8,
    },
    /// A collection carried a slot outside the supported range.
    #[error("curation request contains invalid collection slot {value}")]
    InvalidCollection {
        /// Rejected raw collection value.
        value: u8,
    },
    /// A source-local tag row identifier was not positive.
    #[error("curation request contains invalid source tag id {tag_id}")]
    InvalidTagId {
        /// Rejected source-local tag row identifier.
        tag_id: i64,
    },
    /// A curation tag label was empty or contained only whitespace.
    #[error("curation request contains an empty tag label")]
    EmptyTagLabel,
    /// A curation tag label contained an embedded NUL byte.
    #[error("curation request tag label contains an embedded NUL byte")]
    TagLabelContainsNul,
    /// A curation tag label exceeded the bounded UTF-8 byte length.
    #[error("curation request tag label is {actual} UTF-8 bytes; maximum is {max}")]
    TagLabelTooLong {
        /// Maximum accepted UTF-8 byte length.
        max: usize,
        /// Supplied UTF-8 byte length.
        actual: usize,
    },
    /// The source has no current desired lifecycle admission.
    #[error("source is not admitted for curation: {0}")]
    NotRegistered(SourceId),
    /// The source was admitted but has been made inactive by a remove.
    #[error("source is inactive for curation: {source_id} generation {lifecycle_generation}")]
    Inactive {
        /// Source whose desired lifecycle is inactive.
        source_id: SourceId,
        /// Lifecycle generation supplied by the request.
        lifecycle_generation: u64,
    },
    /// The request lifecycle does not match the current desired lifecycle.
    #[error(
        "curation lifecycle generation mismatch for {source_id}: expected {expected}, received {received}"
    )]
    StaleLifecycleGeneration {
        /// Source whose lifecycle changed.
        source_id: SourceId,
        /// Current desired lifecycle generation.
        expected: u64,
        /// Lifecycle generation supplied by the request.
        received: u64,
    },
}

/// Return whether a catalog command remains durable during shutdown draining.
pub(crate) fn is_durable(command: &CatalogCommand) -> bool {
    matches!(
        command,
        CatalogCommand::ApplyCuration { .. }
            | CatalogCommand::ApplyCurationOperation { .. }
            | CatalogCommand::ApplyRatingDecay { .. }
    )
}

/// Validate curation input and its current desired lifecycle under the admission mutex.
pub(crate) fn validate_submission(
    command: &CatalogCommand,
    state: &AdmissionState,
) -> Result<(), CatalogCurationRequestError> {
    let (source_id, lifecycle_generation) = match command {
        CatalogCommand::ApplyCuration {
            source_id,
            lifecycle_generation,
            updates,
            ..
        } => {
            validate_updates(updates)?;
            (source_id, lifecycle_generation)
        }
        CatalogCommand::ApplyCurationOperation {
            source_id,
            lifecycle_generation,
            request_id,
            fence,
            root_identity,
            batches,
            ..
        } => {
            validate_operation(*request_id, fence, root_identity, batches, state, source_id)?;
            (source_id, lifecycle_generation)
        }
        CatalogCommand::ApplyRatingDecay {
            source_id,
            lifecycle_generation,
            ..
        } => (source_id, lifecycle_generation),
        _ => return Ok(()),
    };
    let Some(desired) = state.sources.get(source_id) else {
        return Err(CatalogCurationRequestError::NotRegistered(
            source_id.clone(),
        ));
    };
    if !desired.active {
        return Err(CatalogCurationRequestError::Inactive {
            source_id: source_id.clone(),
            lifecycle_generation: *lifecycle_generation,
        });
    }
    if desired.lifecycle_generation != *lifecycle_generation {
        return Err(CatalogCurationRequestError::StaleLifecycleGeneration {
            source_id: source_id.clone(),
            expected: desired.lifecycle_generation,
            received: *lifecycle_generation,
        });
    }
    Ok(())
}

fn validate_operation(
    request_id: u64,
    fence: &SourceCurationOperationFence,
    root_identity: &RootIdentity,
    batches: &[Vec<SourceCurationUpdate>],
    state: &AdmissionState,
    source_id: &SourceId,
) -> Result<(), CatalogCurationRequestError> {
    if request_id == 0 {
        return Err(CatalogCurationRequestError::NonMonotonicRequestId {
            last: 0,
            received: request_id,
        });
    }
    if let SourceCurationOperationFence::AfterOperation { request_id } = fence
        && *request_id == 0
    {
        return Err(CatalogCurationRequestError::InvalidPredecessorRequestId);
    }
    if let Some(desired) = state.sources.get(source_id)
        && let Some(last) = desired.last_curation_request_id
        && request_id <= last
    {
        return Err(CatalogCurationRequestError::NonMonotonicRequestId {
            last,
            received: request_id,
        });
    }
    if batches.is_empty() {
        return Err(CatalogCurationRequestError::Empty);
    }
    if batches.len() > MAX_CURATION_OPERATION_BATCHES {
        return Err(CatalogCurationRequestError::TooManyOperationBatches {
            max: MAX_CURATION_OPERATION_BATCHES,
            actual: batches.len(),
        });
    }
    let mut total_updates = 0usize;
    let mut path_bytes = 0usize;
    let mut label_bytes = 0usize;
    let mut existing_tag_id_bytes = 0usize;
    for batch in batches {
        if batch.is_empty() {
            return Err(CatalogCurationRequestError::Empty);
        }
        if batch.len() > MAX_CURATION_UPDATES {
            return Err(CatalogCurationRequestError::TooManyUpdates {
                max: MAX_CURATION_UPDATES,
                actual: batch.len(),
            });
        }
        total_updates = total_updates.saturating_add(batch.len());
        for update in batch {
            validate_update(update)?;
            path_bytes = path_bytes.saturating_add(path_byte_len(update_path(update)));
            label_bytes = label_bytes.saturating_add(update_label_byte_len(update));
            existing_tag_id_bytes =
                existing_tag_id_bytes.saturating_add(update_existing_tag_id_bytes(update));
        }
    }
    if total_updates > MAX_CURATION_OPERATION_UPDATES {
        return Err(CatalogCurationRequestError::TooManyOperationUpdates {
            max: MAX_CURATION_OPERATION_UPDATES,
            actual: total_updates,
        });
    }
    if path_bytes > MAX_CURATION_OPERATION_PATH_BYTES {
        return Err(CatalogCurationRequestError::OperationPayloadTooLarge {
            max: MAX_CURATION_OPERATION_PATH_BYTES,
            actual: path_bytes,
        });
    }
    let estimated_payload_bytes = operation_payload_bytes_with_path_bytes(
        root_identity.as_bytes().len(),
        batches.len(),
        total_updates,
        path_bytes,
        label_bytes,
        existing_tag_id_bytes,
    );
    if estimated_payload_bytes > MAX_CURATION_OPERATION_RECEIPT_BYTES {
        return Err(CatalogCurationRequestError::OperationPayloadTooLarge {
            max: MAX_CURATION_OPERATION_RECEIPT_BYTES,
            actual: estimated_payload_bytes,
        });
    }
    Ok(())
}

fn validate_updates(updates: &[SourceCurationUpdate]) -> Result<(), CatalogCurationRequestError> {
    if updates.is_empty() {
        return Err(CatalogCurationRequestError::Empty);
    }
    if updates.len() > MAX_CURATION_UPDATES {
        return Err(CatalogCurationRequestError::TooManyUpdates {
            max: MAX_CURATION_UPDATES,
            actual: updates.len(),
        });
    }
    for update in updates {
        validate_update(update)?;
    }
    Ok(())
}

fn validate_update(update: &SourceCurationUpdate) -> Result<(), CatalogCurationRequestError> {
    match update {
        SourceCurationUpdate::SetRating { rating, .. }
        | SourceCurationUpdate::SetRatingAndLastCuratedAt { rating, .. }
            if !(-3..=3).contains(&rating.val()) =>
        {
            Err(CatalogCurationRequestError::InvalidRating {
                value: rating.val(),
            })
        }
        SourceCurationUpdate::AddCollection { collection, .. }
        | SourceCurationUpdate::RemoveCollection { collection, .. }
            if usize::from(collection.index()) >= SampleCollection::COUNT =>
        {
            Err(CatalogCurationRequestError::InvalidCollection {
                value: collection.index(),
            })
        }
        SourceCurationUpdate::AssignExistingTag { tag_id, .. }
        | SourceCurationUpdate::RemoveExistingTag { tag_id, .. }
            if *tag_id <= 0 =>
        {
            Err(CatalogCurationRequestError::InvalidTagId { tag_id: *tag_id })
        }
        SourceCurationUpdate::AddTag { label, .. }
        | SourceCurationUpdate::RemoveTag { label, .. } => validate_tag_label(label),
        _ => Ok(()),
    }
}

fn validate_tag_label(label: &str) -> Result<(), CatalogCurationRequestError> {
    if label.len() > MAX_CURATION_TAG_LABEL_BYTES {
        return Err(CatalogCurationRequestError::TagLabelTooLong {
            max: MAX_CURATION_TAG_LABEL_BYTES,
            actual: label.len(),
        });
    }
    if label.contains('\0') {
        return Err(CatalogCurationRequestError::TagLabelContainsNul);
    }
    if label.split_whitespace().next().is_none() {
        return Err(CatalogCurationRequestError::EmptyTagLabel);
    }
    Ok(())
}

fn update_path(update: &SourceCurationUpdate) -> &Path {
    match update {
        SourceCurationUpdate::SetLastPlayedAt { path, .. }
        | SourceCurationUpdate::SetRating { path, .. }
        | SourceCurationUpdate::SetRatingAndLastCuratedAt { path, .. }
        | SourceCurationUpdate::SetLocked { path, .. }
        | SourceCurationUpdate::AddCollection { path, .. }
        | SourceCurationUpdate::RemoveCollection { path, .. }
        | SourceCurationUpdate::AddTag { path, .. }
        | SourceCurationUpdate::RemoveTag { path, .. }
        | SourceCurationUpdate::AssignExistingTag { path, .. }
        | SourceCurationUpdate::RemoveExistingTag { path, .. } => path.as_path(),
    }
}

fn update_label_byte_len(update: &SourceCurationUpdate) -> usize {
    match update {
        SourceCurationUpdate::AddTag { label, .. }
        | SourceCurationUpdate::RemoveTag { label, .. } => label.len(),
        _ => 0,
    }
}

fn update_existing_tag_id_bytes(update: &SourceCurationUpdate) -> usize {
    match update {
        SourceCurationUpdate::AssignExistingTag { .. }
        | SourceCurationUpdate::RemoveExistingTag { .. } => ESTIMATED_EXISTING_TAG_ID_BYTES,
        _ => 0,
    }
}

fn path_byte_len(path: &Path) -> usize {
    #[cfg(unix)]
    {
        path.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().len()
    }
}

fn operation_payload_bytes_with_path_bytes(
    root_identity_bytes: usize,
    batch_count: usize,
    update_count: usize,
    path_bytes: usize,
    label_bytes: usize,
    existing_tag_id_bytes: usize,
) -> usize {
    // Account for the caller-owned path spelling and the path retained in each receipt. Root
    // identity bytes are present in the operation and in every bounded receipt. Labels are
    // retained by the input updates but not by bounded receipts, so count each byte once.
    path_bytes
        .saturating_mul(2)
        .saturating_add(root_identity_bytes.saturating_mul(batch_count.saturating_add(1)))
        .saturating_add(update_count.saturating_mul(ESTIMATED_RECEIPT_ROW_BYTES))
        .saturating_add(label_bytes)
        .saturating_add(existing_tag_id_bytes)
        .saturating_add(batch_count.saturating_mul(ESTIMATED_OPERATION_BATCH_BYTES))
}

/// Estimate the bounded payload retained while an operation and its terminal outcome coexist.
pub(crate) fn operation_payload_bytes(
    root_identity: &RootIdentity,
    batches: &[Vec<SourceCurationUpdate>],
) -> usize {
    let mut update_count = 0usize;
    let mut path_bytes = 0usize;
    let mut label_bytes = 0usize;
    let mut existing_tag_id_bytes = 0usize;
    for batch in batches {
        update_count = update_count.saturating_add(batch.len());
        for update in batch {
            path_bytes = path_bytes.saturating_add(path_byte_len(update_path(update)));
            label_bytes = label_bytes.saturating_add(update_label_byte_len(update));
            existing_tag_id_bytes =
                existing_tag_id_bytes.saturating_add(update_existing_tag_id_bytes(update));
        }
    }
    operation_payload_bytes_with_path_bytes(
        root_identity.as_bytes().len(),
        batches.len(),
        update_count,
        path_bytes,
        label_bytes,
        existing_tag_id_bytes,
    )
}

/// Load the full browser curation projection through revision-fenced pages.
pub(crate) fn load_snapshot_curation_metadata(
    database: &SourceDatabase,
    source_id: &SourceId,
    revision: u64,
    manifest: &[SourceManifestEntry],
    mut should_cancel: impl FnMut() -> bool,
) -> Result<BTreeMap<PathBuf, BrowserFileMetadata>, CatalogError> {
    let mut metadata = BTreeMap::new();
    let mut cursor: Option<BrowserMetadataCursor> = None;
    loop {
        if should_cancel() {
            return Err(CatalogError::Canceled);
        }
        let page = database
            .browser_metadata_page(revision, cursor.as_ref(), 64)
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
        if page.revision != revision {
            return Err(CatalogError::StaleRevision {
                expected: revision,
                actual: page.revision,
            });
        }
        for file in page.files {
            // Browser pages also retain historical missing rows. They are outside the live
            // manifest and therefore do not belong in the curation projection.
            if file.missing {
                continue;
            }
            let Ok(index) =
                manifest.binary_search_by(|entry| entry.relative_path.cmp(&file.relative_path))
            else {
                return Err(CatalogError::DirectoryTruthUnavailable(format!(
                    "browser metadata path {} is absent from manifest revision {revision}",
                    file.relative_path.display()
                )));
            };
            let manifest_entry = &manifest[index];
            if manifest_entry.file_size != file.file_size
                || manifest_entry.modified_ns != file.modified_ns
            {
                return Err(CatalogError::DirectoryTruthUnavailable(format!(
                    "browser metadata facts for {} differ from manifest revision {revision}",
                    file.relative_path.display()
                )));
            }
            if metadata.insert(file.relative_path.clone(), file).is_some() {
                return Err(CatalogError::DirectoryTruthUnavailable(String::from(
                    "browser metadata page returned duplicate paths",
                )));
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if metadata.len() != manifest.len() {
        return Err(CatalogError::DirectoryTruthUnavailable(format!(
            "browser metadata projection contains {} live rows for {} manifest rows",
            metadata.len(),
            manifest.len()
        )));
    }
    Ok(metadata)
}

/// Execute one accepted durable curation command and publish exactly one terminal event.
pub(crate) fn execute(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    root_identity: RootIdentity,
    request_id: u64,
    updates: Vec<SourceCurationUpdate>,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let result = execute_inner(
        &source_id,
        lifecycle_generation,
        expected_revision,
        &root_identity,
        &updates,
        profile_guard,
        sources,
    );
    if let Some(source) = sources.get_mut(&source_id)
        && source.lifecycle_generation == lifecycle_generation
    {
        // An ordinary curation command is an independent writer. It cannot be used as an
        // implicit predecessor for a compound operation, even when it committed successfully.
        source.curation_lineage = None;
    }
    match result {
        Ok(receipt) => {
            let source = sources
                .get(&source_id)
                .filter(|source| source.lifecycle_generation == lifecycle_generation);
            let token = source.and_then(|source| source.attempt_token.as_ref());
            let _ = super::publish_source_receipt(
                &reservation.admission,
                reservation,
                &source_id,
                lifecycle_generation,
                token,
                source,
                |publication| CatalogEvent::CurationCommitted {
                    source_id: source_id.clone(),
                    lifecycle_generation,
                    request_id,
                    receipt,
                    publication,
                },
            );
        }
        Err(error) => reservation.publish(CatalogEvent::CurationFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        }),
    }
}

/// Execute an accepted compound curation command and publish one bounded terminal outcome.
pub(crate) fn execute_operation(
    source_id: SourceId,
    lifecycle_generation: u64,
    fence: SourceCurationOperationFence,
    root_identity: RootIdentity,
    request_id: u64,
    batches: Vec<Vec<SourceCurationUpdate>>,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let initial_revision = match fence {
        SourceCurationOperationFence::ExactRevision(revision) => Some(revision),
        SourceCurationOperationFence::AfterOperation { .. } => None,
    };
    let (initial_revision, mut expected_revision) =
        match resolve_operation_fence(&source_id, fence, sources) {
            Ok(resolved) => resolved,
            Err(error) => {
                let outcome = SourceCurationOperationOutcome {
                    initial_revision,
                    final_revision: initial_revision,
                    committed_batches: Vec::new(),
                    failure: Some(SourceCurationOperationFailure {
                        batch_index: 0,
                        error,
                    }),
                };
                publish_operation_outcome(
                    source_id,
                    lifecycle_generation,
                    request_id,
                    outcome,
                    reservation,
                    sources,
                );
                return;
            }
        };
    let mut committed_batches = Vec::new();
    let mut failure = None;
    for (batch_index, updates) in batches.iter().enumerate() {
        let result = if expected_revision == u64::MAX {
            Err(CatalogError::CurationRevisionOverflow(expected_revision))
        } else {
            execute_inner(
                &source_id,
                lifecycle_generation,
                expected_revision,
                &root_identity,
                updates,
                profile_guard,
                sources,
            )
        };
        match result {
            Ok(receipt) => {
                expected_revision = receipt.source_revision();
                committed_batches.push(receipt);
            }
            Err(error) => {
                failure = Some(SourceCurationOperationFailure { batch_index, error });
                break;
            }
        }
    }

    // The final revision is the committed prefix's revision, or the fenced revision when the
    // first batch failed.
    let final_revision = committed_batches
        .last()
        .map(SourceCurationCommitReceipt::source_revision)
        .or(initial_revision);
    let outcome = SourceCurationOperationOutcome {
        initial_revision,
        final_revision,
        committed_batches,
        failure,
    };
    publish_operation_outcome(
        source_id,
        lifecycle_generation,
        request_id,
        outcome,
        reservation,
        sources,
    );
}

fn resolve_operation_fence(
    source_id: &SourceId,
    fence: SourceCurationOperationFence,
    sources: &HashMap<SourceId, RegisteredSource>,
) -> Result<(Option<u64>, u64), CatalogError> {
    match fence {
        SourceCurationOperationFence::ExactRevision(revision) => Ok((Some(revision), revision)),
        SourceCurationOperationFence::AfterOperation { request_id } => {
            let Some(source) = sources.get(source_id) else {
                return Err(CatalogError::NotRegistered(source_id.clone()));
            };
            let Some(lineage) = source.curation_lineage else {
                return Err(CatalogError::CurationPredecessorUnavailable {
                    source_id: source_id.clone(),
                    request_id,
                });
            };
            let Some(revision) = lineage.last_success_revision else {
                return Err(CatalogError::CurationPredecessorUnavailable {
                    source_id: source_id.clone(),
                    request_id,
                });
            };
            if lineage.last_terminal_request_id != request_id {
                return Err(CatalogError::CurationPredecessorUnavailable {
                    source_id: source_id.clone(),
                    request_id,
                });
            }
            Ok((Some(revision), revision))
        }
    }
}

fn publish_operation_outcome(
    source_id: SourceId,
    lifecycle_generation: u64,
    request_id: u64,
    outcome: SourceCurationOperationOutcome,
    reservation: &mut EventReservation<'_>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let success_revision = outcome
        .is_success()
        .then(|| outcome.final_revision())
        .flatten();
    if let Some(source) = sources.get_mut(&source_id)
        && source.lifecycle_generation == lifecycle_generation
    {
        source.curation_lineage = Some(super::CurationLineage {
            last_terminal_request_id: request_id,
            last_success_revision: success_revision,
        });
    }
    let source = sources
        .get(&source_id)
        .filter(|source| source.lifecycle_generation == lifecycle_generation);
    let token = source.and_then(|source| source.attempt_token.as_ref());
    let _ = super::publish_source_receipt(
        &reservation.admission,
        reservation,
        &source_id,
        lifecycle_generation,
        token,
        source,
        |publication| CatalogEvent::CurationOperationCompleted {
            source_id: source_id.clone(),
            lifecycle_generation,
            request_id,
            outcome,
            publication,
        },
    );
}

pub(crate) fn execute_inner(
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    expected_root_identity: &RootIdentity,
    updates: &[SourceCurationUpdate],
    profile_guard: &WritableProfileGuard,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> Result<SourceCurationCommitReceipt, CatalogError> {
    let Some(source) = sources.get_mut(source_id) else {
        return Err(CatalogError::NotRegistered(source_id.clone()));
    };
    ensure_generation(source_id, source.lifecycle_generation, lifecycle_generation)?;

    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
        )
    })?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;

    let Some(snapshot) = source.snapshot.as_ref() else {
        return Err(CatalogError::SnapshotUnavailable);
    };
    if snapshot.source_id != *source_id {
        return Err(CatalogError::SnapshotUnavailable);
    }
    ensure_generation(
        source_id,
        snapshot.lifecycle_generation,
        lifecycle_generation,
    )?;
    if snapshot.committed_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: snapshot.committed_revision,
        });
    }
    if snapshot.root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("curation root identity differs from the retained snapshot"),
        });
    }
    if source
        .known_root_identity
        .as_ref()
        .is_some_and(|identity| identity != expected_root_identity)
    {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("curation root identity differs from the retained source"),
        });
    }

    let before_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if before_root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity differs from the curation request"),
        });
    }
    let before_revision = source
        .database
        .get_revision()
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if before_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: before_revision,
        });
    }

    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
        )
    })?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    let before_commit_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if before_commit_root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity changed before curation commit"),
        });
    }

    let owner_revision = source
        .database
        .writer
        .current_revision()
        .map_err(|error| curation_writer_error(source_id, error))?;
    if owner_revision.revision() != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: owner_revision.revision(),
        });
    }
    if owner_revision.root_identity() != expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source writer root identity differs from the curation request"),
        });
    }
    let receipt = source
        .database
        .writer
        .commit_catalog_curation_command(&owner_revision, profile_guard, updates)
        .map_err(|error| curation_writer_error(source_id, error))?;

    // The library receipt is durable authority. Any post-commit check failure only invalidates
    // the retained cache; it must never turn a successful database commit into CurationFailed.
    if !post_commit_binding_is_current(
        source_id,
        source,
        profile_guard,
        expected_root_identity,
        receipt.source_revision(),
    ) || !patch_cached_snapshot(source, expected_root_identity, expected_revision, &receipt)
    {
        source.snapshot = None;
    }
    Ok(receipt)
}

fn curation_writer_error(source_id: &SourceId, error: SourceWriterError) -> CatalogError {
    match error {
        SourceWriterError::Database(error) => {
            super::catalog_error_from_db_for_source(source_id, error)
        }
        SourceWriterError::StaleRevision { expected, actual } => {
            CatalogError::StaleRevision { expected, actual }
        }
        SourceWriterError::StaleFence | SourceWriterError::SupersededLifecycle { .. } => {
            CatalogError::SourceIdentityUncertain(error.to_string())
        }
        SourceWriterError::Uncertain { .. } | SourceWriterError::MalformedReceipt => {
            CatalogError::DatabaseIntegrityUncertain(error.to_string())
        }
        error => CatalogError::Database(error.to_string()),
    }
}

fn post_commit_binding_is_current(
    source_id: &SourceId,
    source: &RegisteredSource,
    profile_guard: &WritableProfileGuard,
    expected_root_identity: &RootIdentity,
    committed_revision: u64,
) -> bool {
    profile_guard.validate_current().is_ok()
        && source
            .database
            .validate_profile_guard(profile_guard)
            .is_ok()
        && checked_source_root_identity(source_id, &source.root)
            .is_ok_and(|identity| identity == *expected_root_identity)
        && source
            .database
            .get_revision()
            .is_ok_and(|revision| revision == committed_revision)
}

fn patch_cached_snapshot(
    source: &mut RegisteredSource,
    expected_root_identity: &RootIdentity,
    expected_revision: u64,
    receipt: &SourceCurationCommitReceipt,
) -> bool {
    let Some(snapshot) = source.snapshot.as_mut() else {
        return false;
    };
    let Some(next_revision) = expected_revision.checked_add(1) else {
        return false;
    };
    if receipt.source_revision() != next_revision
        || receipt.root_identity() != expected_root_identity
        || receipt.directory_generation() != snapshot.directory_generation
    {
        return false;
    }
    for file in receipt.files() {
        if file.missing {
            return false;
        }
        let Ok(index) = snapshot
            .manifest
            .binary_search_by(|entry| entry.relative_path.cmp(&file.relative_path))
        else {
            return false;
        };
        let manifest_entry = &snapshot.manifest[index];
        if manifest_entry.file_size != file.file_size
            || manifest_entry.modified_ns != file.modified_ns
        {
            return false;
        }
    }
    snapshot.committed_revision = receipt.source_revision();
    for file in receipt.files() {
        snapshot
            .curation_metadata
            .insert(file.relative_path.clone(), file.clone());
    }
    true
}

pub(crate) fn ensure_generation(
    source_id: &SourceId,
    expected: u64,
    received: u64,
) -> Result<(), CatalogError> {
    if expected == received {
        Ok(())
    } else {
        Err(CatalogError::StaleLifecycleGeneration {
            source_id: source_id.clone(),
            expected,
            received,
        })
    }
}

pub(crate) fn checked_source_root_identity(
    source_id: &SourceId,
    root: &std::path::Path,
) -> Result<RootIdentity, CatalogError> {
    source_root_identity(root).map_err(|error| match error {
        CatalogError::StaleRootGeneration { detail, .. } => CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail,
        },
        error => error,
    })
}
