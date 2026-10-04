//! Worker-local parked owners. This module never transfers a SQLite owner off its worker.
use super::*;
use yield_library::sample_sources::db::write::SourceWalPreparationError;

enum ParkedDatabase {
    Held(SourceWalPreparation),
    Failed(SourceWalPreparationFailure),
}
impl ParkedDatabase {
    fn abort(self) -> Result<CatalogSourceDatabase, Self> {
        match self {
            Self::Held(held) => held
                .abort()
                .map(|writer| CatalogSourceDatabase { writer })
                .map_err(Self::Failed),
            Self::Failed(failed) => failed
                .into_writer()
                .map(|writer| CatalogSourceDatabase { writer })
                .map_err(Self::Failed),
        }
    }
}
struct LostOwner(Arc<Ticket>, bool);
impl Drop for LostOwner {
    fn drop(&mut self) {
        if !self.1 {
            self.0.set(CatalogSourceWalStatus::Unavailable(
                CatalogSourceWalRejection::OwnerLost,
            ));
        }
    }
}
struct Slot {
    source: RegisteredSource<ParkedDatabase>,
    // Declaration order keeps the genuine binding ahead of its shared byte credit on drop.
    binding: Option<yield_library::sample_sources::db::write::SourceWalBinding>,
    scope: Option<ScopeView>,
    owner: LostOwner,
    expected: SourceRevision,
    deadline: Instant,
    rejection: Option<CatalogSourceWalRejection>,
}
pub(crate) struct WorkerReservations {
    control: Control,
    slots: HashMap<SourceId, Slot>,
    unresolved: HashMap<SourceId, LostOwner>,
}
impl WorkerReservations {
    pub(crate) fn new(control: Control) -> Self {
        Self {
            control,
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
            let Some(request) = self.control.take() else {
                break;
            };
            self.prepare(request, admission, sources);
        }
    }
    fn prepare(
        &mut self,
        request: PrepareRequest,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &mut HashMap<SourceId, RegisteredSource>,
    ) {
        let deny = |reason| {
            request
                .ticket
                .finish(CatalogSourceWalStatus::Rejected(reason))
        };
        if request.ticket.cancelled.load(Ordering::Acquire) {
            deny(CatalogSourceWalRejection::Cancelled);
            return;
        }
        if Instant::now() >= request.deadline {
            deny(CatalogSourceWalRejection::Deadline);
            return;
        }
        if !current(admission, &request.expected) {
            deny(CatalogSourceWalRejection::Scope);
            return;
        }
        let id = request.expected.source_id().clone();
        if self.is_held(&id) {
            deny(CatalogSourceWalRejection::Busy);
            return;
        }
        if !self.unresolved.is_empty() {
            // A consumed owner lacking an exact acknowledgement has no trustworthy live
            // identity to exclude physical aliases. Keep new pre-intent work fail-closed.
            deny(CatalogSourceWalRejection::OwnerLost);
            return;
        }
        let Some(source) = sources.remove(&id) else {
            deny(CatalogSourceWalRejection::Scope);
            return;
        };
        // Full revision/physical binding validation belongs INSIDE the retained primitive.
        // Its audit getter can fail rollback; an extra getter here would return a callable
        // writer with an unconfirmed transaction before that primitive acquired custody.
        if source.lifecycle_generation != request.expected.lifecycle_generation() {
            sources.insert(id, source);
            deny(CatalogSourceWalRejection::Scope);
            return;
        }
        let mut rejection = None;
        let mapped: Result<
            RegisteredSource<ParkedDatabase>,
            RegisteredSource<std::convert::Infallible>,
        > = source.try_map_database(|database| {
            #[cfg(test)]
            let prepared = if self.control.shared.unknown_wal.load(Ordering::Acquire) {
                database
                    .writer
                    .prepare_wal_reservation_unknown_for_tests(&request.expected, request.deadline)
            } else {
                database
                    .writer
                    .prepare_wal_reservation(&request.expected, request.deadline)
            };
            #[cfg(not(test))]
            let prepared = database
                .writer
                .prepare_wal_reservation(&request.expected, request.deadline);
            match prepared {
                Ok(held) => Ok(ParkedDatabase::Held(held)),
                Err(failed) => {
                    rejection = Some(classify(failed.error()));
                    Ok(ParkedDatabase::Failed(failed))
                }
            }
        });
        let parked = match mapped {
            Ok(source) => source,
            Err(never) => match never.database {},
        };
        let mut binding = None;
        let mut scope = None;
        if request.require_binding && rejection.is_none() {
            match (&parked.database, parked.source_authority.as_ref()) {
                (ParkedDatabase::Held(held), Some(authority)) => {
                    match self.control.capture_scope(held, authority) {
                        Ok((actual, view)) => {
                            binding = Some(actual);
                            scope = Some(view);
                        }
                        Err(reason) => rejection = Some(reason),
                    }
                }
                _ => rejection = Some(CatalogSourceWalRejection::Scope),
            }
        }
        // Successful holders contain actual owner-validated physical main-file observations,
        // never a caller-supplied pathname or serialized identity used as authority.
        if let ParkedDatabase::Held(held) = &parked.database {
            if self.slots.values().any(|slot| match &slot.source.database {
                ParkedDatabase::Held(other) => {
                    other.revision().database_identity() == held.revision().database_identity()
                }
                ParkedDatabase::Failed(failure) => failure
                    .retained_database_identity()
                    .is_none_or(|identity| identity == held.revision().database_identity()),
            }) {
                rejection = Some(CatalogSourceWalRejection::Busy);
            }
        }
        let mut slot = Slot {
            source: parked,
            binding,
            scope,
            owner: LostOwner(Arc::clone(&request.ticket), false),
            expected: request.expected.clone(),
            deadline: request.deadline,
            rejection,
        };
        if slot.rejection.is_none()
            && (request.ticket.cancelled.load(Ordering::Acquire)
                || !current(admission, &request.expected))
        {
            slot.rejection = Some(CatalogSourceWalRejection::Cancelled);
        }
        match &slot.source.database {
            ParkedDatabase::Held(held) if slot.rejection.is_none() && !held.is_expired() => {
                request.ticket.set(CatalogSourceWalStatus::Prepared(
                    CatalogSourceWalObservation {
                        revision: request.expected,
                        correlation: request.correlation,
                        nonce: held.nonce().to_string(),
                        wal_bytes: held.wal_bytes(),
                        scope: slot.scope.clone(),
                    },
                ));
            }
            ParkedDatabase::Held(_) => {
                slot.rejection
                    .get_or_insert(CatalogSourceWalRejection::Deadline);
                request.ticket.set(CatalogSourceWalStatus::Aborting);
            }
            ParkedDatabase::Failed(_) => request
                .ticket
                .set(CatalogSourceWalStatus::Unavailable(slot.rejection.unwrap())),
        }
        self.slots.insert(id, slot);
        // A rejected/expired preparation is immediately rolled back, without another receive.
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
                let cancelled = slot.owner.0.cancelled.load(Ordering::Acquire);
                let changed = !current(admission, &slot.expected);
                let validation = match &slot.source.database {
                    ParkedDatabase::Held(held) => {
                        if let Some(binding) = &slot.binding {
                            match slot.source.source_authority.as_ref() {
                                Some(authority) => held
                                    .validate_wal_binding(binding, authority)
                                    .err()
                                    .map(|e| classify(&e)),
                                None => Some(CatalogSourceWalRejection::Scope),
                            }
                        } else {
                            held.revalidate_owner().err().map(|e| classify(&e))
                        }
                    }
                    ParkedDatabase::Failed(_) => slot.rejection,
                };
                if slot.rejection.is_some()
                    || cancelled
                    || changed
                    || Instant::now() >= slot.deadline
                    || validation.is_some()
                {
                    if slot.rejection.is_none() {
                        slot.rejection = if cancelled {
                            None
                        } else if changed {
                            Some(CatalogSourceWalRejection::Scope)
                        } else {
                            Some(validation.unwrap_or(CatalogSourceWalRejection::Deadline))
                        };
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
        slot.owner.0.set(CatalogSourceWalStatus::Aborting);
        match slot.source.try_map_database(ParkedDatabase::abort) {
            Ok(source) => {
                assert!(
                    !sources.contains_key(id),
                    "parked source cannot acquire a second SQL owner"
                );
                sources.insert(id.clone(), source);
                drop(slot.binding.take());
                drop(slot.scope.take());
                slot.owner.1 = true;
                slot.owner.0.finish(slot.rejection.map_or(
                    CatalogSourceWalStatus::Aborted,
                    CatalogSourceWalStatus::Rejected,
                ));
            }
            Err(source) => {
                slot.source = source;
                slot.owner.0.set(CatalogSourceWalStatus::Unavailable(
                    slot.rejection
                        .unwrap_or(CatalogSourceWalRejection::OwnerLost),
                ));
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
    // Called only after every successful reservation has aborted, before any journal wait.
    // Sticky failures cannot be replaced by a freshly opened owner for the same source ID.
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
            // No scalar, fixture ID, or missing registration can stand in for original authority.
            let _ = poll_pending_source_authority(&mut slot.source);
            if slot.source.source_authority_receipt.is_none()
                || slot.source.source_authority.is_none()
            {
                self.slots.insert(id, slot);
                continue;
            }
            let failed = slot.source.try_map_database(|db| match db {
                ParkedDatabase::Failed(failure) => Ok(failure),
                held => Err(held),
            });
            let source = match failed {
                Ok(source) => source,
                Err(source) => {
                    slot.source = source;
                    self.slots.insert(id, slot);
                    continue;
                }
            };
            match retire_source(id.clone(), source, journal, pending) {
                RetireSourceOutcome::Removed | RetireSourceOutcome::FinalizationPending { .. } => {
                    drop(slot.binding.take());
                    drop(slot.scope.take());
                    slot.owner.1 = true;
                    slot.owner.0.finish(CatalogSourceWalStatus::Retired);
                }
                RetireSourceOutcome::Retained { source, reason } => {
                    slot.source = match source.try_map_database(|failure| {
                        Ok::<_, std::convert::Infallible>(ParkedDatabase::Failed(failure))
                    }) {
                        Ok(source) => source,
                        Err(never) => match never.database {},
                    };
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
                    slot.owner.0.set(CatalogSourceWalStatus::Unavailable(
                        CatalogSourceWalRejection::OwnerLost,
                    ));
                    self.unresolved.insert(id, slot.owner);
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
    pub(crate) fn has_slots(&self) -> bool {
        !self.slots.is_empty() || !self.unresolved.is_empty()
    }
    pub(crate) fn close(&mut self, sources: &mut HashMap<SourceId, RegisteredSource>) {
        self.control.close();
        self.abort_all(sources);
    }
}
impl Drop for WorkerReservations {
    fn drop(&mut self) {
        self.control.close();
    }
}
fn current(admission: &Arc<Mutex<AdmissionState>>, revision: &SourceRevision) -> bool {
    let state = admission.lock().unwrap_or_else(|p| p.into_inner());
    !state.shutdown
        && !state.closed
        && state
            .sources
            .get(revision.source_id())
            .is_some_and(|s| s.active && s.lifecycle_generation == revision.lifecycle_generation())
}
pub(super) fn classify(error: &SourceWalPreparationError) -> CatalogSourceWalRejection {
    match error {
        SourceWalPreparationError::Deadline => CatalogSourceWalRejection::Deadline,
        SourceWalPreparationError::LeaseRenewalRequired => {
            CatalogSourceWalRejection::LeaseRenewalRequired
        }
        SourceWalPreparationError::Owner(_) | SourceWalPreparationError::BindingUnavailable => {
            CatalogSourceWalRejection::Scope
        }
        SourceWalPreparationError::Wal(_) | SourceWalPreparationError::Sqlite(_) => {
            CatalogSourceWalRejection::WalUnavailable
        }
        SourceWalPreparationError::TransactionActive
        | SourceWalPreparationError::RollbackUnconfirmed => CatalogSourceWalRejection::OwnerLost,
    }
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
        CatalogCommand::ConsumeReplay { submission } => slots.is_held(submission.source_id()),
        CatalogCommand::RequestIndexInspection { handle, .. } => slots.is_held(&handle.source_id),
    }
}

// Broad scans, DSP, replay/writes and waits on another owner cannot keep any successful
// pre-intent writer locked while this single worker is executing that normal command.
// Bounded read pages on an unrelated source remain usable without touching parked owners.
pub(crate) fn interrupts_preparation(command: &CatalogCommand) -> bool {
    match command {
        CatalogCommand::RequestAudit { .. }
        | CatalogCommand::RequestFeatureAnalysis { .. }
        | CatalogCommand::RequestWavMetadata { .. }
        | CatalogCommand::RequestHarvestFacts { .. }
        | CatalogCommand::RequestHarvestFilter { .. }
        | CatalogCommand::RequestCollectionHistory { .. }
        | CatalogCommand::RequestCollectionHistoryGroup { .. }
        | CatalogCommand::ApplyPreparedCollectionOperation { .. }
        | CatalogCommand::RequestSimilarity { .. }
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
