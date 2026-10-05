//! Genuine normal-build metadata producer; Native execution is separately qualified.
use super::{PROCESS_WAIT, REPLY_WAIT, observe};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::WritableProfileGuard,
    durable_capacity::{CapacityRequirement, aggregate_capacity_plan, descriptor_capacity_facts},
    operation_journal::{
        OperationId, OperationJournalOwner, OperationJournalOwnerStatus, OperationJournalStore,
        ParticipantId, StoredRelativePath,
        coordinator::{
            CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
            UniversalCommandArgs,
        },
        model::{
            OperationActor, OperationIntent, OperationKind, UniversalCommandKind, UniversalIntent,
        },
        participants::{
            ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor,
            ParticipantKind, ParticipantRequirement, ParticipantSet, StoredSourceFence,
        },
        work_plan::{WorkUnitRole, derive_work_plan},
    },
    sample_sources::{
        GlobalLibraryWriter, SampleSource, SourceDatabase, SourceId, SourceWriter,
        db::{
            ManifestEnrollmentEntry, ManifestEnrollmentKind, ManifestEnrollmentRequest,
            SourceContentIdentityObservation,
        },
        library::{
            HarvestCopyDisposition, HarvestCopyReceiptKey, HarvestCopyReceiptObservation,
            HarvestCopyRequest, HarvestMetadataSnapshot, ReferenceFileKey,
            ReferenceSnapshotRequest, SourceRegistryParticipantRequest, SourceRegistrySnapshot,
        },
    },
};

// This production producer is typechecked on Linux/Windows, but only the native parent invokes
// it. It does not use cfg(test)/test-support constructors or claim physical file publication.
pub(super) fn produce(
    guard: &WritableProfileGuard,
    global: &GlobalLibraryWriter,
) -> (
    HarvestCopyReceiptObservation,
    [SourceWriter; 2],
    OperationJournalOwner,
) {
    let paths = [
        guard.profile_root().join("origin"),
        guard.profile_root().join("destination"),
    ];
    for path in &paths {
        fs::create_dir(path).unwrap();
    }
    let sources = [
        source(&paths[0], guard, "origin", "one.wav"),
        source(&paths[1], guard, "destination", "new.wav"),
    ];
    let parent = content(&sources[0], "one.wav");
    let child = content(&sources[1], "new.wav");
    let ids = [ParticipantId::new(), ParticipantId::new()];
    let mut descriptors = Vec::new();
    for (index, (id, o)) in ids.iter().zip([&parent, &child]).enumerate() {
        let r = o.file().source_revision();
        let fence = StoredSourceFence::new(
            r.source_id().clone(),
            std::str::from_utf8(r.root_identity().as_bytes()).unwrap(),
            r.database_identity(),
            r.lifecycle_generation(),
            r.revision(),
            r.lease_epoch(),
        )
        .unwrap();
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
                3,
                1,
            )
            .unwrap(),
            CopyCollisionPolicy::NoReplace,
            None,
        )
        .unwrap(),
    );
    let plan = derive_work_plan(&args, &participants).unwrap();
    let operation = OperationId::new();
    let harvest = plan
        .units()
        .iter()
        .find(|u| u.is_applicable() && u.key().role() == WorkUnitRole::Harvest)
        .unwrap()
        .key();
    // Persisted capacity DTOs only: actual owner startup below independently establishes its
    // native protected reserve. These descriptor facts create no operation admission/permit.
    let facts = descriptor_capacity_facts(&fs::File::open(&paths[1]).unwrap()).unwrap();
    let capacity = aggregate_capacity_plan(
        &[CapacityRequirement {
            facts,
            logical_bytes: 3,
        }],
        &std::collections::BTreeMap::new(),
    )
    .unwrap();
    let store = OperationJournalStore::open(guard).unwrap();
    let UniversalCommandArgs::Copy(copy) = args else {
        unreachable!()
    };
    store
        .persist_copy_intent(
            operation,
            OperationIntent {
                actor: OperationActor::User,
                kind: OperationKind::FileMutation,
                label: "historical copy metadata producer".into(),
            },
            UniversalIntent::new(
                None,
                UniversalCommandKind::Copy,
                "operation.status.copy",
                participants,
            )
            .unwrap(),
            copy,
            capacity,
            1,
        )
        .unwrap();
    let journal = OperationJournalOwner::start(store).unwrap();
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        match journal.status() {
            OperationJournalOwnerStatus::Available => break,
            OperationJournalOwnerStatus::Initializing => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            status => panic!("genuine native journal bootstrap failed: {status:?}"),
        }
    }
    let family = journal
        .client()
        .observe_copy_intent(operation, harvest)
        .unwrap()
        .recv_timeout(REPLY_WAIT)
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
    let enrollment = ManifestEnrollmentRequest::new(
        operation,
        destination,
        &sources[1],
        vec![ManifestEnrollmentEntry {
            path: StoredRelativePath::new("new.wav").unwrap(),
            kind: ManifestEnrollmentKind::Audio,
            size: 3,
            modified_ns: 1,
            file_identity: child.file().file_identity().unwrap().into(),
            content_hash: Some(child.content_hash().into()),
        }],
    )
    .unwrap();
    let enrollment = sources[1]
        .commit_manifest_enrollment(
            &enrollment,
            sources[1].catalog_database().write_batch().unwrap(),
            None,
        )
        .unwrap()
        .receipt;
    audit(&sources[1]);
    let global_key = plan
        .units()
        .iter()
        .find(|u| u.is_applicable() && u.key().role() == WorkUnitRole::GlobalLibrary)
        .unwrap()
        .key();
    let expected = global
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(REPLY_WAIT)
        .unwrap()
        .unwrap();
    let registry = global
        .apply_source_registry_participant(
            SourceRegistryParticipantRequest::new(
                guard.owner_identity(),
                operation,
                global_key,
                &expected,
                SourceRegistrySnapshot::new(
                    paths
                        .iter()
                        .zip(["origin", "destination"])
                        .map(|(p, id)| {
                            SampleSource::new_with_id(SourceId::from_string(id), p.clone())
                        })
                        .collect(),
                ),
            )
            .unwrap(),
        )
        .unwrap()
        .recv_timeout(REPLY_WAIT)
        .unwrap()
        .unwrap();
    let parent = content(&sources[0], "one.wav");
    let child = content(&sources[1], "new.wav");
    let snapshot = global
        .reference_snapshot(
            ReferenceSnapshotRequest::new(
                guard.owner_identity(),
                ReferenceFileKey::new(
                    SourceId::from_string("origin"),
                    StoredRelativePath::new("one.wav").unwrap(),
                )
                .unwrap(),
                ReferenceFileKey::new(
                    SourceId::from_string("destination"),
                    StoredRelativePath::new("new.wav").unwrap(),
                )
                .unwrap(),
                sources[0].reference_read_fence().unwrap(),
                sources[1].reference_read_fence().unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
        .recv_timeout(REPLY_WAIT)
        .unwrap()
        .unwrap();
    let request = HarvestCopyRequest::new(
        guard,
        family,
        parent,
        child,
        enrollment,
        registry,
        snapshot,
        HarvestMetadataSnapshot {
            rating: Some(2),
            tags: vec!["drum".into()],
            playback_type: Some("one-shot".into()),
        },
        "yield-native-process-fixture".into(),
    )
    .unwrap();
    let key = HarvestCopyReceiptKey::new(request.operation(), request.work_key()).unwrap();
    let proposal = *blake3::hash(request.proposal_bytes()).as_bytes();
    drop(global.apply_harvest_copy(request).unwrap());
    let observed = observe(global, key).unwrap();
    assert_eq!(observed.disposition(), HarvestCopyDisposition::Applied);
    assert_eq!(observed.proposal_identity(), &proposal);
    (observed, sources, journal)
}
fn source(path: &Path, guard: &WritableProfileGuard, id: &str, file: &str) -> SourceWriter {
    fs::write(path.join(file), b"abc").unwrap();
    let database =
        SourceDatabase::open_for_catalog_with_profile_guard(path, guard, Path::new(id)).unwrap();
    let writer = SourceWriter::new_for_scan(database, SourceId::from_string(id), 1).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let mut batch = writer.catalog_database().write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new(file), 3, 1, &blake3::hash(b"abc").to_hex())
        .unwrap();
    // Indexed metadata identity is an input, not an owned physical object capability.
    batch
        .set_file_identity(Path::new(file), Some(&format!("indexed-{id}")))
        .unwrap();
    writer.commit_scan_manifest_batch(&fence, batch).unwrap();
    audit(&writer);
    writer
}
fn audit(writer: &SourceWriter) {
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
}
fn content(writer: &SourceWriter, file: &str) -> SourceContentIdentityObservation {
    writer
        .observe_file_identity(StoredRelativePath::new(file).unwrap())
        .unwrap()
        .into_content_identity()
        .unwrap()
}
