use super::*;
impl Fixture {
    pub(super) fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().unwrap();
        let config = ConfigBaseGuard::set(base.path().into());
        let profile = PersistenceProfileGuard::named("harvest-copy-owner");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let global = GlobalLibraryWriter::start(&guard).unwrap();
        wait_global(&global);
        let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let sources = [
            source(&roots[0], &guard, "origin", "one.wav"),
            source(&roots[1], &guard, "destination", "new.wav"),
        ];
        let parent = content(&sources[0], "one.wav");
        let child = content(&sources[1], "new.wav");
        let ids = [ParticipantId::new(), ParticipantId::new()];
        let mut descriptors = Vec::new();
        for (index, (id, observation)) in ids.iter().zip([&parent, &child]).enumerate() {
            let fence = stored(observation);
            for kind in [
                ParticipantKind::SourceDatabase,
                ParticipantKind::Projection,
                ParticipantKind::Readiness,
            ] {
                if index == 0 && kind != ParticipantKind::SourceDatabase {
                    continue;
                }
                descriptors.push(
                    ParticipantDescriptor::new(
                        if kind == ParticipantKind::SourceDatabase {
                            *id
                        } else {
                            ParticipantId::new()
                        },
                        kind,
                        ParticipantRequirement::Required,
                        Some(fence.clone()),
                        None,
                    )
                    .unwrap(),
                );
            }
        }
        for kind in [
            ParticipantKind::Filesystem,
            ParticipantKind::GlobalLibrary,
            ParticipantKind::Harvest,
        ] {
            descriptors.push(
                ParticipantDescriptor::new(
                    ParticipantId::new(),
                    kind,
                    ParticipantRequirement::Required,
                    None,
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
        let args = UniversalCommandArgs::Copy(
            CopyIntentArgs::new(
                CopyEndpoint::new(
                    ids[0],
                    SourceId::from_string("origin"),
                    StoredRelativePath::new("one.wav").unwrap(),
                )
                .unwrap(),
                CopyEndpoint::new(
                    ids[1],
                    SourceId::from_string("destination"),
                    StoredRelativePath::new("new.wav").unwrap(),
                )
                .unwrap(),
                CopySourceExpectation::new(
                    parent.file().source_revision().revision(),
                    parent.file().file_identity().unwrap(),
                    10,
                    1,
                )
                .unwrap(),
                CopyCollisionPolicy::NoReplace,
                None,
            )
            .unwrap(),
        );
        let plan = derive_work_plan(&args, &participants).unwrap();
        let op = OperationId::new();
        let harvest = plan
            .units()
            .iter()
            .find(|u| u.is_applicable() && u.key().role() == WorkUnitRole::Harvest)
            .unwrap()
            .key();
        let capacity =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots()
                .0
                .intent()
                .capacity_plan()
                .clone();
        let record = StoredOperationRecord::new_v16_command(
            op,
            OperationIntent {
                actor: OperationActor::User,
                kind: OperationKind::FileMutation,
                label: "copy metadata fixture".into(),
            },
            UniversalIntent::new(
                None,
                UniversalCommandKind::Copy,
                "operation.status.copy",
                participants,
            )
            .unwrap(),
            args,
            capacity,
            1,
        );
        let store = OperationJournalStore::open(&guard).unwrap();
        store.insert_intent(&record).unwrap();
        let journal = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while journal.status() != OperationJournalOwnerStatus::Available {
            assert!(Instant::now() < end, "{:?}", journal.status());
            std::thread::sleep(Duration::from_millis(5));
        }
        let family = journal
            .client()
            .observe_copy_intent(op, harvest)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .unwrap();
        let destination = plan
            .units()
            .iter()
            .find(|u| {
                u.is_applicable()
                    && u.key().participant_id() == ids[1]
                    && matches!(
                        u.key().role(),
                        WorkUnitRole::SourceAffect | WorkUnitRole::DestinationSourceReconcile
                    )
            })
            .unwrap()
            .key();
        let request = ManifestEnrollmentRequest::new(
            op,
            destination,
            &sources[1],
            vec![ManifestEnrollmentEntry {
                path: StoredRelativePath::new("new.wav").unwrap(),
                kind: ManifestEnrollmentKind::Audio,
                size: 10,
                modified_ns: 1,
                file_identity: "object-destination".into(),
                content_hash: Some("a".repeat(64)),
            }],
        )
        .unwrap();
        let enrollment = sources[1]
            .commit_manifest_enrollment(
                &request,
                sources[1].catalog_database().write_batch().unwrap(),
                None,
            )
            .unwrap()
            .receipt;
        // Enrollment advances generic revision even for a previously enrolled row. A real
        // complete audit is required before a fresh reference/file observation; never mark
        // audit coverage by copying a revision or accepting the enrollment DTO.
        sources[1]
            .catalog_database()
            .begin_source_directory_truth_generation(2, 0)
            .unwrap();
        let fence = sources[1].current_scan_fence().unwrap();
        sources[1]
            .catalog_database()
            .write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(
                2,
                fence.revision(),
                10,
                fence.root_identity().clone(),
                None,
            )
            .unwrap();
        let key = plan
            .units()
            .iter()
            .find(|u| u.is_applicable() && u.key().role() == WorkUnitRole::GlobalLibrary)
            .unwrap()
            .key();
        let expected = global
            .load_source_registry_snapshot()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        let replacement = SourceRegistrySnapshot::new(
            roots
                .iter()
                .zip(["origin", "destination"])
                .map(|(root, id)| SampleSource {
                    id: SourceId::from_string(id),
                    root: root.path().into(),
                    role: SourceRole::Normal,
                    metadata_storage: SourceMetadataStorage::SourceFolder,
                    primary_import_folder: "inbox".into(),
                })
                .collect(),
        );
        let registry = global
            .apply_source_registry_participant(
                SourceRegistryParticipantRequest::new(
                    guard.owner_identity(),
                    op,
                    key,
                    &expected,
                    replacement,
                )
                .unwrap(),
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        Self {
            global: Some(global),
            journal: Some(journal),
            sources: Some(sources),
            family,
            enrollment,
            registry,
            guard: Some(guard),
            roots,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
}
