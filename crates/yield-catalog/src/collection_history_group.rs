//! Complete-owner compound preparation and retained worker handoff.
use super::*;
use yield_library::sample_sources::db::{
    SOURCE_COLLECTION_HISTORY_GROUP_MAX_BYTES, SOURCE_COLLECTION_HISTORY_GROUP_MAX_PATHS,
    SourceCollectionHistoryGroup,
};

/// One frozen bounded compound selection, without SQL or history admission authority.
#[derive(Debug)]
pub struct CollectionHistoryGroupRequest {
    source_id: SourceId,
    lifecycle: u64,
    revision: u64,
    generation: u64,
    collection: SampleCollection,
    targets: Vec<(RootRelativePath, bool)>,
    deadline: Instant,
    charged_bytes: usize,
}
impl CollectionHistoryGroupRequest {
    /// Check complete group bounds and canonical unique keys before retaining caller input.
    pub fn new(
        source_id: SourceId,
        lifecycle: u64,
        revision: u64,
        generation: u64,
        collection: SampleCollection,
        desired: bool,
        paths: &[RootRelativePath],
    ) -> Result<Self, CollectionHistoryFailure> {
        let deadline = Instant::now() + Duration::from_secs(2);
        Self::charge(paths.iter().map(|p| p.as_path()), paths.len())?;
        if source_id.as_str().is_empty()
            || source_id.as_str().len() > 256
            || lifecycle == 0
            || generation == 0
        {
            return Err(CollectionHistoryFailure::InvalidInput);
        }
        let targets: Vec<_> = paths.iter().map(|p| (p.clone(), desired)).collect();
        Self::build(
            source_id, lifecycle, revision, generation, collection, &targets, deadline,
        )
    }
    /// Freeze exact per-path Add/Remove targets at one collection slot and original deadline.
    pub fn new_mixed(
        source_id: SourceId,
        lifecycle: u64,
        revision: u64,
        generation: u64,
        collection: SampleCollection,
        targets: &[(RootRelativePath, bool)],
    ) -> Result<Self, CollectionHistoryFailure> {
        Self::build(
            source_id,
            lifecycle,
            revision,
            generation,
            collection,
            targets,
            Instant::now() + Duration::from_secs(2),
        )
    }
    fn charge<'a>(
        paths: impl Iterator<Item = &'a Path>,
        count: usize,
    ) -> Result<usize, CollectionHistoryFailure> {
        if count == 0 || count > SOURCE_COLLECTION_HISTORY_GROUP_MAX_PATHS {
            return Err(CollectionHistoryFailure::InvalidInput);
        }
        let mut bytes = 64usize * 1024;
        for path in paths {
            let len = path.as_os_str().len();
            bytes = bytes
                .checked_add(
                    len.checked_mul(8)
                        .and_then(|n| n.checked_add(1024))
                        .ok_or(CollectionHistoryFailure::BudgetExceeded)?,
                )
                .ok_or(CollectionHistoryFailure::BudgetExceeded)?;
            if len > 4096 || bytes > SOURCE_COLLECTION_HISTORY_GROUP_MAX_BYTES {
                return Err(CollectionHistoryFailure::BudgetExceeded);
            }
        }
        Ok(bytes)
    }
    fn build(
        source_id: SourceId,
        lifecycle: u64,
        revision: u64,
        generation: u64,
        collection: SampleCollection,
        targets: &[(RootRelativePath, bool)],
        deadline: Instant,
    ) -> Result<Self, CollectionHistoryFailure> {
        if source_id.as_str().is_empty()
            || source_id.as_str().len() > 256
            || lifecycle == 0
            || generation == 0
        {
            return Err(CollectionHistoryFailure::InvalidInput);
        }
        let charged_bytes = Self::charge(targets.iter().map(|(p, _)| p.as_path()), targets.len())?;
        let mut keys = std::collections::HashSet::with_capacity(targets.len());
        for chunk in targets.chunks(128) {
            for desired in [false, true] {
                let paths: Vec<_> = chunk
                    .iter()
                    .filter(|(_, d)| *d == desired)
                    .map(|(p, _)| p.clone())
                    .collect();
                if !paths.is_empty() {
                    CollectionHistoryRequest::new(
                        SourceId::from_string(source_id.as_str()),
                        lifecycle,
                        revision,
                        generation,
                        collection,
                        desired,
                        &paths,
                    )?;
                }
            }
            for (path, _) in chunk {
                if !keys.insert(path.as_path()) {
                    return Err(CollectionHistoryFailure::InvalidInput);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(CollectionHistoryFailure::BudgetExceeded);
        }
        Ok(Self {
            source_id: SourceId::from_string(source_id.as_str()),
            lifecycle,
            revision,
            generation,
            collection,
            targets: targets.to_vec(),
            deadline,
            charged_bytes,
        })
    }
    /// Exact registered source context.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }
    /// Exact registered lifecycle.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle
    }
    /// Expected numeric revision before preparation.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Caller context generation for late delivery rejection.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Selected collection slot.
    pub fn collection(&self) -> SampleCollection {
        self.collection
    }
    /// Exact original per-path explicit targets, including observed no-ops.
    pub fn targets(&self) -> &[(RootRelativePath, bool)] {
        &self.targets
    }
    /// Conservative complete preparation/history charge.
    pub fn charged_bytes(&self) -> usize {
        self.charged_bytes
    }
    pub(crate) fn input_bytes(&self) -> usize {
        self.source_id.as_str().len()
            + std::mem::size_of::<Self>()
            + self
                .targets
                .iter()
                .map(|(p, _)| {
                    std::mem::size_of::<(RootRelativePath, bool)>() + p.as_path().as_os_str().len()
                })
                .sum::<usize>()
    }
}

/// Opaque actual catalog delivery custody for one complete owner observation.
#[derive(Debug)]
pub struct CatalogCollectionHistoryGroup {
    source: Arc<SourceCollectionHistoryGroup>,
    capture: CollectionHistoryCancellation,
    generation: u64,
}
impl PartialEq for CatalogCollectionHistoryGroup {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for CatalogCollectionHistoryGroup {}
impl CatalogCollectionHistoryGroup {
    /// Original genuine source observation; never SQL or publication permission.
    pub fn source_observation(&self) -> &Arc<SourceCollectionHistoryGroup> {
        &self.source
    }
    /// Exact preparation caller generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// In-memory timing and cancellation only; owner handoff revalidates actual custody.
    pub fn within_observation_window(&self) -> bool {
        !self.capture.is_cancelled() && self.source.within_observation_window()
    }
}

/// Passive genuine prepared input for one exact compound command, never a live permit.
#[derive(Debug)]
pub struct PreparedCollectionOperation {
    group: Arc<CatalogCollectionHistoryGroup>,
    request: u64,
}
impl PreparedCollectionOperation {
    /// Bind genuine observation and retained capture custody to a new nonzero request.
    pub fn new(
        group: Arc<CatalogCollectionHistoryGroup>,
        request: u64,
    ) -> Result<Self, CollectionHistoryFailure> {
        if request == 0 || group.source.changes().is_empty() {
            return Err(CollectionHistoryFailure::InvalidInput);
        }
        if !group.within_observation_window() {
            return Err(CollectionHistoryFailure::ObservationExpired);
        }
        Ok(Self { group, request })
    }
    /// Original genuine immutable preparation, without restored owner authority.
    pub fn group(&self) -> &Arc<CatalogCollectionHistoryGroup> {
        &self.group
    }
    /// Exact command request correlation.
    pub fn request_id(&self) -> u64 {
        self.request
    }
}
pub(crate) fn dispatch_group(
    request: CollectionHistoryGroupRequest,
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
    let preserve_harvest = state
        .sources
        .get(&request.source_id)
        .and_then(|s| s.attempt_token.as_ref())
        .is_some_and(|token| {
            state
                .retained_harvest
                .owns(&request.source_id, request.lifecycle, token)
        });
    let Some(live) = state
        .sources
        .get_mut(&request.source_id)
        .filter(|live| live.active && live.lifecycle_generation == request.lifecycle)
    else {
        return;
    };
    if let Some(previous) = live.attempt_token.replace(Arc::clone(&cancellation.0))
        && !Arc::ptr_eq(&previous, &cancellation.0)
        && !preserve_harvest
    {
        previous.store(true, Ordering::Release);
    }
    drop(state);
    let Some(source) = sources.get_mut(&request.source_id) else {
        return;
    };
    source.attempt_token = Some(Arc::clone(&cancellation.0));
    source.collection_history_token = Some(Arc::downgrade(&cancellation.0));
    let result = (|| {
        if Instant::now() >= request.deadline {
            return Err(CollectionHistoryFailure::BudgetExceeded);
        }
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
            .capture_collection_history_mixed_group(
                &revision,
                request.collection,
                &request.targets,
                &cancellation.0,
                request.deadline,
            )
            .map(|source| {
                Arc::new(CatalogCollectionHistoryGroup {
                    source: Arc::new(source),
                    capture: cancellation.clone(),
                    generation: request.generation,
                })
            })
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
        CatalogEvent::CollectionHistoryGroupCompleted {
            request,
            cancellation,
            result,
        },
        fence,
    );
}

pub(crate) fn dispatch_prepared(
    operation: PreparedCollectionOperation,
    profile: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let prepared = operation.group;
    let source_group = &prepared.source;
    let id = source_group.revision().source_id().clone();
    let lifecycle = source_group.revision().lifecycle_generation();
    let revision = source_group.revision().revision();
    let root = source_group.revision().root_identity().clone();
    let execution = (|| {
        if !prepared.within_observation_window() {
            return Err(CatalogError::SourceIdentityUncertain(
                "compound preparation expired or cancelled".into(),
            ));
        }
        let source = sources
            .get(&id)
            .ok_or_else(|| CatalogError::NotRegistered(id.clone()))?;
        let state = admission.lock().unwrap_or_else(|p| p.into_inner());
        if !publication_is_current_state(&state, &id, lifecycle, Some(&prepared.capture.0)) {
            return Err(CatalogError::SourceIdentityUncertain(
                "compound preparation custody changed".into(),
            ));
        }
        drop(state);
        source
            .database
            .writer
            .begin_collection_history_execution(Arc::clone(source_group))
            .map_err(|error| CatalogError::SourceIdentityUncertain(error.to_string()))
    })();
    let batches: Vec<Vec<SourceCurationUpdate>> = source_group
        .batches()
        .map(|chunk| {
            chunk
                .iter()
                .map(|c| {
                    if c.after() {
                        SourceCurationUpdate::AddCollection {
                            path: c.path().clone(),
                            collection: source_group.collection(),
                        }
                    } else {
                        SourceCurationUpdate::RemoveCollection {
                            path: c.path().clone(),
                            collection: source_group.collection(),
                        }
                    }
                })
                .collect()
        })
        .collect();
    // Initial validation precedes retirement of this exact observation. The private progression
    // retains original issuer provenance; ordinary mutation gets fresh publication custody.
    if execution.is_ok() {
        super::invalidate_for_source(&id, lifecycle, admission, sources);
    }
    curation::execute_prepared_operation(
        id,
        lifecycle,
        revision,
        root,
        operation.request,
        batches,
        prepared,
        execution,
        profile,
        reservation,
        sources,
    );
}
