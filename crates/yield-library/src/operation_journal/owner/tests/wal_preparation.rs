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
                let authorities = writers
                    .iter()
                    .map(|(id, w)| {
                        (
                            id.clone(),
                            w.catalog_database().prepare_source_authority().unwrap(),
                        )
                    })
                    .collect::<HashMap<_, _>>();
                // Drive the genuine confirmation lifecycle without installing a capacity provider.
                // These fixtures prove transport custody, never physical qualification or admission.
                let registrations = authorities
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
                if let Some(sender) = observation {
                    assert_eq!(writers.len(), 1);
                    let id = writers.keys().next().unwrap().clone();
                    let writer = writers.remove(&id).unwrap();
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
                    sender.send((allocation, fence, deadline)).unwrap();
                }
                let mut held: HashMap<String, SourceWalProducerOwner> = HashMap::new();
                loop {
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
                    let mut remaining = HashMap::new();
                    for (id, mut owner) in held.drain() {
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
                            owner.request_abort(None);
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
            assert_eq!(budget.used(), 0);
            drop(handle);
            drop(actor);
            assert!(global.shutdown());
            assert!(journal.shutdown());
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
    #[test]
    fn all_owner_real_pre_audit_lease_input_stays_distinct_and_aborts_original_writer() {
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
        let (allocation, fence, deadline) = receive.recv_timeout(Duration::from_secs(3)).unwrap();
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
        let handle = journal
            .client()
            .prepare_wal_owners(input)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
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
    fn all_owner_real_tag_and_layout_factories_keep_actual_original_family_policy() {
        use crate::sample_sources::db::write::{
            SOURCE_LAYOUT_ALGORITHM_V1, SourceLayoutComputedView,
        };
        use crate::sample_sources::wal_transport::SourceWalPolicyFamily;
        use crate::sample_sources::{SourceCommandId, SourceTagAssignmentMutation};
        for layout in [false, true] {
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
