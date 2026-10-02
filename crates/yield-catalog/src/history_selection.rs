//! Read-only collection history observations on the registered catalog owner.
use super::*;
use yield_library::sample_sources::SampleCollection;
pub use yield_library::sample_sources::db::SourceCollectionHistorySelection;

pub(super) const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// Immutable bounded explicit request, without mutation or history admission authority.
#[derive(Debug)]
pub struct CollectionHistoryRequest {
    source_id: SourceId,
    lifecycle: u64,
    revision: u64,
    generation: u64,
    collection: SampleCollection,
    desired: bool,
    paths: Vec<RootRelativePath>,
}
impl CollectionHistoryRequest {
    /// Validate bounds and exact keys before cloning any selection pathname.
    pub fn new(
        source_id: SourceId,
        lifecycle: u64,
        revision: u64,
        generation: u64,
        collection: SampleCollection,
        desired: bool,
        paths: &[RootRelativePath],
    ) -> Result<Self, CollectionHistoryFailure> {
        if source_id.as_str().is_empty()
            || source_id.as_str().len() > 256
            || lifecycle == 0
            || generation == 0
            || paths.is_empty()
            || paths.len() > 128
            || usize::from(collection.index()) >= SampleCollection::COUNT
        {
            return Err(CollectionHistoryFailure::InvalidInput);
        }
        // Covers request plus owner result, four bounded identity strings, and row/key
        // overhead. The single output reservation retains both through event consumption.
        let mut bytes = 32 * 1024;
        for (index, path) in paths.iter().enumerate() {
            let len = path.as_path().as_os_str().len();
            bytes += 2 * len + 256;
            if len > 4096
                || bytes > MAX_RESULT_BYTES
                || paths[..index].contains(path)
                || !yield_library::sample_sources::is_supported_audio(path.as_path())
            {
                return Err(CollectionHistoryFailure::InvalidInput);
            }
        }
        for path in paths {
            let key = yield_library::sample_sources::normalize_relative_path(path.as_path())
                .map_err(|_| CollectionHistoryFailure::InvalidInput)?;
            let spelling = path
                .as_path()
                .to_str()
                .ok_or(CollectionHistoryFailure::InvalidInput)?;
            #[cfg(windows)]
            let spelling = spelling.replace('\\', "/");
            if spelling != key {
                return Err(CollectionHistoryFailure::InvalidInput);
            }
        }
        Ok(Self {
            // Retain only bounded identity storage, not the caller's capacity.
            source_id: SourceId::from_string(source_id.as_str()),
            lifecycle,
            revision,
            generation,
            collection,
            desired,
            paths: paths.to_vec(),
        })
    }
    /// Selected registered source.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }
    /// Expected registered lifecycle.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle
    }
    /// Exact manifest revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Consumer request generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Target collection.
    pub fn collection(&self) -> SampleCollection {
        self.collection
    }
    /// Explicit desired membership.
    pub fn desired(&self) -> bool {
        self.desired
    }
    /// Exact explicit selection.
    pub fn paths(&self) -> &[RootRelativePath] {
        &self.paths
    }
    pub(super) fn input_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.source_id.as_str().len()
            + self
                .paths
                .iter()
                .map(|p| p.as_path().as_os_str().len() + std::mem::size_of::<RootRelativePath>())
                .sum::<usize>()
    }
}
/// Cancellation retained through computation and delivery, also used for source retirement.
#[derive(Clone, Debug, Default)]
pub struct CollectionHistoryCancellation(Arc<AtomicBool>);
impl CollectionHistoryCancellation {
    /// New uncancelled request token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Cancel computation or queued delivery.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Cheap atomic-only check.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
/// Diagnostic only; no read, mutation, capacity or physical ownership authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionHistoryFailure {
    /// Malformed, duplicate or over-budget input.
    InvalidInput,
    /// Source lifecycle, revision or custody changed.
    StaleSource,
    /// Retained owner could not confirm the selection.
    OwnerUnavailable,
    /// Owner read exceeded its deadline or allocation bound.
    BudgetExceeded,
    /// Successful observation expired while queued for its still-live caller.
    ObservationExpired,
}

pub(super) fn dispatch(
    request: CollectionHistoryRequest,
    cancellation: CollectionHistoryCancellation,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    if cancellation.is_cancelled() {
        return;
    }
    let mut state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutdown || state.closed {
        return;
    }
    let Some(live) = state
        .sources
        .get_mut(&request.source_id)
        .filter(|live| live.active && live.lifecycle_generation == request.lifecycle)
    else {
        return;
    };
    if let Some(previous) = live.attempt_token.replace(Arc::clone(&cancellation.0)) {
        previous.store(true, Ordering::Release);
    }
    drop(state);
    let Some(source) = sources.get_mut(&request.source_id) else {
        return;
    };
    source.attempt_token = Some(Arc::clone(&cancellation.0));
    source.collection_history_token = Some(Arc::downgrade(&cancellation.0));
    let result = (|| {
        let revision = source
            .database
            .writer
            .current_revision()
            .map_err(|_| CollectionHistoryFailure::StaleSource)?;
        if revision.source_id() != &request.source_id
            || revision.lifecycle_generation() != request.lifecycle
            || revision.revision() != request.revision
        {
            return Err(CollectionHistoryFailure::StaleSource);
        }
        source
            .database
            .writer
            .capture_collection_history_selection(
                &revision,
                request.collection,
                request.desired,
                &request.paths,
            )
            .map_err(|error| match error {
                SourceWriterError::StaleFence | SourceWriterError::StaleRevision { .. } => {
                    CollectionHistoryFailure::StaleSource
                }
                SourceWriterError::SnapshotBudgetExceeded => {
                    CollectionHistoryFailure::BudgetExceeded
                }
                SourceWriterError::InvalidRequest(_) => CollectionHistoryFailure::InvalidInput,
                _ => CollectionHistoryFailure::OwnerUnavailable,
            })
    })();
    if cancellation.is_cancelled() {
        return;
    }
    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if !publication_is_current_state(
        &state,
        &request.source_id,
        request.lifecycle,
        Some(&cancellation.0),
    ) {
        return;
    }
    drop(state);
    // Validate physical custody on this owner thread, never at event consumption.
    let result = if result.is_ok() && source.publication_fence.validate().is_err() {
        Err(CollectionHistoryFailure::StaleSource)
    } else {
        result
    };
    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
    if !publication_is_current_state(
        &state,
        &request.source_id,
        request.lifecycle,
        Some(&cancellation.0),
    ) {
        return;
    }
    let fence = result.is_ok().then(|| source.publication_fence.clone());
    reservation.publish_with_fence(
        CatalogEvent::CollectionHistoryCompleted {
            request,
            cancellation,
            result,
        },
        fence,
    );
}

// A later sole-owner mutation must retire queued before-state without SQL at delivery.
// Advance existing attempt custody only when this exact capture still owns it; ordinary
// commands without a current history observation keep their existing behavior.
pub(super) fn invalidate_before_write(
    command: &CatalogCommand,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let (id, lifecycle) = match command {
        CatalogCommand::ApplyCuration {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogCommand::ApplyCurationOperation {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogCommand::ApplyRatingDecay {
            source_id,
            lifecycle_generation,
            ..
        } => (source_id, *lifecycle_generation),
        _ => return,
    };
    let Some(source) = sources
        .get_mut(id)
        .filter(|s| s.lifecycle_generation == lifecycle)
    else {
        return;
    };
    let Some(token) = source
        .collection_history_token
        .take()
        .and_then(|t| t.upgrade())
    else {
        return;
    };
    if token.load(Ordering::Acquire) {
        return;
    }
    if source
        .attempt_token
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &token))
    {
        source.attempt_token = begin_source_attempt(admission, id, lifecycle);
    } else {
        token.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod input_storage_tests {
    use super::*;

    #[test]
    fn history_request_does_not_retain_oversized_source_identity_storage() {
        let mut identity = String::with_capacity(MAX_RESULT_BYTES * 2);
        identity.push_str("bounded");
        assert!(identity.capacity() > MAX_RESULT_BYTES);
        let original = identity.as_ptr();
        let request = CollectionHistoryRequest::new(
            SourceId::from_string(identity),
            1,
            1,
            1,
            SampleCollection::new(0).unwrap(),
            true,
            &[RootRelativePath::try_from_path(PathBuf::from("a.wav")).unwrap()],
        )
        .unwrap();
        // The caller allocation is still alive when normalization allocates the
        // accepted identity, so address inequality proves it was not retained.
        assert_ne!(request.source_id().as_str().as_ptr(), original);
        assert_eq!(request.source_id().as_str(), "bounded");
        assert!(request.input_bytes() < 32 * 1024);
    }
}
