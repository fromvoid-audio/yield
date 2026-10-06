//! Real original SQLite workers; registered lifecycle custody and logical plans give no grant.
#[cfg(test)]
mod integration {
    use super::super::*;
    use crate::operation_journal::{
        FullCapacityAllocation, FullCapacityAllocationClass as Class, FullCapacityPlan,
        ParticipantId,
    };
    use crate::sample_sources::library::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus, LIBRARY_DB_FILE_NAME,
    };
    use crate::sample_sources::wal_transport::{SourceWalProducerOwner, SourceWalTransport};
    use crate::sample_sources::{SourceDatabase, SourceRevision, SourceWriter};
    use std::{collections::HashMap, sync::atomic::AtomicBool, time::Instant};

    fn source(fixture: &Fixture, name: &str) -> SourceWriter {
        let media = fixture._base.path().join(name);
        fs::create_dir(&media).unwrap();
        fs::write(media.join("one.wav"), b"original fixture bytes").unwrap();
        let writer = SourceWriter::new_for_scan(
            SourceDatabase::open_for_catalog_with_profile_guard(&media, &fixture.owner, name)
                .unwrap(),
            SourceId::from_string(name),
            1,
        )
        .unwrap();
        writer
            .catalog_database()
            .upsert_file(std::path::Path::new("one.wav"), 1, 1)
            .unwrap();
        let generation = writer.begin_scan_directory_truth_generation(0).unwrap();
        let fence = writer.current_scan_fence().unwrap();
        let batch = writer.catalog_database().write_batch().unwrap();
        writer
            .commit_scan_audit_completion(
                &fence,
                batch,
                generation,
                1,
                fence.root_identity().clone(),
                None,
            )
            .unwrap();
        writer
    }
    fn feature_source(fixture: &Fixture, name: &str) -> SourceWriter {
        use crate::sample_sources::readiness::{
            FeatureArtifactPayload, FeatureArtifactPublication, ReadinessClassification,
            ReadinessDeficit,
        };
        let media = fixture._base.path().join(name);
        fs::create_dir(&media).unwrap();
        let bytes = b"original fixture bytes";
        let path = media.join("one.wav");
        fs::write(&path, bytes).unwrap();
        let metadata = path.metadata().unwrap();
        let identity =
            crate::filesystem_identity::stable_filesystem_identity(&path, &metadata).unwrap();
        let mut writer = SourceWriter::new_for_scan(
            SourceDatabase::open_for_catalog_with_profile_guard(&media, &fixture.owner, name)
                .unwrap(),
            SourceId::from_string(name),
            1,
        )
        .unwrap();
        let mut batch = writer.catalog_database().write_batch().unwrap();
        batch
            .upsert_file_with_hash(
                std::path::Path::new("one.wav"),
                metadata.len(),
                1,
                &blake3::hash(bytes).to_hex(),
            )
            .unwrap();
        batch
            .set_file_identity(std::path::Path::new("one.wav"), Some(&identity))
            .unwrap();
        batch.commit().unwrap();
        let generation = writer.begin_scan_directory_truth_generation(0).unwrap();
        let fence = writer.current_scan_fence().unwrap();
        let batch = writer.catalog_database().write_batch().unwrap();
        writer
            .commit_scan_audit_completion(
                &fence,
                batch,
                generation,
                1,
                fence.root_identity().clone(),
                None,
            )
            .unwrap();
        let revision = writer.current_revision().unwrap();
        let cancel = AtomicBool::new(false);
        let target = writer
            .prepare_feature_readiness_target(
                &revision,
                std::path::Path::new("one.wav"),
                30,
                &cancel,
            )
            .unwrap();
        writer
            .enqueue_readiness_jobs(
                &revision,
                &[ReadinessDeficit {
                    target: target.clone(),
                    reason: ReadinessClassification::Pending,
                    enqueued_at: None,
                }],
                30,
            )
            .unwrap();
        let claim = writer
            .claim_feature_artifact(&revision, &target, 31, 100)
            .unwrap()
            .unwrap();
        let payload = FeatureArtifactPayload::from_v1_vector(&[0.25; 183]).unwrap();
        assert!(matches!(
            writer
                .publish_feature_artifact(&revision, &claim, &payload, 32, &cancel)
                .unwrap(),
            FeatureArtifactPublication::Recorded(_)
        ));
        writer
    }
    fn fence(revision: &SourceRevision) -> StoredSourceFence {
        StoredSourceFence::new(
            revision.source_id().clone(),
            std::str::from_utf8(revision.root_identity().as_bytes()).unwrap(),
            revision.database_identity(),
            revision.lifecycle_generation(),
            revision.revision(),
            revision.lease_epoch(),
        )
        .unwrap()
    }
    fn command(
        fixture: &Fixture,
        revisions: &[SourceRevision],
    ) -> (
        UniversalCommandArgs,
        ParticipantSet,
        [crate::operation_journal::ParticipantId; 2],
    ) {
        let ids = [
            crate::operation_journal::ParticipantId::new(),
            crate::operation_journal::ParticipantId::new(),
        ];
        let mut descriptors = revisions
            .iter()
            .zip(ids)
            .map(|(revision, id)| {
                ParticipantDescriptor::new(
                    id,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(fence(revision)),
                    None,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        for kind in [
            ParticipantKind::Filesystem,
            ParticipantKind::GlobalLibrary,
            ParticipantKind::Harvest,
        ] {
            descriptors.push(
                ParticipantDescriptor::new(
                    crate::operation_journal::ParticipantId::new(),
                    kind,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .unwrap(),
            );
        }
        for kind in [ParticipantKind::Projection, ParticipantKind::Readiness] {
            descriptors.push(
                ParticipantDescriptor::new(
                    crate::operation_journal::ParticipantId::new(),
                    kind,
                    ParticipantRequirement::Required,
                    Some(fence(&revisions[1])),
                    None,
                )
                .unwrap(),
            );
        }
        let checkpoints = descriptors
            .iter()
            .map(|d| {
                ParticipantCheckpoint::new(d.id(), ParticipantCheckpointState::Pending, None, None)
                    .unwrap()
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
        let file = fs::File::open(
            fixture
                ._base
                .path()
                .join(revisions[0].source_id().as_str())
                .join("one.wav"),
        )
        .unwrap();
        let metadata = file.metadata().unwrap();
        let identity =
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file).unwrap();
        let modified_ns = i64::try_from(
            metadata
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap();
        let args = UniversalCommandArgs::Copy(
            CopyIntentArgs::new(
                CopyEndpoint::new(
                    ids[0],
                    revisions[0].source_id().clone(),
                    StoredRelativePath::new("one.wav").unwrap(),
                )
                .unwrap(),
                CopyEndpoint::new(
                    ids[1],
                    revisions[1].source_id().clone(),
                    StoredRelativePath::new("copied.wav").unwrap(),
                )
                .unwrap(),
                CopySourceExpectation::new(
                    revisions[0].revision(),
                    identity,
                    metadata.len(),
                    modified_ns,
                )
                .unwrap(),
                CopyCollisionPolicy::NoReplace,
                None,
            )
            .unwrap(),
        );
        crate::operation_journal::derive_work_plan(&args, &participants).unwrap();
        (args, participants, ids)
    }
    fn plan(
        fixture: &Fixture,
    ) -> (
        FullCapacityPlan,
        AdmissionClaimInventory,
        AdmissionClaimId,
        AdmissionClaimId,
    ) {
        // Logical observed claim correlations only. No capacity provider or native grant is installed.
        let facts = crate::durable_capacity::descriptor_capacity_facts(
            &fs::File::open(fixture.owner.profile_root()).unwrap(),
        )
        .unwrap();
        let volume = facts.identity;
        let unit = facts.allocation_unit;
        let classes = [
            Class::SourceDatabaseCommit,
            Class::SourceWalShm,
            Class::GlobalDatabaseCommit,
            Class::GlobalWalShm,
        ];
        let allocations = classes
            .iter()
            .map(|class| {
                FullCapacityAllocation::new(
                    volume.clone(),
                    unit,
                    *class,
                    8192,
                    8192,
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .unwrap()
            })
            .collect();
        let claims = classes
            .iter()
            .map(|class| {
                AdmissionClaim::new(AdmissionClaimId::new(), volume.clone(), *class, 8192).unwrap()
            })
            .collect::<Vec<_>>();
        let main = claims[0].claim_id();
        let side = claims[1].claim_id();
        (
            FullCapacityPlan::new(allocations).unwrap(),
            AdmissionClaimInventory::new(claims).unwrap(),
            main,
            side,
        )
    }
    struct SourceActor {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        events: Arc<Mutex<Vec<String>>>,
        pause_abort: Arc<AtomicBool>,
        pause_validation: Arc<AtomicBool>,
        validation_paused_seen: Arc<AtomicBool>,
        current_validations: Arc<AtomicUsize>,
        drain_requested: Arc<AtomicBool>,
        drained: Arc<AtomicBool>,
        abandon_requested: Arc<AtomicBool>,
        abandoned: Arc<AtomicBool>,
        tokens: Arc<Mutex<HashMap<String, crate::sample_sources::db::RegisteredDecisionBoundary>>>,
    }
    impl SourceActor {
        fn start(
            writers: Vec<SourceWriter>,
            inbox: crate::sample_sources::wal_transport::SourceWalTransportInbox,
        ) -> Self {
            Self::start_with_lease_observation(writers, inbox, None)
        }
        fn start_with_lease_observation(
            writers: Vec<SourceWriter>,
            inbox: crate::sample_sources::wal_transport::SourceWalTransportInbox,
            observation: Option<
                mpsc::SyncSender<(
                    crate::sample_sources::db::SourceLeaseMaintenanceAllocation,
                    crate::operation_journal::source_lease_binding::StoredSourceLeaseFence,
                    Instant,
                    crate::sample_sources::db::SourceRegistryReferenceObservation,
                )>,
            >,
        ) -> Self {
            Self::start_with_registration(writers, inbox, observation, true)
        }
        fn start_with_registration(
            writers: Vec<SourceWriter>,
            mut inbox: crate::sample_sources::wal_transport::SourceWalTransportInbox,
            observation: Option<
                mpsc::SyncSender<(
                    crate::sample_sources::db::SourceLeaseMaintenanceAllocation,
                    crate::operation_journal::source_lease_binding::StoredSourceLeaseFence,
                    Instant,
                    crate::sample_sources::db::SourceRegistryReferenceObservation,
                )>,
            >,
            register: bool,
        ) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = Arc::clone(&stop);
            let events = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&events);
            let pause_abort = Arc::new(AtomicBool::new(false));
            let paused = Arc::clone(&pause_abort);
            let pause_validation = Arc::new(AtomicBool::new(false));
            let validation_paused = Arc::clone(&pause_validation);
            let validation_paused_seen = Arc::new(AtomicBool::new(false));
            let validation_pause_seen = Arc::clone(&validation_paused_seen);
            let current_validations = Arc::new(AtomicUsize::new(0));
            let validated_count = Arc::clone(&current_validations);
            let drain_requested = Arc::new(AtomicBool::new(false));
            let draining = Arc::clone(&drain_requested);
            let drained = Arc::new(AtomicBool::new(false));
            let drain_complete = Arc::clone(&drained);
            let abandon_requested = Arc::new(AtomicBool::new(false));
            let abandoning = Arc::clone(&abandon_requested);
            let abandoned = Arc::new(AtomicBool::new(false));
            let abandon_complete = Arc::clone(&abandoned);
            let tokens = Arc::new(Mutex::new(HashMap::new()));
            let published_tokens = Arc::clone(&tokens);
            let worker = thread::spawn(move || {
                let mut writers = writers
                    .into_iter()
                    .map(|w| {
                        (
                            w.current_scan_fence()
                                .unwrap()
                                .source_id()
                                .as_str()
                                .to_owned(),
                            w,
                        )
                    })
                    .collect::<HashMap<_, _>>();
                let mut authorities = writers
                    .iter()
                    .map(|(id, w)| {
                        (
                            id.clone(),
                            w.catalog_database().prepare_source_authority().unwrap(),
                        )
                    })
                    .collect::<HashMap<_, _>>();
                for writer in writers.values() {
                    assert!(matches!(
                        writer.catalog_database().prepare_source_authority(),
                        Err(crate::sample_sources::db::SourceAuthorityError::AlreadyPrepared)
                    ));
                }
                // Drive the genuine confirmation lifecycle without installing a capacity provider.
                // These fixtures prove transport custody, never physical qualification or admission.
                let mut registrations = authorities
                    .iter()
                    .map(|(id, authority)| {
                        let envelope = authority.journal_request().unwrap();
                        let proof = if register {
                            Some(
                                envelope
                                    .request
                                    .accept_confirmation(envelope.confirmation)
                                    .unwrap(),
                            )
                        } else {
                            None
                        };
                        (id.clone(), (envelope.request, proof))
                    })
                    .collect::<HashMap<_, _>>();
                if register {
                    *published_tokens.lock().unwrap() = authorities
                        .iter()
                        .map(|(id, authority)| (id.clone(), authority.decision_boundary().unwrap()))
                        .collect();
                }
                if let Some(sender) = observation {
                    assert_eq!(writers.len(), 1);
                    let id = writers.keys().next().unwrap().clone();
                    let writer = writers.remove(&id).unwrap();
                    let reference = writer
                        .capture_registry_reference_observation(Some(&authorities[&id]))
                        .unwrap();
                    let deadline = Instant::now() + Duration::from_secs(2);
                    let allocation = writer
                        .capture_lease_maintenance_allocation(deadline)
                        .unwrap();
                    let prepared = writer
                        .prepare_lease_maintenance(allocation.clone(), deadline)
                        .unwrap();
                    let binding = prepared.capture_lease_binding(&authorities[&id]).unwrap();
                    let fence = binding.stored_fence().clone();
                    let writer = prepared.abort().unwrap();
                    drop(binding);
                    writers.insert(id, writer);
                    sender
                        .send((allocation, fence, deadline, reference))
                        .unwrap();
                }
                let mut held: HashMap<String, SourceWalProducerOwner> = HashMap::new();
                let mut validated = std::collections::HashSet::new();
                loop {
                    if draining.swap(false, Ordering::AcqRel) {
                        for (request, proof) in registrations.values_mut() {
                            request
                                .begin_drain(proof.take().unwrap().into_drain_marker())
                                .unwrap();
                        }
                        drain_complete.store(true, Ordering::Release);
                    }
                    if abandoning.swap(false, Ordering::AcqRel) {
                        authorities.clear();
                        abandon_complete.store(true, Ordering::Release);
                    }
                    if stopped.load(Ordering::Acquire) {
                        inbox.close();
                    }
                    while let Some(command) = inbox.try_next_any() {
                        let id = command.source_id().to_owned();
                        let Some(writer) = writers.remove(&id) else {
                            command.reject(
                            crate::sample_sources::wal_transport::SourceWalTransportRejection::Busy,
                        );
                            continue;
                        };
                        recorded.lock().unwrap().push(format!("acquire:{id}"));
                        let mut owner = command.acquire(writer);
                        let result = owner.publish_prepared(authorities.get(&id));
                        if let Err(reason) = result {
                            // Match the actual catalog producer: carry publication refusal
                            // through original cleanup, rather than reporting a caller abort.
                            owner.request_abort(Some(reason));
                        }
                        if result.is_ok() {
                            assert_eq!(
                                registrations[&id].0.registered_uses_for_tests(),
                                1,
                                "successful publication retains original use {id}"
                            );
                        }
                        held.insert(id, owner);
                    }
                    validation_pause_seen
                        .store(validation_paused.load(Ordering::Acquire), Ordering::Release);
                    let mut remaining = HashMap::new();
                    for (id, mut owner) in held.drain() {
                        if owner.settlement_held() {
                            let round = owner.current_round_for_tests().unwrap();
                            owner.request_abort(None);
                            let validation = owner.revalidate(authorities.get(&id));
                            if validation != Err(crate::sample_sources::wal_transport::SourceWalTransportRejection::Busy) {
                                // NoAttempt may resolve between the first fence observation and
                                // this call. Only actual resolution permits ordinary cleanup.
                                assert!(!round.cleanup_fenced_for_tests());
                            }
                            match owner.into_writer() {
                                Err(retained) => {
                                    remaining.insert(id, retained);
                                }
                                Ok(writer) => {
                                    assert!(
                                        !round.cleanup_fenced_for_tests(),
                                        "unresolved ACK restored a writer"
                                    );
                                    assert_eq!(registrations[&id].0.registered_uses_for_tests(), 0);
                                    recorded.lock().unwrap().push(format!("reuse:{id}"));
                                    writers.insert(id, writer);
                                }
                            }
                            continue;
                        }
                        if !stopped.load(Ordering::Acquire)
                            && owner.is_cancelled()
                            && paused.load(Ordering::Acquire)
                        {
                            remaining.insert(id, owner);
                            continue;
                        }
                        if stopped.load(Ordering::Acquire)
                            || owner.is_cancelled()
                            || !owner.is_prepared()
                            || owner.rejection().is_some()
                            || Instant::now() >= owner.deadline()
                            || (!validation_paused.load(Ordering::Acquire)
                                && owner.revalidate(authorities.get(&id)).is_err())
                        {
                            let original_round = owner.current_round_for_tests();
                            owner.request_abort(None);
                            if let Some(round) = original_round {
                                assert!(
                                    round.invalidated_for_tests(),
                                    "round must close before actual SQLite rollback"
                                );
                            }
                            match owner.into_writer() {
                                Ok(writer) => {
                                    assert_eq!(
                                        registrations[&id].0.registered_uses_for_tests(),
                                        0,
                                        "count released only after original cleanup"
                                    );
                                    recorded.lock().unwrap().push(format!("reuse:{id}"));
                                    writers.insert(id, writer);
                                }
                                Err(owner) => {
                                    remaining.insert(id, owner);
                                }
                            }
                        } else {
                            if owner.current_round_for_tests().is_some()
                                && validated.insert(id.clone())
                            {
                                validated_count.fetch_add(1, Ordering::Release);
                            }
                            remaining.insert(id, owner);
                        }
                    }
                    held = remaining;
                    if stopped.load(Ordering::Acquire) && held.is_empty() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                // Real ordinary owners close before their genuine prepared-registration guards.
                drop(writers);
                drop(authorities);
                drop(registrations);
            });
            Self {
                stop,
                worker: Some(worker),
                events,
                pause_abort,
                pause_validation,
                validation_paused_seen,
                current_validations,
                drain_requested,
                drained,
                abandon_requested,
                abandoned,
                tokens,
            }
        }
    }
    impl Drop for SourceActor {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !thread::panicking() {
                    result.unwrap();
                }
            }
        }
    }
    fn wait(
        handle: &WalPreparationHandle,
        predicate: impl Fn(&WalPreparationStatus) -> bool,
    ) -> WalPreparationStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = handle.status();
            if predicate(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "{status:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn all_owner_real_journal_collects_two_sources_global_in_order_and_confirms_abort() {
        real_current_round(false);
    }
    #[test]
    fn all_owner_cancel_missing_current_reply_unwinds_original_prefix_without_identity() {
        real_current_round(true);
    }
    fn real_current_round(cancel: bool) {
        let (fixture, mut journal) = open_owner("all-owner-real-ordered");
        let mut global = GlobalLibraryWriter::start(&fixture.owner).unwrap();
        while global.status() != GlobalLibraryWriterStatus::Available {
            thread::sleep(Duration::from_millis(5));
        }
        let writers = vec![source(&fixture, "owner-a"), source(&fixture, "owner-b")];
        let revisions = writers
            .iter()
            .map(|w| w.current_revision().unwrap())
            .collect::<Vec<_>>();
        let (args, participants, ids) = command(&fixture, &revisions);
        let (plan, claims, main, side) = plan(&fixture);
        let (requester, inbox) = SourceWalTransport::channel();
        let actor = SourceActor::start(writers, inbox);
        actor.pause_validation.store(true, Ordering::Release);
        // Reverse caller order must not change actual physical acquisition order.
        let sources = (0..2)
            .rev()
            .map(|i| {
                WalPreparationSource::new(
                    requester.clone(),
                    ids[i],
                    main,
                    side,
                    SourceWalPreparationInput::Audited(revisions[i].clone()),
                )
            })
            .collect();
        let input = WalPreparationInput::from_universal(
            &args,
            &participants,
            &plan,
            &claims,
            sources,
            Some(global.wal_preparation_client()),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let budget = input.budget_for_test();
        let client = journal.client();
        let handle = journal
            .client()
            .prepare_wal_owners(input)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        let acquired_deadline = Instant::now() + Duration::from_secs(1);
        while actor.events.lock().unwrap().len() < 2 {
            assert!(Instant::now() < acquired_deadline);
            thread::sleep(Duration::from_millis(1));
        }
        // Both genuine source workers published Prepared and retain actual SQLite writers.
        // Those historical replies cannot complete the collector's fresh validation round.
        for _ in 0..10 {
            assert!(matches!(handle.status(), WalPreparationStatus::Preparing));
            thread::sleep(Duration::from_millis(5));
        }
        if cancel {
            handle.abort();
            assert!(matches!(
                wait(&handle, terminal),
                WalPreparationStatus::Aborted
            ));
            assert_eq!(
                journal
                    .prepared_admission_id_allocations
                    .load(Ordering::Relaxed),
                0
            );
            drop(handle);
            drop(actor);
            assert!(global.shutdown());
            assert!(journal.shutdown());
            // Terminal publication precedes destruction of the original collector's
            // CurrentRound. Its shared debit remains funded until the last real
            // holder drops; all original joins above establish that boundary.
            assert_eq!(budget.used(), 0);
            return;
        }
        actor.pause_validation.store(false, Ordering::Release);
        let ready = wait(&handle, |status| {
            !matches!(
                status,
                WalPreparationStatus::Preparing | WalPreparationStatus::Aborting
            )
        });
        let WalPreparationStatus::Ready(view) = ready else {
            panic!("{ready:?}");
        };
        assert_eq!(view.databases().len(), 3);
        let counted_deadline = Instant::now() + Duration::from_secs(1);
        while actor.current_validations.load(Ordering::Acquire) < 2 {
            assert!(Instant::now() < counted_deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(actor.current_validations.load(Ordering::Acquire), 2);
        assert!(matches!(
            view.databases()[0],
            WalPreparedDatabase::Global(_)
        ));
        assert!(view.databases()[1].database_identity() < view.databases()[2].database_identity());
        let mut outside = Vec::new();
        for child in ["owner-a", "owner-b"] {
            let connection = rusqlite::Connection::open(
                fixture
                    .owner
                    .profile_root()
                    .join(child)
                    .join(crate::sample_sources::DB_FILE_NAME),
            )
            .unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            outside.push(connection);
        }
        outside.push(
            rusqlite::Connection::open(fixture.owner.profile_root().join(LIBRARY_DB_FILE_NAME))
                .unwrap(),
        );
        for connection in &outside {
            connection.busy_timeout(Duration::ZERO).unwrap();
            assert_eq!(
                connection
                    .execute_batch("BEGIN IMMEDIATE")
                    .unwrap_err()
                    .sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
        }
        let historical = view.clone();
        actor.pause_abort.store(true, Ordering::Release);
        handle.abort();
        wait(&handle, |s| matches!(s, WalPreparationStatus::Aborting));
        // Abort is only a request. Until the original last source confirms rollback,
        // every acquired owner, including global, must still exclude other writers.
        for connection in &outside {
            assert!(connection.execute_batch("BEGIN IMMEDIATE").is_err());
        }
        actor.pause_abort.store(false, Ordering::Release);
        assert!(matches!(
            wait(&handle, |s| matches!(
                s,
                WalPreparationStatus::Aborted
                    | WalPreparationStatus::Unavailable(_)
                    | WalPreparationStatus::Rejected(_)
            )),
            WalPreparationStatus::Aborted
        ));
        for connection in &outside {
            connection
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
        assert_eq!(historical.databases().len(), 3);
        let events = actor.events.lock().unwrap().clone();
        assert_eq!(events.len(), 4, "{events:?}");
        assert_eq!(
            events[2].strip_prefix("reuse:"),
            events[1].strip_prefix("acquire:")
        );
        assert_eq!(
            events[3].strip_prefix("reuse:"),
            events[0].strip_prefix("acquire:")
        );
        assert_eq!(
            journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        assert!(journal.shutdown());
        let rows = rusqlite::Connection::open(
            fixture
                .owner
                .profile_root()
                .join(crate::operation_journal::store::JOURNAL_DATABASE_ROOT_NAME)
                .join(crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME),
        )
        .unwrap()
        .query_row("SELECT count(*) FROM operation_journal", [], |r| {
            r.get::<_, u64>(0)
        })
        .unwrap();
        assert_eq!(rows, 0);
        assert!(budget.used() > 0);
        assert_eq!(
            ORDINARY_INPUT_BYTES - client.shared.input_bytes.lock().unwrap().remaining[0],
            64 * 1024
        );
        let held_bytes = budget.used();
        let another_copy = historical.clone();
        assert_eq!(budget.used(), held_bytes);
        drop(historical);
        drop(view);
        assert_eq!(budget.used(), held_bytes);
        drop(another_copy);
        drop(handle);
        assert_eq!(budget.used(), 0);
        assert_eq!(
            client.shared.input_bytes.lock().unwrap().remaining[0],
            ORDINARY_INPUT_BYTES
        );
        drop(actor);
        assert!(global.shutdown());
    }

    struct Rig {
        fixture: Fixture,
        journal: OperationJournalOwner,
        global: GlobalLibraryWriter,
        revisions: Vec<SourceRevision>,
        requester: SourceWalTransport,
        actor: Option<SourceActor>,
    }
    impl Rig {
        fn new(name: &str) -> Self {
            Self::with_registration(name, true)
        }
        fn with_registration(name: &str, register: bool) -> Self {
            let (fixture, journal) = open_owner(name);
            let global = GlobalLibraryWriter::start(&fixture.owner).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while global.status() != GlobalLibraryWriterStatus::Available {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            let writers = vec![source(&fixture, "owner-a"), source(&fixture, "owner-b")];
            let revisions = writers
                .iter()
                .map(|w| w.current_revision().unwrap())
                .collect();
            let (requester, inbox) = SourceWalTransport::channel();
            let actor = SourceActor::start_with_registration(writers, inbox, None, register);
            Self {
                fixture,
                journal,
                global,
                revisions,
                requester,
                actor: Some(actor),
            }
        }
        fn input(&self, deadline: Instant) -> WalPreparationInput {
            let (args, participants, ids) = command(&self.fixture, &self.revisions);
            let (plan, claims, main, side) = plan(&self.fixture);
            let sources = (0..2)
                .map(|i| {
                    WalPreparationSource::new(
                        self.requester.clone(),
                        ids[i],
                        main,
                        side,
                        SourceWalPreparationInput::Audited(self.revisions[i].clone()),
                    )
                })
                .collect();
            WalPreparationInput::from_universal(
                &args,
                &participants,
                &plan,
                &claims,
                sources,
                Some(self.global.wal_preparation_client()),
                deadline,
            )
            .unwrap()
        }
        fn submit(&self, input: WalPreparationInput) -> WalPreparationHandle {
            self.journal
                .client()
                .prepare_wal_owners(input)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
        }
        fn outside_source(&self, index: usize) -> rusqlite::Connection {
            let connection = rusqlite::Connection::open(
                self.fixture
                    .owner
                    .profile_root()
                    .join(self.revisions[index].source_id().as_str())
                    .join(crate::sample_sources::DB_FILE_NAME),
            )
            .unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            connection
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            drop(self.actor.take());
            self.journal.shutdown();
            self.global.shutdown();
        }
    }
    fn terminal(status: &WalPreparationStatus) -> bool {
        matches!(
            status,
            WalPreparationStatus::Aborted
                | WalPreparationStatus::Rejected(_)
                | WalPreparationStatus::Unavailable(_)
        )
    }
    #[test]
    fn all_owner_prepared_registration_cannot_publish_original_journal_custody() {
        let mut rig = Rig::with_registration("all-owner-prepared-refusal", false);
        let input = rig.input(Instant::now() + Duration::from_secs(2));
        let budget = input.budget_for_test();
        let handle = rig.submit(input);
        let status = wait(&handle, terminal);
        assert!(
            matches!(status, WalPreparationStatus::Rejected(_)),
            "{status:?}"
        );
        for index in 0..2 {
            rig.outside_source(index)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
        assert!(
            rig.actor
                .as_ref()
                .unwrap()
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.starts_with("reuse:"))
        );
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        assert!(rig.journal.shutdown());
        let rows = rusqlite::Connection::open(
            rig.fixture
                .owner
                .profile_root()
                .join(crate::operation_journal::store::JOURNAL_DATABASE_ROOT_NAME)
                .join(crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME),
        )
        .unwrap()
        .query_row("SELECT count(*) FROM operation_journal", [], |r| {
            r.get::<_, u64>(0)
        })
        .unwrap();
        assert_eq!(rows, 0);
        drop(handle);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn all_owner_busy_last_real_database_unwinds_acquired_prefix_without_identity() {
        let rig = Rig::new("all-owner-busy-prefix");
        let index = if rig.revisions[0].database_identity() < rig.revisions[1].database_identity() {
            1
        } else {
            0
        };
        let busy = rig.outside_source(index);
        busy.execute_batch("BEGIN IMMEDIATE").unwrap();
        let input = rig.input(Instant::now() + Duration::from_secs(2));
        let budget = input.budget_for_test();
        let handle = rig.submit(input);
        let result = wait(&handle, terminal);
        assert!(
        matches!(
            result,
            WalPreparationStatus::Rejected(WalPreparationError::Source(
                crate::sample_sources::wal_transport::SourceWalTransportRejection::Busy | crate::sample_sources::wal_transport::SourceWalTransportRejection::WalUnavailable
            ))
        ),
        "{result:?}"
    );
        let events = rig.actor.as_ref().unwrap().events.lock().unwrap().clone();
        assert_eq!(events.len(), 4, "{events:?}");
        assert_eq!(
            events[2].strip_prefix("reuse:"),
            events[1].strip_prefix("acquire:")
        );
        assert_eq!(
            events[3].strip_prefix("reuse:"),
            events[0].strip_prefix("acquire:")
        );
        rig.outside_source(1 - index)
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
        let global =
            rusqlite::Connection::open(rig.fixture.owner.profile_root().join(LIBRARY_DB_FILE_NAME))
                .unwrap();
        global.busy_timeout(Duration::ZERO).unwrap();
        global.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        drop(handle);
        assert_eq!(budget.used(), 0);
        busy.execute_batch("ROLLBACK").unwrap();
    }
    #[test]
    fn all_owner_real_unknown_schema_refuses_and_confirms_prefix_cleanup() {
        let rig = Rig::new("all-owner-schema-prefix");
        let index = if rig.revisions[0].database_identity() < rig.revisions[1].database_identity() {
            1
        } else {
            0
        };
        let unknown = rig.outside_source(index);
        unknown
            .execute_batch("CREATE VIEW unknown_contract AS SELECT 1 AS x")
            .unwrap();
        let handle = rig.submit(rig.input(Instant::now() + Duration::from_secs(2)));
        let result = wait(&handle, terminal);
        assert!(
            matches!(result, WalPreparationStatus::Rejected(_)),
            "{result:?}"
        );
        for i in 0..2 {
            rig.outside_source(i)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
    }
    #[test]
    fn all_owner_caller_drop_and_original_deadline_release_real_custody() {
        let rig = Rig::new("all-owner-drop-deadline");
        let input = rig.input(Instant::now() + Duration::from_secs(2));
        let budget = input.budget_for_test();
        let handle = rig.submit(input);
        assert!(matches!(
            wait(&handle, |s| matches!(s, WalPreparationStatus::Ready(_))
                || terminal(s)),
            WalPreparationStatus::Ready(_)
        ));
        drop(handle);
        let deadline = Instant::now() + Duration::from_secs(3);
        while budget.used() != 0 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        for i in 0..2 {
            rig.outside_source(i)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
        let handle = rig.submit(rig.input(Instant::now() + Duration::from_millis(400)));
        assert!(matches!(
            wait(&handle, |s| matches!(s, WalPreparationStatus::Ready(_))
                || terminal(s)),
            WalPreparationStatus::Ready(_)
        ));
        let result = wait(&handle, terminal);
        assert!(
            matches!(
                result,
                WalPreparationStatus::Rejected(WalPreparationError::Deadline)
            ),
            "{result:?}"
        );
        for i in 0..2 {
            rig.outside_source(i)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
    }
    #[test]
    fn all_owner_copied_scope_retains_original_pool_and_cannot_acknowledge_next_handshake() {
        let rig = Rig::new("all-owner-copied-pool");
        let mut copies = Vec::new();
        let mut refused = false;
        for _ in 0..5 {
            let handle = rig.submit(rig.input(Instant::now() + Duration::from_secs(2)));
            match wait(&handle, |s| {
                matches!(s, WalPreparationStatus::Ready(_)) || terminal(s)
            }) {
                WalPreparationStatus::Ready(view) => {
                    copies.push(view.clone());
                    handle.abort();
                    assert!(matches!(
                        wait(&handle, terminal),
                        WalPreparationStatus::Aborted
                    ));
                }
                WalPreparationStatus::Rejected(WalPreparationError::Full)
                | WalPreparationStatus::Rejected(WalPreparationError::Global(
                    crate::sample_sources::library::GlobalPreparationError::InputBound,
                ))
                | WalPreparationStatus::Rejected(WalPreparationError::Source(
                    crate::sample_sources::wal_transport::SourceWalTransportRejection::Full,
                )) => {
                    refused = true;
                    break;
                }
                result => panic!("{result:?}"),
            }
        }
        assert!(
            refused,
            "retained original scope copies must exhaust their shared original pool"
        );
        drop(copies);
        let handle = rig.submit(rig.input(Instant::now() + Duration::from_secs(2)));
        assert!(matches!(
            wait(&handle, |s| matches!(s, WalPreparationStatus::Ready(_))
                || terminal(s)),
            WalPreparationStatus::Ready(_)
        ));
        handle.abort();
        assert!(matches!(
            wait(&handle, terminal),
            WalPreparationStatus::Aborted
        ));
    }

    #[test]
    fn all_owner_active_handshake_busy_and_lost_reply_require_actual_cancellation() {
        let rig = Rig::new("all-owner-single-context");
        let handle = rig.submit(rig.input(Instant::now() + Duration::from_secs(2)));
        assert!(matches!(
            wait(&handle, |s| matches!(s, WalPreparationStatus::Ready(_))
                || terminal(s)),
            WalPreparationStatus::Ready(_)
        ));
        let rejected = rig
            .journal
            .client()
            .prepare_wal_owners(rig.input(Instant::now() + Duration::from_secs(2)))
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            rejected,
            Err(OperationJournalOwnerError::WalPreparation(
                WalPreparationError::Busy
            ))
        ));
        handle.abort();
        assert!(matches!(
            wait(&handle, terminal),
            WalPreparationStatus::Aborted
        ));
        drop(handle);
        let input = rig.input(Instant::now() + Duration::from_secs(2));
        let budget = input.budget_for_test();
        let lost = rig.journal.client().prepare_wal_owners(input).unwrap();
        drop(lost);
        let deadline = Instant::now() + Duration::from_secs(3);
        while budget.used() != 0 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
    }
    #[test]
    fn all_owner_factories_refuse_missing_duplicate_foreign_claim_and_spare_backing() {
        let rig = Rig::new("all-owner-exact-factory");
        let (args, participants, ids) = command(&rig.fixture, &rig.revisions);
        let (plan, claims, main, side) = plan(&rig.fixture);
        for case in 0..6 {
            let mut sources = (0..2)
                .map(|i| {
                    WalPreparationSource::new(
                        rig.requester.clone(),
                        ids[i],
                        main,
                        side,
                        SourceWalPreparationInput::Audited(rig.revisions[i].clone()),
                    )
                })
                .collect::<Vec<_>>();
            match case {
                0 => {
                    sources.pop();
                }
                1 => {
                    sources[1] = WalPreparationSource::new(
                        rig.requester.clone(),
                        ids[0],
                        main,
                        side,
                        SourceWalPreparationInput::Audited(rig.revisions[0].clone()),
                    );
                }
                2 => {
                    sources[1] = WalPreparationSource::new(
                        rig.requester.clone(),
                        ids[1],
                        side,
                        main,
                        SourceWalPreparationInput::Audited(rig.revisions[1].clone()),
                    );
                }
                3 => {
                    sources[1] = WalPreparationSource::new(
                        rig.requester.clone(),
                        ids[1],
                        main,
                        side,
                        SourceWalPreparationInput::Audited(rig.revisions[0].clone()),
                    );
                }
                4 => {
                    sources.reserve(128);
                }
                _ => {}
            }
            let global = if case == 5 {
                None
            } else {
                Some(rig.global.wal_preparation_client())
            };
            let result = WalPreparationInput::from_universal(
                &args,
                &participants,
                &plan,
                &claims,
                sources,
                global,
                Instant::now() + Duration::from_secs(2),
            );
            assert!(
                matches!(
                    result,
                    Err(WalPreparationError::InvalidInput | WalPreparationError::Full)
                ),
                "case {case}"
            );
        }
        let events = rig.actor.as_ref().unwrap().events.lock().unwrap();
        assert!(events.is_empty());
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
    }
    #[test]
    fn all_owner_claim_device_mismatch_refuses_after_real_global_and_source_capture() {
        let rig = Rig::new("all-owner-exact-device");
        let (args, participants, ids) = command(&rig.fixture, &rig.revisions);
        let (actual_plan, actual_claims, _, _) = plan(&rig.fixture);
        for global_wrong in [true, false] {
            let allocations = actual_plan
                .allocations()
                .iter()
                .map(|a| {
                    let wrong =
                        matches!(a.class(), Class::GlobalDatabaseCommit | Class::GlobalWalShm)
                            == global_wrong;
                    FullCapacityAllocation::new(
                        VolumeIdentity {
                            device: a.volume().device + u64::from(wrong),
                        },
                        a.allocation_unit(),
                        a.class(),
                        a.allocated_bytes(),
                        a.allocated_bytes(),
                        crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
                    )
                    .unwrap()
                })
                .collect();
            let claims = actual_claims
                .claims()
                .iter()
                .map(|c| {
                    let wrong = matches!(
                        c.allocation_class(),
                        Class::GlobalDatabaseCommit | Class::GlobalWalShm
                    ) == global_wrong;
                    AdmissionClaim::new(
                        c.claim_id(),
                        VolumeIdentity {
                            device: c.volume().device + u64::from(wrong),
                        },
                        c.allocation_class(),
                        c.allocated_bytes(),
                    )
                    .unwrap()
                })
                .collect();
            let plan = FullCapacityPlan::new(allocations).unwrap();
            let claims = AdmissionClaimInventory::new(claims).unwrap();
            let main = claims.claims()[0].claim_id();
            let side = claims.claims()[1].claim_id();
            let sources = (0..2)
                .map(|i| {
                    WalPreparationSource::new(
                        rig.requester.clone(),
                        ids[i],
                        main,
                        side,
                        SourceWalPreparationInput::Audited(rig.revisions[i].clone()),
                    )
                })
                .collect();
            let input = WalPreparationInput::from_universal(
                &args,
                &participants,
                &plan,
                &claims,
                sources,
                Some(rig.global.wal_preparation_client()),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            let handle = rig.submit(input);
            let result = wait(&handle, terminal);
            assert!(
                matches!(
                    result,
                    WalPreparationStatus::Rejected(WalPreparationError::InvalidInput)
                ),
                "{result:?}"
            );
        }
    }
    fn mismatched_unit_plan(plan: &FullCapacityPlan) -> FullCapacityPlan {
        FullCapacityPlan::new(
            plan.allocations()
                .iter()
                .map(|allocation| {
                    let unit = allocation.allocation_unit();
                    let wrong = if unit > 1 { unit / 2 } else { 2 };
                    FullCapacityAllocation::new(
                        allocation.volume().clone(),
                        wrong,
                        allocation.class(),
                        allocation.allocated_bytes(),
                        allocation.allocated_bytes(),
                        PROTECTED_FREE_SPACE_FLOOR,
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn all_owner_native_global_unit_mismatch_refuses_original_prefix_without_identity() {
        let rig = Rig::new("all-owner-global-unit");
        let (args, participants, ids) = command(&rig.fixture, &rig.revisions);
        let (plan, claims, main, side) = plan(&rig.fixture);
        let plan = mismatched_unit_plan(&plan);
        let sources = (0..2)
            .map(|i| {
                WalPreparationSource::new(
                    rig.requester.clone(),
                    ids[i],
                    main,
                    side,
                    SourceWalPreparationInput::Audited(rig.revisions[i].clone()),
                )
            })
            .collect();
        let input = WalPreparationInput::from_universal(
            &args,
            &participants,
            &plan,
            &claims,
            sources,
            Some(rig.global.wal_preparation_client()),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let budget = input.budget_for_test();
        let handle = rig.submit(input);
        let result = wait(&handle, |status| {
            matches!(status, WalPreparationStatus::Ready(_)) || terminal(status)
        });
        assert!(
            matches!(
                result,
                WalPreparationStatus::Rejected(WalPreparationError::InvalidInput)
            ),
            "{result:?}"
        );
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        drop(handle);
        drop(rig);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn all_owner_real_pre_audit_lease_input_stays_distinct_and_aborts_original_writer() {
        original_lease_unit_fixture(false);
    }
    #[test]
    fn all_owner_native_lease_unit_mismatch_refuses_actual_original_source_without_identity() {
        original_lease_unit_fixture(true);
    }
    fn original_lease_unit_fixture(wrong_unit: bool) {
        let (fixture, mut journal) = open_owner("all-owner-preaudit");
        let media = fixture._base.path().join("pre-audit");
        fs::create_dir(&media).unwrap();
        let writer = SourceWriter::new_for_scan(
            SourceDatabase::open_for_catalog_with_profile_guard(
                &media,
                &fixture.owner,
                "pre-audit",
            )
            .unwrap(),
            SourceId::from_string("pre-audit"),
            1,
        )
        .unwrap();
        assert!(writer.current_revision().is_err());
        let (requester, inbox) = SourceWalTransport::channel();
        let (send, receive) = mpsc::sync_channel(1);
        let actor = SourceActor::start_with_lease_observation(vec![writer], inbox, Some(send));
        let (allocation, fence, deadline, _) =
            receive.recv_timeout(Duration::from_secs(3)).unwrap();
        let participant = ParticipantId::new();
        let participants = ParticipantSet::new(
            vec![ParticipantDescriptor::source_lease_maintenance(participant, fence).unwrap()],
            vec![
                ParticipantCheckpoint::new(
                    participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let (plan, claims, main, side) = plan(&fixture);
        let plan = if wrong_unit {
            mismatched_unit_plan(&plan)
        } else {
            plan
        };

        let input = WalPreparationInput::from_lease_scope(
            &participants,
            &plan,
            &claims,
            WalPreparationSource::new(
                requester,
                participant,
                main,
                side,
                SourceWalPreparationInput::Lease(allocation),
            ),
            deadline,
        )
        .unwrap();
        let budget = input.budget_for_test();
        let handle = journal
            .client()
            .prepare_wal_owners(input)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        if wrong_unit {
            let result = wait(&handle, |status| {
                matches!(status, WalPreparationStatus::Ready(_)) || terminal(status)
            });
            assert!(
                matches!(
                    result,
                    WalPreparationStatus::Rejected(WalPreparationError::InvalidInput)
                ),
                "{result:?}"
            );
            assert_eq!(
                journal
                    .prepared_admission_id_allocations
                    .load(Ordering::Relaxed),
                0
            );
            drop(handle);
            drop(actor);
            assert!(journal.shutdown());
            assert_eq!(budget.used(), 0);
            return;
        }
        let WalPreparationStatus::Ready(view) = wait(&handle, |s| {
            matches!(s, WalPreparationStatus::Ready(_)) || terminal(s)
        }) else {
            panic!("not ready")
        };
        assert!(matches!(view.databases()[0], WalPreparedDatabase::Lease(_)));
        handle.abort();
        assert!(matches!(
            wait(&handle, terminal),
            WalPreparationStatus::Aborted
        ));
        drop(view);
        drop(handle);
        drop(actor);
        assert!(journal.shutdown());
    }

    #[test]
    fn all_owner_real_registry_retirement_collects_global_and_pre_audit_lease_with_exact_graph() {
        original_retirement_custody(0);
    }

    #[test]
    fn retirement_original_ack_fences_real_commit_cancel_deadline_and_shutdown() {
        original_retirement_custody(1);
    }

    #[test]
    fn retirement_original_ack_fences_confirmed_rollback_and_unconfirmed_commit_delivery() {
        original_retirement_custody(2);
        original_retirement_custody(3);
    }

    #[test]
    fn retirement_original_consumer_refuses_invented_aggregate_component_budgets() {
        original_retirement_custody(4);
        original_retirement_custody(5);
    }

    #[test]
    fn retirement_original_consumer_refuses_missing_source_release_policy() {
        original_retirement_custody_with_source_policy(0, Some(false));
    }

    #[test]
    fn retirement_original_consumer_refuses_another_valid_source_release_family() {
        original_retirement_custody_with_source_policy(0, Some(true));
    }

    #[test]
    fn original_source_drain_revokes_real_retirement_decision_acknowledgement() {
        original_retirement_custody(6);
    }

    #[test]
    fn original_source_drain_races_real_decision_comparison_without_stale_acceptance() {
        original_retirement_custody(7);
    }
    #[test]
    fn original_source_guard_drop_revokes_ack_without_unlocking_native_custody() {
        original_retirement_custody(8);
    }
    #[test]
    fn original_late_source_ack_cannot_fill_an_unlocked_snapshot_before_drain() {
        original_retirement_custody(9);
    }
    #[test]
    fn genuine_foreign_source_registration_cannot_fill_original_decision_slot() {
        original_retirement_custody(10);
    }
    #[test]
    fn genuine_original_source_gate_contention_is_pending_then_current_then_revoked() {
        original_retirement_custody(11);
    }
    #[test]
    fn source_gate_becoming_busy_between_completion_and_publication_preserves_preparing_round() {
        original_retirement_custody(12);
    }
    #[test]
    fn genuine_ready_retirement_command_preserves_busy_input_until_original_gate_releases() {
        original_retirement_custody(13);
    }
    #[test]
    fn genuine_ready_retirement_busy_command_cancellation_retains_original_cleanup() {
        original_retirement_custody(14);
    }
    #[test]
    fn genuine_ready_retirement_busy_command_original_deadline_refuses_without_sql() {
        original_retirement_custody(15);
    }
    #[test]
    fn genuine_ready_retirement_busy_command_original_drain_revokes_without_sql() {
        original_retirement_custody(16);
    }
    fn original_retirement_custody(outcome: u8) {
        original_retirement_custody_with_source_policy(outcome, None);
    }

    fn original_retirement_custody_with_source_policy(
        outcome: u8,
        source_policy_fault: Option<bool>,
    ) {
        use crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs;
        use crate::operation_journal::work_plan::{
            WORK_PLAN_VERSION_REGISTRY_RETIREMENT, WorkPlan, WorkPrerequisite, WorkUnitKey,
            WorkUnitRole, derive_work_plan_v1,
        };
        use crate::sample_sources::db::{
            SourceRegistryReferenceExpected, SourceRegistryReferenceProposal,
            SourceRegistryReferenceTransition as Transition,
        };
        let fixture = fixture("all-owner-registry-retirement");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        if outcome == 2 || outcome == 3 {
            use crate::operation_journal::store::{InsertTestBoundary, InsertTestDirective};
            store.set_insert_test_hook(move |boundary| {
                let selected = if outcome == 2 {
                    InsertTestBoundary::BeforePrecommitValidation
                } else {
                    InsertTestBoundary::Commit
                };
                if boundary != selected {
                    return InsertTestDirective::Continue;
                }
                let error = OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: "original custody outcome fixture".into(),
                };
                if outcome == 3 {
                    InsertTestDirective::ExecuteThenFail(error)
                } else {
                    InsertTestDirective::Fail(error)
                }
            });
        }
        let mut journal = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&journal);
        let mut global = GlobalLibraryWriter::start(&fixture.owner).unwrap();
        let startup = Instant::now() + Duration::from_secs(5);
        while global.status() != GlobalLibraryWriterStatus::Available {
            assert!(Instant::now() < startup);
            thread::sleep(Duration::from_millis(1));
        }
        let media = fixture._base.path().join("retiring-pre-audit");
        fs::create_dir(&media).unwrap();
        let writer = SourceWriter::new_for_scan(
            SourceDatabase::open_for_catalog_with_profile_guard(
                &media,
                &fixture.owner,
                "retiring-pre-audit",
            )
            .unwrap(),
            SourceId::from_string("retiring-pre-audit"),
            1,
        )
        .unwrap();
        assert!(writer.current_revision().is_err());
        let (requester, inbox) = SourceWalTransport::channel();
        let (send, receive) = mpsc::sync_channel(1);
        let actor = SourceActor::start_with_lease_observation(vec![writer], inbox, Some(send));
        let (allocation, fence, deadline, reference) =
            receive.recv_timeout(Duration::from_secs(3)).unwrap();
        let foreign_actor = if outcome == 10 {
            let foreign_media = fixture._base.path().join("foreign-registration");
            fs::create_dir(&foreign_media).unwrap();
            let foreign_writer = SourceWriter::new_for_scan(
                SourceDatabase::open_for_catalog_with_profile_guard(
                    &foreign_media,
                    &fixture.owner,
                    "foreign-registration",
                )
                .unwrap(),
                SourceId::from_string("foreign-registration"),
                1,
            )
            .unwrap();
            let (_foreign_requester, foreign_inbox) = SourceWalTransport::channel();
            Some(SourceActor::start(vec![foreign_writer], foreign_inbox))
        } else {
            None
        };
        let source = ParticipantId::new();
        let global_id = ParticipantId::new();
        let operation = OperationId::new();
        // Historical expected-row hashes only. No global transition, authenticated receipt,
        // qualified provider or release permission is installed by this preparation fixture.
        let draining = SourceRegistryReferenceProposal::new(
            &reference,
            operation,
            WorkUnitKey::new(0, 0, global_id, WorkUnitRole::GlobalLibrary),
            Transition::Draining,
            SourceRegistryReferenceExpected::Present {
                state: Transition::Enrolled,
                row_identity: [1; 32],
            },
        )
        .unwrap();
        let retired = SourceRegistryReferenceProposal::new(
            &reference,
            operation,
            WorkUnitKey::new(0, 2, global_id, WorkUnitRole::GlobalLibrary),
            Transition::Retired,
            SourceRegistryReferenceExpected::Present {
                state: Transition::Draining,
                row_identity: [2; 32],
            },
        )
        .unwrap();
        let retirement = SourceRegistryRetirementIntentArgs::new(
            source,
            global_id,
            fence.clone(),
            &draining,
            &retired,
        )
        .unwrap();
        let descriptors = vec![
            retirement.source_descriptor().unwrap(),
            ParticipantDescriptor::new(
                global_id,
                ParticipantKind::GlobalLibrary,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .unwrap(),
        ];
        let checkpoints = descriptors
            .iter()
            .map(|descriptor| {
                ParticipantCheckpoint::new(
                    descriptor.id(),
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .unwrap()
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
        let args = UniversalCommandArgs::SourceRegistryRetirement(retirement.clone());
        let work = derive_work_plan(&args, &participants).unwrap();
        assert_eq!(work.plan_version(), WORK_PLAN_VERSION_REGISTRY_RETIREMENT);
        assert_eq!(
            work.units()
                .iter()
                .map(|unit| unit.key())
                .collect::<Vec<_>>(),
            vec![
                retirement.draining_work(),
                retirement.source_work(),
                retirement.retired_work()
            ]
        );
        assert_eq!(
            work.prerequisites(),
            &[
                WorkPrerequisite::new(retirement.draining_work(), retirement.source_work()),
                WorkPrerequisite::new(retirement.source_work(), retirement.retired_work())
            ]
        );
        work.validate_against(&args, &participants).unwrap();
        assert!(derive_work_plan_v1(&args, &participants).is_err());
        let mut image = serde_json::to_value(&work).unwrap();
        image["prerequisites"] = serde_json::json!([]);
        assert!(
            serde_json::from_value::<WorkPlan>(image)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut image = serde_json::to_value(&work).unwrap();
        image["plan_version"] = serde_json::json!(1);
        assert!(
            serde_json::from_value::<WorkPlan>(image)
                .unwrap()
                .validate()
                .is_err()
        );
        let (mut base_plan, mut base_claims, main, side) = plan(&fixture);
        if outcome == 0 || outcome == 5 {
            // An actual original worker supplies planning ceilings before freezing the
            // envelope. Its later retirement scope must independently match all of them.
            // This observation does not qualify physical capacity or issue an ACK.
            let request = crate::sample_sources::library::GlobalPreparationRequest::new(
                global.preparation_owner(),
                [9; 32],
                deadline,
                1,
                16 * 1024,
            )
            .unwrap()
            .with_lifecycle_policy();
            let mut pending = global.submit_preparation(request).unwrap();
            let original_scope = pending
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let global_allocation = original_scope.observation().lifecycle_allocation().unwrap();
            let source_caps = fence.write_ceilings();
            let global_caps = [
                global_allocation.main(),
                global_allocation.wal(),
                global_allocation.shm(),
            ]
            .map(|object| object.write_ceiling());
            let mut exact = Vec::new();
            for allocation in base_plan.allocations() {
                let bytes = match allocation.class() {
                    Class::SourceDatabaseCommit => source_caps[0],
                    Class::SourceWalShm => source_caps[1].checked_add(source_caps[2]).unwrap(),
                    Class::GlobalDatabaseCommit => global_caps[0].checked_mul(2).unwrap(),
                    Class::GlobalWalShm => global_caps[1]
                        .checked_add(global_caps[2])
                        .unwrap()
                        .checked_mul(2)
                        .unwrap(),
                    _ => unreachable!(),
                };
                let bytes = if outcome == 5 && allocation.class() == Class::SourceDatabaseCommit {
                    bytes.checked_add(allocation.allocation_unit()).unwrap()
                } else {
                    bytes
                };
                exact.push(
                    FullCapacityAllocation::new(
                        allocation.volume().clone(),
                        allocation.allocation_unit(),
                        allocation.class(),
                        bytes,
                        bytes,
                        PROTECTED_FREE_SPACE_FLOOR,
                    )
                    .unwrap(),
                );
            }
            base_plan = FullCapacityPlan::new(exact).unwrap();
            base_claims = AdmissionClaimInventory::new(
                base_claims
                    .claims()
                    .iter()
                    .map(|claim| {
                        let allocation = base_plan
                            .allocations()
                            .iter()
                            .find(|a| a.class() == claim.allocation_class())
                            .unwrap();
                        AdmissionClaim::new(
                            claim.claim_id(),
                            allocation.volume().clone(),
                            allocation.class(),
                            allocation.allocated_bytes(),
                        )
                        .unwrap()
                    })
                    .collect(),
            )
            .unwrap();
            original_scope
                .abort()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            drop(pending);
        }
        let original = &base_plan.allocations()[0];
        let mut allocations = base_plan.allocations().to_vec();
        allocations.push(
            FullCapacityAllocation::new(
                original.volume().clone(),
                original.allocation_unit(),
                Class::JournalRecord,
                8192,
                8192,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap(),
        );
        let plan = FullCapacityPlan::new(allocations).unwrap();
        let mut correlations = base_claims.claims().to_vec();
        correlations.push(
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                original.volume().clone(),
                Class::JournalRecord,
                8192,
            )
            .unwrap(),
        );
        let claims = AdmissionClaimInventory::new(correlations).unwrap();
        let source_input = || {
            WalPreparationSource::new(
                requester.clone(),
                source,
                main,
                side,
                SourceWalPreparationInput::Lease(allocation.clone()),
            )
        };
        for fault in 0..5 {
            let mut image = serde_json::to_value(&fence).unwrap();
            match fault {
                0 | 1 => {
                    let object = &mut image["objects"][fault + 1];
                    object["identity"] = serde_json::json!(format!(
                        "{}-foreign",
                        object["identity"].as_str().unwrap()
                    ));
                }
                2 => {
                    for object in image["objects"].as_array_mut().unwrap() {
                        object["device"] =
                            serde_json::json!(object["device"].as_u64().unwrap() + 1);
                    }
                }
                3 => {
                    image["maximum_pages"] =
                        serde_json::json!(image["maximum_pages"].as_u64().unwrap() / 2)
                }
                _ => {
                    image["objects"][0]["bytes"] =
                        serde_json::json!(image["objects"][0]["bytes"].as_u64().unwrap() + 1)
                }
            }
            let page = image["page_size"].as_u64().unwrap();
            let pages = image["maximum_pages"].as_u64().unwrap();
            let windows = [page * pages, (page + 24) * pages + 32, pages * 16 + 32768];
            for (object, window) in image["objects"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .zip(windows)
            {
                object["ceiling"] = serde_json::json!(
                    crate::lifecycle_allocation::logical_ceiling(
                        object["bytes"].as_u64().unwrap(),
                        window,
                        object["unit"].as_u64().unwrap(),
                    )
                    .unwrap()
                );
            }
            let foreign: crate::operation_journal::source_lease_binding::StoredSourceLeaseFence =
                serde_json::from_value(image).unwrap();
            foreign.validate().unwrap(); // Valid history, never a live allocation or grant.
            let other = SourceRegistryRetirementIntentArgs::new(
                source, global_id, foreign, &draining, &retired,
            )
            .unwrap();
            let descriptors = vec![
                other.source_descriptor().unwrap(),
                participants.descriptors()[1].clone(),
            ];
            let other_participants =
                ParticipantSet::new(descriptors, participants.checkpoints().to_vec()).unwrap();
            assert!(
                matches!(
                    WalPreparationInput::from_universal(
                        &UniversalCommandArgs::SourceRegistryRetirement(other),
                        &other_participants,
                        &plan,
                        &claims,
                        vec![source_input()],
                        Some(global.wal_preparation_client()),
                        deadline,
                    ),
                    Err(WalPreparationError::InvalidInput)
                ),
                "foreign frozen allocation fault={fault}"
            );
            let lease_participants = ParticipantSet::new(
                vec![other_participants.descriptors()[0].clone()],
                vec![
                    ParticipantCheckpoint::new(
                        source,
                        ParticipantCheckpointState::Pending,
                        None,
                        None,
                    )
                    .unwrap(),
                ],
            )
            .unwrap();
            assert!(
                matches!(
                    WalPreparationInput::from_lease_scope(
                        &lease_participants,
                        &plan,
                        &claims,
                        source_input(),
                        deadline,
                    ),
                    Err(WalPreparationError::InvalidInput)
                ),
                "isolated foreign allocation fault={fault}"
            );
        }
        assert!(matches!(
            WalPreparationInput::from_universal(
                &args,
                &participants,
                &plan,
                &claims,
                vec![source_input()],
                None,
                deadline
            ),
            Err(WalPreparationError::InvalidInput)
        ));
        assert!(matches!(
            WalPreparationInput::from_universal(
                &args,
                &participants,
                &plan,
                &claims,
                Vec::new(),
                Some(global.wal_preparation_client()),
                deadline
            ),
            Err(WalPreparationError::InvalidInput)
        ));
        for sources in [
            vec![source_input(), source_input()],
            vec![WalPreparationSource::new(
                requester.clone(),
                ParticipantId::new(),
                main,
                side,
                SourceWalPreparationInput::Lease(allocation.clone()),
            )],
            vec![WalPreparationSource::new(
                requester.clone(),
                source,
                AdmissionClaimId::new(),
                side,
                SourceWalPreparationInput::Lease(allocation.clone()),
            )],
        ] {
            assert!(matches!(
                WalPreparationInput::from_universal(
                    &args,
                    &participants,
                    &plan,
                    &claims,
                    sources,
                    Some(global.wal_preparation_client()),
                    deadline,
                ),
                Err(WalPreparationError::InvalidInput)
            ));
        }
        let mut input = WalPreparationInput::from_universal(
            &args,
            &participants,
            &plan,
            &claims,
            vec![source_input()],
            Some(global.wal_preparation_client()),
            deadline,
        )
        .unwrap();
        if let Some(foreign_family) = source_policy_fault {
            if foreign_family {
                // A different complete valid frozen family is transported by the actual
                // Source producer; it supplies no authority for the original family.
                let other_operation = OperationId::new();
                let other_draining = SourceRegistryReferenceProposal::new(
                    &reference,
                    other_operation,
                    retirement.draining_work(),
                    Transition::Draining,
                    SourceRegistryReferenceExpected::Present {
                        state: Transition::Enrolled,
                        row_identity: [1; 32],
                    },
                )
                .unwrap();
                let other_retired = SourceRegistryReferenceProposal::new(
                    &reference,
                    other_operation,
                    retirement.retired_work(),
                    Transition::Retired,
                    SourceRegistryReferenceExpected::Present {
                        state: Transition::Draining,
                        row_identity: [2; 32],
                    },
                )
                .unwrap();
                let other = SourceRegistryRetirementIntentArgs::new(
                    source,
                    global_id,
                    fence.clone(),
                    &other_draining,
                    &other_retired,
                )
                .unwrap();
                other.validate().unwrap();
                input.replace_source_retirement_policy_for_test(Some(&other));
            } else {
                input.replace_source_retirement_policy_for_test(None);
            }
        }
        let digest = crate::operation_journal::wal_admission::prepared_digest(
            &args,
            &participants,
            &plan,
            &claims,
        )
        .unwrap();
        let budget = input.budget_for_test();
        let mut local_collector = None;
        let handle = if outcome == 0 || outcome == 4 || outcome == 5 || outcome >= 13 {
            journal
                .client()
                .prepare_wal_owners(input)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
        } else {
            let permit = journal
                .shared
                .reserve_input(
                    InputClass::Ordinary,
                    crate::operation_journal::wal_admission::HANDSHAKE_MAX_BYTES,
                )
                .unwrap();
            let acquisition = Uuid::parse_str(fixture.owner.owner_identity()).unwrap();
            let (mut collector, handle) =
                crate::operation_journal::wal_admission::Collector::new(input, permit, acquisition);
            if outcome == 12 {
                let round = loop {
                    if let Some(round) = collector.preparing_round_for_tests() {
                        break round;
                    }
                    assert!(Instant::now() < deadline);
                    collector.poll();
                    thread::sleep(Duration::from_millis(1));
                };
                while !round.all_ack_slots_for_tests() {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                let token = actor
                    .tokens
                    .lock()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
                let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
                let holder = Arc::new(Mutex::new(None));
                let published_holder = Arc::clone(&holder);
                let publication = Arc::clone(&round);
                let completion = Arc::clone(&round);
                // Poll first observes revocation, then completion, then publication.
                round.snapshot_hook_for_tests(Box::new(move || {
                    completion.snapshot_hook_for_tests(Box::new(move || {
                        publication.snapshot_hook_for_tests(Box::new(move || {
                            *published_holder.lock().unwrap() = Some(thread::spawn(move || {
                                assert!(
                                    token
                                        .with_current(|| {
                                            entered_tx.send(()).unwrap();
                                            release_rx
                                                .recv_timeout(Duration::from_secs(1))
                                                .unwrap();
                                        })
                                        .is_some()
                                );
                            }));
                            entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                        }));
                    }));
                }));
                assert!(!collector.poll());
                assert!(matches!(handle.status(), WalPreparationStatus::Preparing));
                assert!(!round.invalidated_for_tests());
                release_tx.send(()).unwrap();
                holder.lock().unwrap().take().unwrap().join().unwrap();
            }
            while matches!(handle.status(), WalPreparationStatus::Preparing) {
                assert!(Instant::now() < deadline);
                collector.poll();
                thread::sleep(Duration::from_millis(1));
            }
            local_collector = Some(collector);
            handle
        };
        let WalPreparationStatus::Ready(view) = wait(&handle, |status| {
            matches!(status, WalPreparationStatus::Ready(_)) || terminal(status)
        }) else {
            panic!("{:?}", handle.status());
        };
        assert_eq!(view.databases().len(), 2);
        assert!(matches!(
            view.databases()[0],
            WalPreparedDatabase::Global(_)
        ));
        assert!(matches!(view.databases()[1], WalPreparedDatabase::Lease(_)));
        let WalPreparedDatabase::Global(original) = &view.databases()[0] else {
            unreachable!();
        };
        let descriptor: serde_json::Value =
            serde_json::from_slice(original.wal_scope().unwrap().descriptor_bytes()).unwrap();
        assert!(
            descriptor["retirement_forward_policy"].is_object(),
            "the actual retirement producer must seal its typed initial-forward policy"
        );
        let policy = original
            .wal_scope()
            .unwrap()
            .retirement_forward_policy()
            .unwrap();
        assert_eq!(policy.operation_id(), operation);
        assert_eq!(
            policy.work_keys(),
            &[retirement.draining_work(), retirement.retired_work()]
        );
        let allocation = original.lifecycle_allocation().unwrap();
        let expected = [
            allocation.main().write_ceiling(),
            allocation.wal().write_ceiling(),
            allocation.shm().write_ceiling(),
        ];
        assert_eq!(policy.per_transition_ceilings(), expected);
        assert_eq!(
            policy.total_forward_ceilings(),
            expected.map(|n| n.checked_mul(2).unwrap())
        );
        let sealed = &descriptor["retirement_forward_policy"];
        assert_eq!(
            sealed["registration"],
            serde_json::to_value(original.owner().registration()).unwrap()
        );
        assert_eq!(
            sealed["registration_generation"],
            original.owner().generation()
        );
        assert_eq!(
            sealed["preparation_nonce"],
            serde_json::to_value(original.nonce()).unwrap()
        );
        let expected_digest = blake3::derive_key(
            "yield.global-retirement-initial-forward.v1",
            &serde_json::to_vec(&retirement).unwrap(),
        );
        assert_eq!(policy.family_digest(), &expected_digest);

        let WalPreparedDatabase::Lease(original_source) = &view.databases()[1] else {
            unreachable!();
        };
        let retained_credit = budget.used();
        assert!(retained_credit > 0);
        let historical_alias = original_source.clone();
        assert_eq!(budget.used(), retained_credit);
        drop(historical_alias);
        assert_eq!(budget.used(), retained_credit);
        let source_policy = original_source
            .original_binding()
            .retirement_forward_policy();
        if source_policy_fault == Some(false) {
            assert!(source_policy.is_none());
        } else {
            let source_policy = source_policy
                .expect("the actual Source producer must retain the frozen lease RELEASE policy");
            assert_eq!(
                source_policy.matches_original(
                    &retirement,
                    policy.family_digest(),
                    original_source.original_binding().stored_fence(),
                    original_source.scope_descriptor_bytes(),
                ),
                source_policy_fault.is_none(),
            );
        }

        if let Some(collector) = local_collector.as_mut() {
            if outcome == 9 || outcome == 10 {
                actor.pause_validation.store(true, Ordering::Release);
                while !actor.validation_paused_seen.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
            }
            collector.begin_decision(digest).unwrap();
            let nonce = collector.decision_nonce().unwrap();
            let mut foreign_custody = None;
            if outcome == 10 {
                let foreign = foreign_actor.as_ref().unwrap();
                let token = loop {
                    if let Some(token) = foreign
                        .tokens
                        .lock()
                        .unwrap()
                        .get("foreign-registration")
                        .cloned()
                    {
                        break token;
                    }
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                };
                let round = collector.round_for_tests();
                foreign_custody = Some(round.foreign_ack_for_tests(token));
                assert!(!round.all_ack_slots_for_tests());
                assert!(!collector.decision_acknowledged());
                actor.pause_validation.store(false, Ordering::Release);
            }
            if outcome == 9 {
                let round = collector.round_for_tests();
                assert!(!round.all_ack_slots_for_tests());
                let original = Arc::clone(&round);
                let pause = Arc::clone(&actor.pause_validation);
                let drain = Arc::clone(&actor.drain_requested);
                let drained = Arc::clone(&actor.drained);
                round.snapshot_hook_for_tests(Box::new(move || {
                    // Snapshot contains no Source gate. Install the real original ACK,
                    // then complete real drain before the comparison can lock the round.
                    pause.store(false, Ordering::Release);
                    while !original.all_ack_slots_for_tests() {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    drain.store(true, Ordering::Release);
                    while !drained.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                }));
                assert!(
                    !collector.decision_acknowledged(),
                    "late original token must invalidate the unlocked cohort"
                );
            }
            while outcome != 9 && !collector.decision_acknowledged() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(collector.decision_nonce(), Some(nonce));
            if outcome == 11 {
                let token = actor
                    .tokens
                    .lock()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
                let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
                let holder = thread::spawn(move || {
                    assert!(
                        token
                            .with_current(|| {
                                entered_tx.send(()).unwrap();
                                release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                            })
                            .is_some()
                    );
                });
                entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                let started = Instant::now();
                assert!(!collector.decision_acknowledged());
                assert!(!collector.round_for_tests().invalidated_for_tests());
                assert!(!collector.start_settlement());
                assert!(started.elapsed() < Duration::from_millis(100));
                release_tx.send(()).unwrap();
                holder.join().unwrap();
                assert!(collector.decision_acknowledged());
            }
            if outcome == 10 {
                assert!(
                    collector.decision_acknowledged(),
                    "foreign refusal must preserve the genuine original round"
                );
                collector.refuse_decision(WalPreparationError::Canceled);
                drop(foreign_custody.take());
                while !collector.poll() {
                    assert!(Instant::now() < deadline + Duration::from_secs(3));
                    thread::sleep(Duration::from_millis(1));
                }
                drop(local_collector.take());
                drop(view);
                drop(handle);
                assert_eq!(budget.used(), 0);
                drop(foreign_actor);
                drop(actor);
                assert!(global.shutdown());
                assert!(journal.shutdown());
                return;
            }
            if outcome >= 6 {
                let completed = if outcome == 9 {
                    &actor.drained
                } else if outcome == 8 {
                    actor.abandon_requested.store(true, Ordering::Release);
                    &actor.abandoned
                } else {
                    actor.drain_requested.store(true, Ordering::Release);
                    &actor.drained
                };
                while !completed.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline);
                    if outcome == 7 {
                        // Either ordering may win before revocation completes; no stale
                        // success is legal once the actual gate transition is observed.
                        let _ = collector.decision_acknowledged();
                        thread::yield_now();
                    } else {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                assert!(
                    !collector.decision_acknowledged(),
                    "a genuine Draining original source cannot remain a current Registered acknowledgement"
                );
                assert!(!collector.start_settlement());
                collector.refuse_decision(WalPreparationError::OwnerLost);
                while !collector.poll() {
                    assert!(Instant::now() < deadline + Duration::from_secs(3));
                    thread::sleep(Duration::from_millis(1));
                }
                drop(local_collector.take());
                drop(view);
                drop(handle);
                assert_eq!(budget.used(), 0);
                drop(actor);
                assert!(global.shutdown());
                assert!(journal.shutdown());
                return;
            }
            assert!(collector.start_settlement());
            // An actual journal transaction exercises the owner fence, not admission:
            // this V1 storage fixture has no retirement inventory, qualified provider or permit.
            let record = intent_record(OperationId::new());
            let observed = journal
                .client()
                .persist_intent(record.clone())
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            if outcome == 1 {
                assert!(observed.receipt().is_some());
            } else {
                let failure = observed.failure().unwrap();
                assert_eq!(
                    failure.stage(),
                    if outcome == 2 {
                        JournalInsertWriteStage::RolledBackBeforeCommit
                    } else {
                        JournalInsertWriteStage::CommitOutcomeUnknown
                    }
                );
            }
            handle.abort();
            actor.stop.store(true, Ordering::Release);
            // Shutdown requests must not tear down the original Global SQLite writer.
            let closing = thread::spawn(move || global.shutdown());
            while Instant::now() <= deadline + Duration::from_millis(20) {
                collector.poll();
                thread::sleep(Duration::from_millis(1));
            }
            assert!(!closing.is_finished());
            assert!(!actor.worker.as_ref().unwrap().is_finished());
            assert!(
                !actor
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event.starts_with("reuse:"))
            );
            let outside =
                rusqlite::Connection::open(fixture.owner.profile_root().join(LIBRARY_DB_FILE_NAME))
                    .unwrap();
            outside.busy_timeout(Duration::ZERO).unwrap();
            assert_eq!(
                outside
                    .execute_batch("BEGIN IMMEDIATE")
                    .unwrap_err()
                    .sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            let stored = journal
                .client()
                .get(record.operation_id)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            if outcome == 2 {
                assert!(stored.is_none());
            } else {
                assert_eq!(encode(&stored.unwrap()).unwrap(), encode(&record).unwrap());
            }
            // Resolve only the genuine completed transaction or its exact durable image.
            // Mere elapsed time, dropped response or owner shutdown is insufficient.
            collector.resolve_settlement();
            while !collector.poll() {
                thread::sleep(Duration::from_millis(1));
            }
            drop(local_collector.take());
            drop(view);
            drop(handle);
            assert_eq!(budget.used(), 0);
            drop(actor);
            assert!(closing.join().unwrap());
            assert!(journal.shutdown());
            return;
        }
        let result = journal
            .client()
            .admit_full(args.clone(), participants.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            result,
            Err(OperationJournalOwnerError::PreparedAdmission(
                PreparedAdmissionError::Unsupported { .. }
            ))
        ));
        let intent = OperationIntent {
            actor: crate::operation_journal::model::OperationActor::User,
            kind: crate::operation_journal::model::OperationKind::SourceRegistryRetirement,
            label: "Original owner retirement acknowledgement".into(),
        };
        let universal = UniversalIntent::new(
            None,
            crate::operation_journal::model::UniversalCommandKind::SourceRegistryRetirement,
            "operation.status.source_registry_retirement",
            participants.clone(),
        )
        .unwrap();
        let prepared = PreparedAdmissionInput::try_new(
            intent.clone(),
            universal.clone(),
            args.clone(),
            participants.clone(),
            plan.clone(),
            claims.clone(),
            1,
        )
        .unwrap();
        // A valid envelope with a different ordinary-claim identity cannot consume
        // the existing original preparation, even though its plan totals match.
        let mut foreign_claims = claims.claims().to_vec();
        let old = &foreign_claims[0];
        foreign_claims[0] = AdmissionClaim::new(
            AdmissionClaimId::new(),
            old.volume().clone(),
            old.allocation_class(),
            old.allocated_bytes(),
        )
        .unwrap();
        let foreign = PreparedAdmissionInput::try_new(
            intent,
            universal,
            args,
            participants,
            plan,
            AdmissionClaimInventory::new(foreign_claims).unwrap(),
            1,
        )
        .unwrap();
        assert!(matches!(
            journal
                .client()
                .admit_prepared(foreign)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap(),
            Err(OperationJournalOwnerError::WalPreparation(
                WalPreparationError::InvalidInput
            ))
        ));
        let busy = if outcome >= 13 {
            let token = actor
                .tokens
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .clone();
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let holder = thread::spawn(move || {
                assert!(
                    token
                        .with_current(|| {
                            entered_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(4)).unwrap();
                        })
                        .is_some()
                );
            });
            entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            Some((release_tx, holder))
        } else {
            None
        };
        let mut admitted = journal.client().admit_prepared(prepared).unwrap();
        if outcome >= 13 {
            assert!(
                matches!(
                    admitted.recv_timeout(Duration::from_millis(50)),
                    Err(OperationJournalRequestError::Empty)
                ),
                "genuine Ready command must retain original input while its gate is busy"
            );
            if outcome == 14 {
                handle.abort();
            } else if outcome == 15 {
                while Instant::now() < deadline + Duration::from_millis(20) {
                    thread::sleep(Duration::from_millis(1));
                }
            } else if outcome == 16 {
                actor.drain_requested.store(true, Ordering::Release);
            }
            let (release, holder) = busy.unwrap();
            release.send(()).unwrap();
            holder.join().unwrap();
            if outcome == 16 {
                while !actor.drained.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline + Duration::from_secs(1));
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let result = admitted.recv_timeout(Duration::from_secs(3)).unwrap();
        if outcome >= 14 {
            assert!(
                result.is_err(),
                "revoked original command cannot succeed: {result:?}"
            );
        } else if outcome == 4 || outcome == 5 || source_policy_fault.is_some() {
            assert!(
                matches!(
                    result,
                    Err(OperationJournalOwnerError::WalPreparation(
                        WalPreparationError::InvalidInput
                    ))
                ),
                "foreign or missing original policy must fail before acknowledgement: {result:?}"
            );
        } else {
            assert!(
                matches!(&result, Err(OperationJournalOwnerError::PreparedAdmission(PreparedAdmissionError::Unsupported { reason })) if reason.contains("original owners acknowledged")),
                "original retirement acknowledgement result: {result:?}"
            );
        }
        assert!(
            journal
                .client()
                .get(operation)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert_eq!(
            journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        let outside =
            rusqlite::Connection::open(fixture.owner.profile_root().join(LIBRARY_DB_FILE_NAME))
                .unwrap();
        assert_eq!(
            outside
                .query_row(
                    "SELECT count(*) FROM source_lifecycle_references",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
        handle.abort();
        let aborted = wait(&handle, terminal);
        assert!(
            if outcome == 4 || outcome == 5 || source_policy_fault.is_some() {
                matches!(
                    aborted,
                    WalPreparationStatus::Rejected(WalPreparationError::InvalidInput)
                )
            } else if outcome >= 14 {
                matches!(
                    aborted,
                    WalPreparationStatus::Aborted | WalPreparationStatus::Rejected(_)
                )
            } else {
                matches!(aborted, WalPreparationStatus::Aborted)
            },
            "{aborted:?}"
        );
        drop(view);
        drop(handle);
        assert_eq!(budget.used(), 0);
        drop(actor);
        assert!(global.shutdown());
        assert!(journal.shutdown());
    }
    #[test]
    fn all_owner_real_tag_and_layout_factories_keep_actual_original_family_policy() {
        use crate::sample_sources::db::write::{
            SOURCE_LAYOUT_ALGORITHM_V1, SourceLayoutComputedView,
        };
        use crate::sample_sources::wal_transport::SourceWalPolicyFamily;
        use crate::sample_sources::{SourceCommandId, SourceTagAssignmentMutation};
        for (layout, wrong_unit) in [(false, false), (true, false), (false, true), (true, true)] {
            let (fixture, mut journal) = open_owner(if layout {
                "all-owner-layout"
            } else {
                "all-owner-tag"
            });
            let writer = if layout {
                feature_source(&fixture, "family")
            } else {
                source(&fixture, "family")
            };
            let revision = writer.current_revision().unwrap();
            let participant = ParticipantId::new();
            let (plan, claims, main, side) = plan(&fixture);
            let plan = if wrong_unit {
                mismatched_unit_plan(&plan)
            } else {
                plan
            };
            let (requester, inbox) = SourceWalTransport::channel();
            let input = if layout {
                let cancel = AtomicBool::new(false);
                let deadline = Instant::now() + Duration::from_secs(2);
                let capture = writer
                    .capture_layout_features(&revision, &cancel, deadline)
                    .unwrap();
                assert_eq!(capture.features().len(), 1);
                let mut hash = blake3::Hasher::new();
                hash.update(b"yield.starmap.cpu-input.v1\0");
                hash.update(&(SOURCE_LAYOUT_ALGORITHM_V1.len() as u64).to_le_bytes());
                hash.update(SOURCE_LAYOUT_ALGORITHM_V1.as_bytes());
                hash.update(&(capture.features().len() as u64).to_le_bytes());
                for row in capture.features() {
                    hash.update(&(row.sample_identity().len() as u64).to_le_bytes());
                    hash.update(row.sample_identity().as_bytes());
                    for bytes in row.payload().bytes().chunks_exact(4) {
                        hash.update(bytes);
                    }
                }
                let points = [
                    crate::sample_sources::db::write::SourceLayoutComputedPoint {
                        sample_identity: capture.features()[0].sample_identity(),
                        coordinates: [0.25, -0.0],
                        cluster_id: None,
                    },
                ];
                let digest = *hash.finalize().as_bytes();
                let prepared = writer
                    .prepare_layout_generation(
                        &capture,
                        &SourceLayoutComputedView {
                            algorithm_version: SOURCE_LAYOUT_ALGORITHM_V1,
                            feature_input_digest: &digest,
                            points: &points,
                        },
                        &cancel,
                        deadline,
                    )
                    .unwrap();
                let budget = writer.capture_layout_metadata_budget(&revision).unwrap();
                let args =
                crate::operation_journal::source_layout_commands::SourceLayoutIntentArgs::from_prepared(
                    &writer,
                    &prepared,
                    &budget,
                    participant,
                    SourceCommandId::new(),
                    &cancel,
                )
                .unwrap();
                let participants = args.participant_observation().unwrap();
                WalPreparationInput::from_universal(
                    &UniversalCommandArgs::SourceDerivedLayout(args),
                    &participants,
                    &plan,
                    &claims,
                    vec![WalPreparationSource::new(
                        requester,
                        participant,
                        main,
                        side,
                        SourceWalPreparationInput::Layout(budget),
                    )],
                    None,
                    deadline,
                )
                .unwrap()
            } else {
                let outside = rusqlite::Connection::open(
                    fixture
                        .owner
                        .profile_root()
                        .join("family")
                        .join(crate::sample_sources::DB_FILE_NAME),
                )
                .unwrap();
                outside.execute_batch("INSERT INTO source_tags(normalized_text,display_label) VALUES('old tag','Old Tag'); INSERT INTO wav_file_tags(path,tag_id) SELECT 'one.wav',id FROM source_tags WHERE normalized_text='old tag';").unwrap();
                let observation = writer
                    .capture_tag_admission_observation(
                        &revision,
                        "old tag",
                        SourceTagAssignmentMutation::Delete,
                    )
                    .unwrap();
                let args=crate::operation_journal::metadata_tag_commands::MetadataTagSourceIntentArgs::from_observation(&writer,&observation,participant,SourceCommandId::new()).unwrap();
                WalPreparationInput::from_tag(
                    &writer,
                    &args,
                    &plan,
                    &claims,
                    WalPreparationSource::new(
                        requester,
                        participant,
                        main,
                        side,
                        SourceWalPreparationInput::Tag(observation),
                    ),
                    Instant::now() + Duration::from_secs(2),
                )
                .unwrap()
            };
            let actor = SourceActor::start(vec![writer], inbox);
            let handle = journal
                .client()
                .prepare_wal_owners(input)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            if wrong_unit {
                let result = wait(&handle, |status| {
                    matches!(status, WalPreparationStatus::Ready(_)) || terminal(status)
                });
                assert!(
                    matches!(
                        result,
                        WalPreparationStatus::Rejected(WalPreparationError::InvalidInput)
                    ),
                    "layout={layout} {result:?}"
                );
                assert_eq!(
                    journal
                        .prepared_admission_id_allocations
                        .load(Ordering::Relaxed),
                    0
                );
                drop(handle);
                drop(actor);
                assert!(journal.shutdown());
                continue;
            }
            let WalPreparationStatus::Ready(view) = wait(&handle, |s| {
                matches!(s, WalPreparationStatus::Ready(_)) || terminal(s)
            }) else {
                panic!("family not ready: {:?}", handle.status())
            };
            let WalPreparedDatabase::Source(observation) = &view.databases()[0] else {
                panic!("wrong original domain")
            };
            assert_eq!(
                observation.policy_family(),
                Some(if layout {
                    SourceWalPolicyFamily::Layout
                } else {
                    SourceWalPolicyFamily::Tag
                })
            );
            handle.abort();
            assert!(matches!(
                wait(&handle, terminal),
                WalPreparationStatus::Aborted
            ));
            let outside = rusqlite::Connection::open(
                fixture
                    .owner
                    .profile_root()
                    .join("family")
                    .join(crate::sample_sources::DB_FILE_NAME),
            )
            .unwrap();
            if layout {
                assert_eq!(
                outside
                    .query_row(
                        "SELECT count(*) FROM metadata WHERE key='source_starmap_generation_v1'",
                        [],
                        |r| r.get::<_, u64>(0)
                    )
                    .unwrap(),
                0
            );
            } else {
                assert_eq!(
                    outside
                        .query_row("SELECT count(*) FROM wav_file_tags", [], |r| r
                            .get::<_, u64>(0))
                        .unwrap(),
                    1
                );
            }
            drop(view);
            drop(handle);
            drop(actor);
            assert!(journal.shutdown());
        }
    }
    #[test]
    fn all_owner_journal_shutdown_drains_real_custody_before_original_resources() {
        let mut rig = Rig::new("all-owner-shutdown");
        let handle = rig.submit(rig.input(Instant::now() + Duration::from_secs(2)));
        let WalPreparationStatus::Ready(view) = wait(&handle, |s| {
            matches!(s, WalPreparationStatus::Ready(_)) || terminal(s)
        }) else {
            panic!("not ready")
        };
        let client = rig.journal.client();
        assert!(rig.journal.shutdown());
        assert!(matches!(handle.status(), WalPreparationStatus::Aborted));
        for i in 0..2 {
            rig.outside_source(i)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
        assert_eq!(
            ORDINARY_INPUT_BYTES - client.shared.input_bytes.lock().unwrap().remaining[0],
            64 * 1024
        );
        drop(view);
        drop(handle);
        assert_eq!(
            client.shared.input_bytes.lock().unwrap().remaining[0],
            ORDINARY_INPUT_BYTES
        );
    }
    #[test]
    fn all_owner_original_global_loss_never_becomes_reusable_or_clean_handoff() {
        let mut rig = Rig::new("all-owner-global-loss");
        let handle = rig.submit(rig.input(Instant::now() + Duration::from_secs(2)));
        assert!(matches!(
            wait(&handle, |s| matches!(s, WalPreparationStatus::Ready(_))
                || terminal(s)),
            WalPreparationStatus::Ready(_)
        ));
        assert!(rig.global.shutdown());
        let result = wait(&handle, terminal);
        assert!(
            matches!(
                result,
                WalPreparationStatus::Unavailable(WalPreparationError::OwnerLost)
            ),
            "{result:?}"
        );
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        assert!(!rig.journal.shutdown());
    }

    #[test]
    fn all_owner_actual_hard_wal_on_last_source_unwinds_real_prefix() {
        let rig = Rig::new("all-owner-hard-prefix");
        let index = if rig.revisions[0].database_identity() < rig.revisions[1].database_identity() {
            1
        } else {
            0
        };
        let path = rig
            .fixture
            .owner
            .profile_root()
            .join(rig.revisions[index].source_id().as_str())
            .join(format!("{}-wal", crate::sample_sources::DB_FILE_NAME));
        // Negative sparse-file fixture: the original VFS reads actual file size.
        // No injected positive native observation or scalar watermark grant.
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        let original = file.metadata().unwrap().len();
        file.set_len(crate::WAL_HARD_WATERMARK_BYTES).unwrap();
        let input = rig.input(Instant::now() + Duration::from_secs(2));
        let budget = input.budget_for_test();
        let handle = rig.submit(input);
        let result = wait(&handle, terminal);
        assert!(
            matches!(
            result,
            WalPreparationStatus::Rejected(WalPreparationError::Source(
                crate::sample_sources::wal_transport::SourceWalTransportRejection::WalUnavailable
            ))
        ),
            "{result:?}"
        );
        for i in 0..2 {
            rig.outside_source(i)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
        }
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        drop(handle);
        assert_eq!(budget.used(), 0);
        file.set_len(original).unwrap();
    }
    #[test]
    fn all_owner_original_deadline_includes_actual_journal_queue_wait() {
        let mut rig = Rig::new("all-owner-queued-expiry");
        assert!(rig.journal.shutdown());
        let store = OperationJournalStore::open(&rig.fixture.owner).unwrap();
        let (owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        rig.journal = owner;
        let input = rig.input(Instant::now() + Duration::from_millis(100));
        let budget = input.budget_for_test();
        let mut reply = rig.journal.client().prepare_wal_owners(input).unwrap();
        ready.wait();
        thread::sleep(Duration::from_millis(120));
        release.wait();
        assert!(matches!(
            reply.recv_timeout(Duration::from_secs(3)).unwrap(),
            Err(OperationJournalOwnerError::WalPreparation(
                WalPreparationError::Deadline
            ))
        ));
        assert!(
            rig.actor
                .as_ref()
                .unwrap()
                .events
                .lock()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            rig.journal
                .prepared_admission_id_allocations
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(budget.used(), 0);
    }
}
