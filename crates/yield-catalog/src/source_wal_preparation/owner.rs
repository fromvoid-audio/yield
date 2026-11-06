//! Worker-local adapter. Genuine native custody and outcomes belong to the library.
use super::*;
use crate::PlaymarkTempoOperation;
struct Slot {
    source: RegisteredSource<SourceWalProducerOwner>,
    lifecycle_generation: u64,
    deadline: Instant,
    rejection: Option<CatalogSourceWalRejection>,
}
pub(crate) struct WorkerReservations {
    inbox: SourceWalTransportInbox,
    slots: HashMap<SourceId, Slot>,
    unresolved: HashMap<SourceId, SourceWalOwnerObservation>,
}
impl WorkerReservations {
    pub(crate) fn new(inbox: SourceWalTransportInbox) -> Self {
        Self {
            inbox,
            slots: HashMap::new(),
            unresolved: HashMap::new(),
        }
    }
    pub(crate) fn poll(
        &mut self,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &mut HashMap<SourceId, RegisteredSource>,
    ) {
        self.sweep(admission, sources);
        for _ in 0..CATALOG_WAL_MAX_SLOTS {
            let Some(command) = self.inbox.try_next_any() else {
                break;
            };
            self.prepare(command, admission, sources);
        }
    }
    fn prepare(
        &mut self,
        command: SourceWalProducerCommand,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &mut HashMap<SourceId, RegisteredSource>,
    ) {
        if command.is_cancelled() {
            command.reject(CatalogSourceWalRejection::Cancelled);
            return;
        }
        if Instant::now() >= command.deadline() {
            command.reject(CatalogSourceWalRejection::Deadline);
            return;
        }
        let id = SourceId::from_string(command.source_id().to_owned());
        let lifecycle_generation = command.lifecycle_generation();
        if !current(admission, &id, lifecycle_generation) {
            command.reject(CatalogSourceWalRejection::Scope);
            return;
        }
        if self.is_held(&id) {
            command.reject(CatalogSourceWalRejection::Busy);
            return;
        }
        if !self.unresolved.is_empty() {
            command.reject(CatalogSourceWalRejection::OwnerLost);
            return;
        }
        let Some(source) = sources.remove(&id) else {
            command.reject(CatalogSourceWalRejection::Scope);
            return;
        };
        if source.lifecycle_generation != lifecycle_generation {
            sources.insert(id, source);
            command.reject(CatalogSourceWalRejection::Scope);
            return;
        }
        let deadline = command.deadline();
        let mapped: Result<
            RegisteredSource<SourceWalProducerOwner>,
            RegisteredSource<std::convert::Infallible>,
        > = source.try_map_database(|database| Ok(command.acquire(database.writer)));
        let mut parked = match mapped {
            Ok(source) => source,
            Err(never) => match never.database {},
        };
        let mut rejection = parked.database.rejection();
        if parked.database.is_prepared()
            && self.slots.values().any(|slot| {
                slot.source
                    .database
                    .database_identity()
                    .is_none_or(|identity| Some(identity) == parked.database.database_identity())
            })
        {
            rejection = Some(CatalogSourceWalRejection::Busy);
        }
        if rejection.is_none()
            && (parked.database.is_cancelled() || !current(admission, &id, lifecycle_generation))
        {
            rejection = Some(CatalogSourceWalRejection::Cancelled);
        }
        if rejection.is_none() {
            rejection = parked
                .database
                .publish_prepared(parked.source_authority.as_ref())
                .err();
        }
        if rejection.is_some() {
            parked.database.request_abort(rejection);
        }
        self.slots.insert(
            id,
            Slot {
                source: parked,
                lifecycle_generation,
                deadline,
                rejection,
            },
        );
        self.sweep(admission, sources);
    }
    fn sweep(
        &mut self,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &mut HashMap<SourceId, RegisteredSource>,
    ) {
        let abort: Vec<_> = self
            .slots
            .iter_mut()
            .filter_map(|(id, slot)| {
                // The genuine journal decision fences deadline, cancellation and shutdown.
                // Continue polling; never wait for the journal with this original Native owner.
                if slot.source.database.settlement_held() {
                    return None;
                }
                // Validation can race service shutdown. Observe admission again afterwards;
                // invalidation caused by closing is cancellation, not a changed source scope.
                // Never hold admission across the original owner's SQLite validation.
                let validation = slot
                    .source
                    .database
                    .revalidate(slot.source.source_authority.as_ref())
                    .err();
                let (closing, changed) = {
                    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        state.shutdown || state.closed,
                        !state.sources.get(id).is_some_and(|source| {
                            source.active
                                && source.lifecycle_generation == slot.lifecycle_generation
                        }),
                    )
                };
                let cancelled = closing || slot.source.database.is_cancelled();
                if slot.rejection.is_some()
                    || cancelled
                    || changed
                    || Instant::now() >= slot.deadline
                    || validation.is_some()
                {
                    if slot.rejection.is_none() && !cancelled {
                        slot.rejection = Some(if changed {
                            CatalogSourceWalRejection::Scope
                        } else {
                            validation.unwrap_or(CatalogSourceWalRejection::Deadline)
                        });
                    }
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        for id in abort {
            self.abort_one(&id, sources);
        }
    }
    fn abort_one(&mut self, id: &SourceId, sources: &mut HashMap<SourceId, RegisteredSource>) {
        let Some(mut slot) = self.slots.remove(id) else {
            return;
        };
        slot.source.database.request_abort(slot.rejection);
        match slot.source.try_map_database(|owner| {
            owner
                .into_writer()
                .map(|writer| CatalogSourceDatabase { writer })
        }) {
            Ok(source) => {
                assert!(
                    !sources.contains_key(id),
                    "parked source cannot acquire a second SQL owner"
                );
                sources.insert(id.clone(), source);
            }
            Err(source) => {
                slot.source = source;
                self.slots.insert(id.clone(), slot);
            }
        }
    }
    pub(crate) fn abort_all(&mut self, sources: &mut HashMap<SourceId, RegisteredSource>) {
        let ids: Vec<_> = self.slots.keys().cloned().collect();
        for id in ids {
            self.abort_one(&id, sources);
        }
    }
    pub(crate) fn retire_failed(
        &mut self,
        journal: Option<&OperationJournalClient>,
        pending: &mut HashMap<SourceId, PendingSourceFinalization>,
    ) {
        let Some(journal) = journal else {
            return;
        };
        let ids: Vec<_> = self.slots.keys().cloned().collect();
        for id in ids {
            let mut slot = self.slots.remove(&id).expect("retained failed slot");
            let _ = poll_pending_source_authority(&mut slot.source);
            if slot.source.source_authority_receipt.is_none()
                || slot.source.source_authority.is_none()
                || slot.source.database.is_prepared()
            {
                self.slots.insert(id, slot);
                continue;
            }
            let outcome = slot.source.database.outcome_observation();
            match retire_source(id.clone(), slot.source, journal, pending) {
                RetireSourceOutcome::Removed | RetireSourceOutcome::FinalizationPending { .. } => {}
                RetireSourceOutcome::Retained { source, reason } => {
                    slot.source = source;
                    report_source_authority_issue_for_source(
                        &id,
                        "failed preparation retains original drain evidence",
                        reason,
                    );
                    self.slots.insert(id, slot);
                }
                RetireSourceOutcome::Unresolved { reason } => {
                    report_source_authority_issue_for_source(
                        &id,
                        "failed preparation closed without a retirement acknowledgement",
                        reason,
                    );
                    self.unresolved.insert(id, outcome);
                }
            }
        }
    }
    pub(crate) fn blocked_lifecycle(
        &self,
        command: &CatalogCommand,
    ) -> Option<(SourceId, u64, CatalogOperation)> {
        match command {
            CatalogCommand::Register {
                source_id,
                lifecycle_generation,
                ..
            }
            | CatalogCommand::RegisterVerified {
                source_id,
                lifecycle_generation,
                ..
            } if self.is_held(source_id) => Some((
                source_id.clone(),
                *lifecycle_generation,
                CatalogOperation::Register,
            )),
            CatalogCommand::Remove {
                source_id,
                lifecycle_generation,
            } if self.is_held(source_id) => Some((
                source_id.clone(),
                *lifecycle_generation,
                CatalogOperation::Remove,
            )),
            _ => None,
        }
    }
    pub(crate) fn is_held(&self, source: &SourceId) -> bool {
        self.slots.contains_key(source) || self.unresolved.contains_key(source)
    }
    pub(crate) fn has_settling(&self) -> bool {
        self.slots
            .values()
            .any(|slot| slot.source.database.settlement_held())
    }
    pub(crate) fn has_slots(&self) -> bool {
        !self.slots.is_empty() || !self.unresolved.is_empty()
    }
    pub(crate) fn close(&mut self, sources: &mut HashMap<SourceId, RegisteredSource>) {
        self.inbox.close();
        self.abort_all(sources);
    }
}
impl Drop for WorkerReservations {
    fn drop(&mut self) {
        self.inbox.close();
    }
}
fn current(
    admission: &Arc<Mutex<AdmissionState>>,
    id: &SourceId,
    lifecycle_generation: u64,
) -> bool {
    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
    !state.shutdown
        && !state.closed
        && state
            .sources
            .get(id)
            .is_some_and(|s| s.active && s.lifecycle_generation == lifecycle_generation)
}
pub(crate) fn lifecycle(command: &CatalogCommand) -> bool {
    matches!(
        command,
        CatalogCommand::Register { .. }
            | CatalogCommand::RegisterVerified { .. }
            | CatalogCommand::Remove { .. }
            | CatalogCommand::Shutdown
    )
}
pub(crate) fn deferred(command: &CatalogCommand, slots: &WorkerReservations) -> bool {
    match command {
        CatalogCommand::Register { .. }
        | CatalogCommand::RegisterVerified { .. }
        | CatalogCommand::Remove { .. }
        | CatalogCommand::Shutdown => false,
        CatalogCommand::RequestAudit { source_id, .. }
        | CatalogCommand::RequestFeatureAnalysis { source_id, .. }
        | CatalogCommand::RequestWavMetadata { source_id, .. }
        | CatalogCommand::RequestIndexPage { source_id, .. }
        | CatalogCommand::RequestTagPage { source_id, .. }
        | CatalogCommand::RequestTagAssignments { source_id, .. }
        | CatalogCommand::RequestTagFilter { source_id, .. }
        | CatalogCommand::RequestDisplayLabels { source_id, .. }
        | CatalogCommand::RequestCurationTagFacts { source_id, .. }
        | CatalogCommand::ApplyCuration { source_id, .. }
        | CatalogCommand::ApplyCurationOperation { source_id, .. }
        | CatalogCommand::ApplyRatingDecay { source_id, .. }
        | CatalogCommand::ReadSnapshot { source_id, .. } => slots.is_held(source_id),
        CatalogCommand::RequestTagAssignmentPreview { .. } => slots.has_slots(),
        CatalogCommand::RequestHarvestFacts { request, .. } => slots.is_held(request.source_id()),
        CatalogCommand::RequestHarvestFilter { request, .. } => {
            slots.is_held(request.context().source_id())
        }
        CatalogCommand::RequestCollectionHistory { request, .. } => {
            slots.is_held(request.source_id())
        }
        CatalogCommand::RequestCollectionHistoryGroup { request, .. } => {
            slots.is_held(request.source_id())
        }
        CatalogCommand::ApplyPreparedCollectionOperation { operation } => slots.is_held(
            operation
                .group()
                .source_observation()
                .revision()
                .source_id(),
        ),
        CatalogCommand::RequestSimilarity { request, .. } => slots.is_held(request.source_id()),
        CatalogCommand::RequestTransientMarkers { request, .. } => {
            slots.is_held(request.source_id())
        }
        CatalogCommand::RequestSourceContentDigest { request, .. } => {
            slots.is_held(&request.scope().source_id)
        }
        CatalogCommand::PlaymarkTempo { request, .. } => slots.is_held(request.source_id()),
        CatalogCommand::PlaymarkGridTempo { request, .. } => slots.is_held(request.source_id()),
        CatalogCommand::ConsumeReplay { submission } => slots.is_held(submission.source_id()),
        CatalogCommand::RequestIndexInspection { handle, .. } => slots.is_held(&handle.source_id),
    }
}

// Broad scans, DSP, replay/writes and waits on another owner cannot keep any successful
// pre-intent writer locked while this single worker is executing that normal command.
// Bounded read pages on an unrelated source remain usable without touching parked owners.
pub(crate) fn interrupts_preparation(command: &CatalogCommand) -> bool {
    match command {
        CatalogCommand::PlaymarkTempo { request, .. } => {
            matches!(request.operation(), PlaymarkTempoOperation::Replace { .. })
        }
        CatalogCommand::PlaymarkGridTempo { request, .. } => {
            matches!(
                request.operation(),
                PlaymarkGridTempoOperation::Replace { .. }
            )
        }
        CatalogCommand::RequestAudit { .. }
        | CatalogCommand::RequestFeatureAnalysis { .. }
        | CatalogCommand::RequestWavMetadata { .. }
        | CatalogCommand::RequestHarvestFacts { .. }
        | CatalogCommand::RequestHarvestFilter { .. }
        | CatalogCommand::RequestCollectionHistory { .. }
        | CatalogCommand::RequestCollectionHistoryGroup { .. }
        | CatalogCommand::ApplyPreparedCollectionOperation { .. }
        | CatalogCommand::RequestSimilarity { .. }
        | CatalogCommand::RequestTransientMarkers { .. }
        | CatalogCommand::RequestSourceContentDigest { .. }
        | CatalogCommand::ConsumeReplay { .. }
        | CatalogCommand::ApplyCuration { .. }
        | CatalogCommand::ApplyCurationOperation { .. }
        | CatalogCommand::ApplyRatingDecay { .. }
        | CatalogCommand::ReadSnapshot { .. } => true,
        CatalogCommand::RequestIndexPage { .. }
        | CatalogCommand::RequestIndexInspection { .. }
        | CatalogCommand::RequestTagPage { .. }
        | CatalogCommand::RequestTagAssignments { .. }
        | CatalogCommand::RequestTagAssignmentPreview { .. }
        | CatalogCommand::RequestTagFilter { .. }
        | CatalogCommand::RequestDisplayLabels { .. }
        | CatalogCommand::RequestCurationTagFacts { .. }
        | CatalogCommand::Register { .. }
        | CatalogCommand::RegisterVerified { .. }
        | CatalogCommand::Remove { .. }
        | CatalogCommand::Shutdown => false,
    }
}
