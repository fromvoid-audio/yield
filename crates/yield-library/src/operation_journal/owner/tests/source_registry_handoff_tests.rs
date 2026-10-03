#[cfg(test)]
mod integration {
    use super::super::*;
    use crate::sample_sources::SourceDatabase as Db;
    use crate::sample_sources::db::{
        SourceRegistryMetadataCandidate, SourceRegistryMetadataHandoffError,
        SourceRegistryMetadataPermit, SourceWriter,
    };
    use std::time::Instant;

    fn writer(
        f: &Fixture,
        name: &str,
    ) -> (
        tempfile::TempDir,
        SourceWriter,
        crate::sample_sources::db::SourceAuthorityRegistration,
    ) {
        let root = tempfile::tempdir().unwrap();
        let db = Db::open_for_catalog_with_profile_guard(root.path(), &f.owner, name).unwrap();
        let authority = db.prepare_source_authority().unwrap();
        let writer = SourceWriter::new_for_scan(db, SourceId::from_string(name), 1).unwrap();
        (root, writer, authority)
    }

    // Explicit logical registration model: genuine roots/lease/writer/SQLite, but no
    // native reserve or capacity provider. This cannot qualify production registration.
    fn registration_model(
        authority: &crate::sample_sources::db::SourceAuthorityRegistration,
    ) -> (
        Arc<SourceAuthorityRequest>,
        SourceAuthorityRegistrationReceipt,
        SourceAuthorityDrainMarker,
    ) {
        let input = authority.journal_request().unwrap();
        let proof = input
            .request
            .accept_confirmation(input.confirmation)
            .unwrap();
        let receipt = SourceAuthorityRegistrationReceipt::new(
            input.request.registration_id(),
            input.request.lease_identity().clone(),
            vec![VolumeIdentity { device: 1 }],
        );
        (input.request, receipt, proof.into_drain_marker())
    }
    fn candidate(
        w: &SourceWriter,
        a: &crate::sample_sources::db::SourceAuthorityRegistration,
        r: &SourceAuthorityRegistrationReceipt,
    ) -> SourceRegistryMetadataCandidate {
        w.prepare_registry_metadata_handoff(a, r.clone(), Instant::now() + Duration::from_secs(2))
            .unwrap()
    }
    fn insert_model(
        runtime: &mut OwnerRuntime,
        request: Arc<SourceAuthorityRequest>,
        receipt: SourceAuthorityRegistrationReceipt,
        marker: SourceAuthorityDrainMarker,
    ) {
        let metadata_registration =
            crate::sample_sources::db::source_registry_handoff::MetadataRegistrationCustody::new();
        // Explicit modeled journal completeness; no native capacity registration.
        metadata_registration.activate().unwrap();
        runtime.source_registrations.insert(
            receipt.registration_id,
            SourceAuthorityRecord {
                metadata_registration,
                request,
                receipt,
                drain_marker: Some(marker),
                readiness: SourceRegistrationReadiness::Registered,
                metadata_writer: None,
            },
        );
    }
    fn rejected(
        result: Result<SourceRegistryMetadataPermit, OperationJournalOwnerError>,
    ) -> OperationJournalOwnerError {
        match result {
            Err(e) => e,
            Ok(_) => panic!("handoff unexpectedly authenticated"),
        }
    }

    #[test]
    fn actual_prepared_owner_and_matching_scalar_receipt_cannot_issue_candidate() {
        let f = fixture("metadata-real-prepared-refusal");
        let (_root, w, a) = writer(&f, "prepared");
        let input = a.journal_request().unwrap();
        // Deliberately forged scalar fixture. There is no public receipt constructor.
        let r = SourceAuthorityRegistrationReceipt::new(
            input.request.registration_id(),
            input.request.lease_identity().clone(),
            vec![VolumeIdentity { device: 1 }],
        );
        assert!(matches!(
            w.prepare_registry_metadata_handoff(&a, r, Instant::now() + Duration::from_secs(2)),
            Err(SourceRegistryMetadataHandoffError::Authority(
                SourceAuthorityError::ConfirmationClosed
            ))
        ));
        assert!(w.catalog_database().source_authority_close_ready());
    }

    #[test]
    fn actual_worker_refuses_model_source_phase_without_registration_and_closed_queue_refunds_pin()
    {
        let (f, mut owner) = open_owner("metadata-actual-queue-refusal");
        let (_root, w, a) = writer(&f, "queue");
        let (_request, receipt, _marker) = registration_model(&a);
        let client = owner.client();
        let result = client
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            rejected(result),
            OperationJournalOwnerError::SourceAuthorityConflict { .. }
        ));
        assert!(w.catalog_database().source_authority_close_ready());
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Available);
        assert!(owner.shutdown());
        assert!(
            client
                .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
                .is_err()
        );
        assert!(
            w.catalog_database().source_authority_close_ready(),
            "rejected queue retained a pin"
        );
    }

    #[test]
    fn modeled_registration_expired_original_deadline_cannot_authenticate_or_bind_writer() {
        let f = fixture("metadata-original-deadline-model");
        let (_root, w, a) = writer(&f, "expired");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let queued = w
            .prepare_registry_metadata_handoff(
                &a,
                receipt.clone(),
                Instant::now() + Duration::from_millis(200),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(220));
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(queued)),
            OperationJournalOwnerError::SourceMetadata(
                SourceRegistryMetadataHandoffError::Deadline
            )
        ));
        assert!(
            runtime.source_registrations[&receipt.registration_id]
                .metadata_writer
                .is_none()
        );
        assert!(w.catalog_database().source_authority_close_ready());
    }

    #[test]
    fn model_retained_map_limit_precedes_effects_and_preserves_exact_existing_replay() {
        let f = fixture("metadata-retention-limit-model");
        let (_root, _w, a) = writer(&f, "retained");
        let input = a.journal_request().unwrap();
        let replay = SourceAuthorityRegistrationRequest {
            request: Arc::clone(&input.request),
            confirmation: input.confirmation.clone(),
        };
        let proof = input
            .request
            .accept_confirmation(input.confirmation)
            .unwrap();
        let receipt = SourceAuthorityRegistrationReceipt::new(
            input.request.registration_id(),
            input.request.lease_identity().clone(),
            vec![VolumeIdentity { device: 1 }],
        );
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(
            &mut runtime,
            Arc::clone(&input.request),
            receipt.clone(),
            proof.into_drain_marker(),
        );
        // Count-only synthetic tombstones, not genuine registration/retirement receipts.
        for n in 1..4096 {
            runtime.retired_source_registrations.insert(
                crate::sample_sources::db::SourceRegistrationId::model_key_for_tests(n),
                RetiredSourceAuthorityRecord {
                    media_root_identity: input.request.media_root().identity().into(),
                    database_root_identity: input.request.database_root().identity().into(),
                    database_file_identity: input.request.database_file_identity().into(),
                    lease_identity: input.request.lease_identity().clone(),
                    receipt: receipt.clone(),
                },
            );
        }
        assert_eq!(runtime.register_source_authority(replay).unwrap(), receipt);
        let (_new_root, _new_writer, new_a) = writer(&f, "new-at-cap");
        let new_input = new_a.journal_request().unwrap();
        let request = Arc::clone(&new_input.request);
        assert!(matches!(runtime.register_source_authority(new_input),
            Err(OperationJournalOwnerError::SourceAuthorityConflict {reason})
                if reason.contains("retention limit")));
        assert_eq!(
            request.phase_for_tests(),
            0,
            "cap rejection consumed original confirmation"
        );
        assert_eq!(
            runtime.source_registrations.len() + runtime.retired_source_registrations.len(),
            4096
        );
        assert!(runtime.capacity_provider.is_none());
    }

    #[test]
    fn modeled_registered_phase_does_not_bypass_actual_missing_or_partial_journal_record() {
        let f = fixture("metadata-real-consumer-refusal");
        let (_root, w, a) = writer(&f, "partial");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(candidate(&w, &a, &receipt))),
            OperationJournalOwnerError::SourceAuthorityConflict { .. }
        ));
        insert_model(&mut runtime, request, receipt.clone(), marker);
        for readiness in [
            SourceRegistrationReadiness::Registering,
            SourceRegistrationReadiness::RecoveryRequired {
                stage: "model-only",
                reason: "incomplete".into(),
            },
            SourceRegistrationReadiness::Draining,
        ] {
            runtime
                .source_registrations
                .get_mut(&receipt.registration_id)
                .unwrap()
                .readiness = readiness;
            assert!(matches!(
                rejected(
                    runtime.authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
                ),
                OperationJournalOwnerError::SourceAuthorityConflict { .. }
            ));
            assert!(
                runtime.source_registrations[&receipt.registration_id]
                    .metadata_writer
                    .is_none()
            );
        }
        runtime
            .source_registrations
            .get_mut(&receipt.registration_id)
            .unwrap()
            .readiness = SourceRegistrationReadiness::Registered;
        let mut wrong = candidate(&w, &a, &receipt);
        wrong.receipt.volume_identities = vec![VolumeIdentity { device: 2 }];
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(wrong)),
            OperationJournalOwnerError::SourceAuthorityConflict { .. }
        ));
    }

    #[test]
    fn modeled_registration_pins_original_writer_and_retained_lease_but_no_capacity() {
        let f = fixture("metadata-writer-binding-model");
        let (_root, w, a) = writer(&f, "binding");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let permit = runtime
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap();
        assert_eq!(permit.observation().subject().source_id(), "binding");
        assert!(!w.catalog_database().source_authority_close_ready());
        permit
            .validate_metadata_boundary(&f.owner, permit.observation().subject())
            .unwrap();
        let sql = permit
            .enter_metadata_sql(&f.owner, permit.observation().subject())
            .unwrap();
        drop(sql);
        assert!(
            runtime.capacity_provider.is_none(),
            "model supplied no physical capacity provider"
        );
        drop(permit);
        assert!(w.catalog_database().source_authority_close_ready());
        let stale = candidate(&w, &a, &receipt);
        let db = w.into_catalog_database();
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(stale)),
            OperationJournalOwnerError::SourceMetadata(_)
        ));
        let replacement =
            SourceWriter::new_for_scan(db, SourceId::from_string("binding"), 1).unwrap();
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(candidate(
                &replacement,
                &a,
                &receipt
            ))),
            OperationJournalOwnerError::SourceAuthorityConflict { .. }
        ));
        assert!(runtime.capacity_provider.is_none());
    }

    #[test]
    fn modeled_delivered_handoff_loses_registration_on_partial_state_or_actual_owner_drop() {
        let f = fixture("metadata-delivered-registration-loss-model");
        let (_root, w, a) = writer(&f, "registration-loss");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let permit = runtime
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap();
        assert!(
            runtime
                .retain_source_recovery(
                    receipt.registration_id,
                    "model-partial",
                    "no native qualification".into(),
                    None
                )
                .is_err()
        );
        assert!(matches!(
            permit.enter_metadata_sql(&f.owner, permit.observation().subject()),
            Err(SourceRegistryMetadataHandoffError::RegistrationLost)
        ));
        drop(permit);
        // Model completion is explicit again; production only activates after genuine installation.
        let record = runtime
            .source_registrations
            .get_mut(&receipt.registration_id)
            .unwrap();
        record.readiness = SourceRegistrationReadiness::Registered;
        assert!(
            record.metadata_registration.activate().is_err(),
            "revocation was revived"
        );
        record.metadata_registration =
            crate::sample_sources::db::source_registry_handoff::MetadataRegistrationCustody::new();
        record.metadata_registration.activate().unwrap();
        let permit = runtime
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap();
        drop(runtime);
        assert!(matches!(
            permit.enter_metadata_sql(&f.owner, permit.observation().subject()),
            Err(SourceRegistryMetadataHandoffError::RegistrationLost)
        ));
        // An unopened delivered token owns no writer SQL effect and cannot stall handoff.
        let db = w.into_catalog_database();
        drop(permit);
        assert!(db.source_authority_close_ready());
    }

    #[test]
    fn modeled_handoff_rejects_actual_foreign_profile_and_replaced_source_root() {
        let f = fixture("metadata-namespace-context-model");
        let (root, w, a) = writer(&f, "namespace");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let permit = runtime
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap();
        {
            let _other_profile = PersistenceProfileGuard::named("metadata-foreign-consumer");
            let other = WritableProfileGuard::acquire_current().unwrap();
            assert!(matches!(
                permit.enter_metadata_sql(&other, permit.observation().subject()),
                Err(SourceRegistryMetadataHandoffError::Context)
            ));
        }
        drop(permit);
        let queued = candidate(&w, &a, &receipt);
        let displaced = f._base.path().join("displaced-source");
        fs::rename(root.path(), &displaced).unwrap();
        fs::create_dir(root.path()).unwrap();
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(queued)),
            OperationJournalOwnerError::SourceMetadata(_)
        ));
        // Restore the fixture namespace so the original genuine lease can close normally.
        fs::remove_dir(root.path()).unwrap();
        fs::rename(displaced, root.path()).unwrap();
        assert!(w.catalog_database().source_authority_close_ready());
    }

    #[test]
    fn modeled_registration_drain_and_guard_drop_revoke_queued_original_candidate() {
        let f = fixture("metadata-drain-model");
        let (_root, w, a) = writer(&f, "drain");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let queued = candidate(&w, &a, &receipt);
        runtime.begin_source_drain(receipt.clone()).unwrap();
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(queued)),
            OperationJournalOwnerError::SourceAuthorityConflict { .. }
        ));
        assert!(matches!(
            w.prepare_registry_metadata_handoff(
                &a,
                receipt,
                Instant::now() + Duration::from_secs(2)
            ),
            Err(SourceRegistryMetadataHandoffError::Authority(_))
        ));
        drop(a);
    }

    #[test]
    fn modeled_registration_actual_writer_handoff_waits_for_metadata_quiescence() {
        let f = fixture("metadata-handoff-custody-model");
        let (_root, w, a) = writer(&f, "handoff");
        let (request, receipt, marker) = registration_model(&a);
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let permit = runtime
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap();
        let gate = w.registry_handoff_gate_for_tests();
        let profile = f.owner.try_clone().unwrap();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let consumer_ready = Arc::clone(&ready);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let consumer = scope.spawn(move || {
                let sql = permit
                    .enter_metadata_sql(&profile, permit.observation().subject())
                    .unwrap();
                consumer_ready.wait();
                let stop = Instant::now() + Duration::from_secs(2);
                while !gate.is_closed_for_tests() {
                    assert!(
                        Instant::now() < stop,
                        "original writer never closed admission"
                    );
                    thread::yield_now();
                }
                tx.send("SQL quiescent before original database handoff")
                    .unwrap();
                drop(sql);
                drop(permit);
            });
            ready.wait();
            let db = w.into_catalog_database();
            consumer.join().unwrap();
            assert_eq!(
                rx.try_recv().unwrap(),
                "SQL quiescent before original database handoff"
            );
            assert!(db.source_authority_close_ready());
            drop(db);
        });
    }

    #[test]
    fn actual_original_owner_rejects_foreign_registration_deadline_and_dropped_guard() {
        let f = fixture("metadata-source-context-model");
        let (_root, w, a) = writer(&f, "original");
        let (_foreign_root, _other, foreign) = writer(&f, "foreign");
        let (request, receipt, marker) = registration_model(&a);
        let (_foreign_request, foreign_receipt, _foreign_marker) = registration_model(&foreign);
        assert!(matches!(
            w.prepare_registry_metadata_handoff(
                &foreign,
                foreign_receipt,
                Instant::now() + Duration::from_secs(2)
            ),
            Err(SourceRegistryMetadataHandoffError::Observation(_))
        ));
        for deadline in [Instant::now(), Instant::now() + Duration::from_secs(3)] {
            assert!(matches!(
                w.prepare_registry_metadata_handoff(&a, receipt.clone(), deadline),
                Err(SourceRegistryMetadataHandoffError::Deadline)
            ));
        }
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&f.owner).unwrap()).unwrap();
        insert_model(&mut runtime, request, receipt.clone(), marker);
        let queued = candidate(&w, &a, &receipt);
        drop(a);
        assert!(matches!(
            rejected(runtime.authenticate_source_registry_metadata(queued)),
            OperationJournalOwnerError::SourceMetadata(_)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_completed_registration_authenticates_then_exactly_drains_and_retires() {
        let f = fixture("native-metadata-handoff");
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let mut owner = OperationJournalOwner::start_with_capacity_fixture_for_test(
            store,
            fixture_volume_registry(&f),
        )
        .unwrap();
        wait_until_available(&owner);
        let (_root, w, mut a) = writer(&f, "native");
        let client = owner.client();
        let receipt = client
            .register_source_authority(a.journal_request().unwrap())
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let permit = client
            .authenticate_source_registry_metadata(candidate(&w, &a, &receipt))
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        permit
            .validate_metadata_boundary(&f.owner, permit.observation().subject())
            .unwrap();
        let sql = permit
            .enter_metadata_sql(&f.owner, permit.observation().subject())
            .unwrap();
        drop(sql);
        assert!(!w.catalog_database().source_authority_close_ready());
        drop(permit);
        let db = w.into_catalog_database();
        client
            .begin_source_drain(receipt.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let ack = a.close_database(db).unwrap();
        client
            .finalize_source_retirement(receipt, ack)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        drop(a);
        assert!(owner.shutdown());
    }
}
