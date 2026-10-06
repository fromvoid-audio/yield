//! Actual profile/journal/source refusal checks. Unissued receipts are adversarial inputs only;
//! they never install a registry entry or qualify a physical recording route.
use super::*;
use crate::operation_journal::{
    OperationActor, OperationIntent, OperationKind, UniversalCommandKind,
};
use crate::{app_dirs::AppRootGuard, test_runtime::TestRuntimeGuard};
use std::time::{Duration, Instant};

struct Fixture {
    owner: OperationJournalOwner,
    guard: WritableProfileGuard,
    _root: AppRootGuard,
    _profile: tempfile::TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let profile = tempfile::tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_owned()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let store = OperationJournalStore::open(&guard).unwrap();
        let owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while owner.status() != OperationJournalOwnerStatus::Available {
            assert!(Instant::now() < deadline, "actual journal initialization");
            std::thread::yield_now();
        }
        Self {
            owner,
            guard,
            _root: root,
            _profile: profile,
            _runtime: runtime,
        }
    }
}

#[test]
fn recording_custody_original_unqualified_journal_refuses_unissued_source_receipt() {
    let _native_harness: fn(bool) = native_original_recording_custody;
    let mut f = Fixture::new();
    let media = tempfile::tempdir().unwrap();
    let source = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
        media.path(),
        &f.guard,
        "recording-original-source",
    )
    .unwrap();
    let authority = source.prepare_source_authority().unwrap();
    let envelope = authority.journal_request().unwrap();
    let request = envelope.request;
    let descriptor = request.media_root().try_clone_descriptor().unwrap();
    let facts = descriptor_capacity_facts(&descriptor).unwrap();
    // Genuine original source confirmation/custody, but explicitly NO journal registration.
    // This adversarial unissued value must not be upgraded into a receipt or producer route.
    let unissued = SourceAuthorityRegistrationReceipt::new(
        request.registration_id(),
        request.lease_identity().clone(),
        vec![facts.identity],
    );
    let proof = request.accept_confirmation(envelope.confirmation).unwrap();
    let client = f.owner.client();
    let before = *client.shared.input_bytes.lock().unwrap();
    let factories = FACTORIES.load(Ordering::Acquire);
    let operation = OperationId::new();
    let result = client
        .prepare_recording_producer_custody(operation, unissued, &authority)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(matches!(
        result,
        Err(OperationJournalOwnerError::RecordingProducerCustody(
            RecordingProducerCustodyError::Changed | RecordingProducerCustodyError::Closed
        ))
    ));
    assert_eq!(FACTORIES.load(Ordering::Acquire), factories);
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), before);
    assert!(
        client
            .get(operation)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .is_none()
    );
    drop(proof);
    drop(request);
    drop(authority);
    drop(source);
    assert!(f.owner.shutdown());
}

#[test]
fn recording_custody_scalar_facade_cannot_create_slot_and_drain_closes_original_queue() {
    let mut f = Fixture::new();
    let client = f.owner.client();
    let before = *client.shared.input_bytes.lock().unwrap();
    // Private test construction is a forgery counterexample, not an issuance seam.
    let forged = RecordingProducerCustody {
        operation: OperationId::new(),
        digest: [1; 32],
        owner: Arc::downgrade(&client.shared),
    };
    let factories = FACTORIES.load(Ordering::Acquire);
    for _ in 0..2 {
        assert!(matches!(
            forged
                .revalidate()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap(),
            Err(OperationJournalOwnerError::RecordingProducerCustody(
                RecordingProducerCustodyError::Changed
            ))
        ));
    }
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), before);
    assert_eq!(FACTORIES.load(Ordering::Acquire), factories);
    let full = client
        .shared
        .reserve_input(
            InputClass::Ordinary,
            before.remaining[InputClass::Ordinary.index()],
        )
        .unwrap();
    assert!(matches!(
        forged.revalidate(),
        Err(OperationJournalOwnerQueueError::InputBytesFull { .. })
    ));
    drop(full);
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), before);
    assert!(f.owner.begin_command_drain());
    assert!(matches!(
        forged.revalidate(),
        Err(OperationJournalOwnerQueueError::Closed)
    ));
    assert!(f.owner.shutdown());
    assert!(matches!(
        forged.revalidate(),
        Err(OperationJournalOwnerQueueError::Closed)
    ));
    assert_eq!(
        *client.shared.input_bytes.lock().unwrap(),
        InputBudget::shared_initial()
    );
}

#[test]
fn recording_custody_revalidation_queues_while_original_retained_debit_is_held() {
    let mut f = Fixture::new();
    let client = f.owner.client();
    let before = *client.shared.input_bytes.lock().unwrap();
    // Hold the actual original-pool debit for a producer lifetime. This test issues
    // no Source registration or producer slot: the unissued facade must still refuse.
    let retained = client
        .shared
        .reserve_input(InputClass::Ordinary, INPUT_BYTES)
        .unwrap();
    let forged = RecordingProducerCustody {
        operation: OperationId::new(),
        digest: [1; 32],
        owner: Arc::downgrade(&client.shared),
    };
    assert!(matches!(
        forged
            .revalidate()
            .expect("temporary command and retained producer credit must coexist")
            .recv_timeout(Duration::from_secs(3))
            .unwrap(),
        Err(OperationJournalOwnerError::RecordingProducerCustody(
            RecordingProducerCustodyError::Changed
        ))
    ));
    // The worker may send its reply just before dropping temporary input custody.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while client.shared.input_bytes.lock().unwrap().remaining[0]
        != before.remaining[0] - INPUT_BYTES
    {
        assert!(
            std::time::Instant::now() < deadline,
            "temporary command input was not released"
        );
        std::thread::yield_now();
    }
    drop(retained);
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), before);
    assert!(f.owner.shutdown());
}

// Native acceptance uses the production constructor and real registered source/root/admission.
// No fixture capacity registry, injected receipt, synthetic root or platform bypass is used.
fn native_original_recording_custody(lost_reply: bool) {
    use crate::file_operation_owner::{FileOperationOwner, SourceFence};
    use crate::operation_journal::coordinator::StoredRelativePath;
    use crate::operation_journal::namespace_commands::{
        NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation, NamespaceDirectoryLocator,
        NamespaceLeafName, NamespaceTarget,
    };
    use crate::operation_journal::participants::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantId,
        ParticipantKind, ParticipantRequirement, StoredSourceFence,
    };
    use crate::operation_journal::recording_commands::{
        RECORDING_JOURNAL_PEAK_BYTES, RecordingFormat,
    };
    use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, FullCapacityAllocation, FullCapacityAllocationClass,
    };
    let runtime = TestRuntimeGuard::acquire();
    let profile = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(profile.path()).unwrap();
    let root = AppRootGuard::set(canonical).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut owner =
        OperationJournalOwner::start(OperationJournalStore::open(&guard).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match owner.status() {
            OperationJournalOwnerStatus::Available => break,
            OperationJournalOwnerStatus::Unavailable { reason } => {
                panic!("genuine native bootstrap unavailable: {reason}")
            }
            _ => assert!(
                Instant::now() < deadline,
                "genuine native bootstrap timeout"
            ),
        }
        std::thread::yield_now();
    }
    let client = owner.client();
    let media = tempfile::tempdir().unwrap();
    let source = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
        media.path(),
        &guard,
        "native-recording-custody",
    )
    .unwrap();
    let authority = source.prepare_source_authority().unwrap();
    let envelope = authority.journal_request().unwrap();
    let original = Arc::clone(&envelope.request);
    let receipt = client
        .register_source_authority(envelope)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let epoch = u64::try_from(receipt.lease_identity.epoch()).unwrap();
    let source_id = crate::sample_sources::SourceId::from_string("native-recording-custody");
    let stored = StoredSourceFence::new(
        source_id.clone(),
        original.media_root().identity(),
        original.database_file_identity(),
        1,
        1,
        epoch,
    )
    .unwrap();
    let fence = SourceFence::new(
        source_id.clone(),
        stored.root_identity(),
        stored.database_identity(),
        stored.lifecycle_generation(),
        Some(stored.source_revision()),
        epoch,
    )
    .unwrap();
    let files = FileOperationOwner::start().unwrap();
    let registered_root = files
        .register_source_root(Arc::clone(&original), &guard, fence)
        .unwrap();
    let source_participant = ParticipantId::new();
    let filesystem = ParticipantId::new();
    let mut descriptors = Vec::new();
    for (id, kind, fence) in [
        (
            source_participant,
            ParticipantKind::SourceDatabase,
            Some(stored.clone()),
        ),
        (filesystem, ParticipantKind::Filesystem, None),
        (ParticipantId::new(), ParticipantKind::GlobalLibrary, None),
        (
            ParticipantId::new(),
            ParticipantKind::Harvest,
            Some(stored.clone()),
        ),
        (
            ParticipantId::new(),
            ParticipantKind::Projection,
            Some(stored.clone()),
        ),
        (
            ParticipantId::new(),
            ParticipantKind::Readiness,
            Some(stored.clone()),
        ),
    ] {
        descriptors.push(
            ParticipantDescriptor::new(id, kind, ParticipantRequirement::Required, fence, None)
                .unwrap(),
        );
    }
    let checkpoints = descriptors
        .iter()
        .map(|p| {
            ParticipantCheckpoint::new(p.id(), ParticipantCheckpointState::Pending, None, None)
                .unwrap()
        })
        .collect();
    let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
    let parent = NamespaceDirectoryEndpoint::new(
        source_participant,
        source_id,
        NamespaceDirectoryLocator::Root,
        NamespaceDirectoryExpectation::new(1, original.media_root().identity(), None).unwrap(),
    )
    .unwrap();
    let format = RecordingFormat::new(48_000, 2, 100, 68).unwrap();
    let proposal = RecordingProposal::new(
        OperationId::new(),
        OutputIdentity::new(),
        WorkUnitKey::new(0, 30, filesystem, WorkUnitRole::Stage),
        participants.clone(),
        registered_root,
        registered_root,
        NamespaceTarget::new(parent, NamespaceLeafName::new("capture.wav").unwrap()).unwrap(),
        StoredRelativePath::new("capture-staging.wav").unwrap(),
        format,
    )
    .unwrap();
    let reserve = std::fs::File::open(
        guard
            .profile_root()
            .join("operation_journal")
            .join(super::super::super::capacity_reserve_namespace::RESERVE_FILE_NAME),
    )
    .unwrap();
    let facts = descriptor_capacity_facts(&reserve).unwrap();
    let mut allocations = Vec::new();
    let mut claims = Vec::new();
    for (class, logical) in [
        (
            FullCapacityAllocationClass::DestinationStaging,
            format.max_encoded_bytes().unwrap(),
        ),
        (
            FullCapacityAllocationClass::JournalRecord,
            RECORDING_JOURNAL_PEAK_BYTES,
        ),
    ] {
        let allocated = logical
            .div_ceil(facts.allocation_unit)
            .checked_mul(facts.allocation_unit)
            .unwrap();
        allocations.push(
            FullCapacityAllocation::new(
                facts.identity.clone(),
                facts.allocation_unit,
                class,
                logical,
                allocated,
                crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap(),
        );
        claims.push(
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                facts.identity.clone(),
                class,
                allocated,
            )
            .unwrap(),
        );
    }
    let prepared = PreparedAdmissionInput::try_new(
        OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::FileMutation,
            label: "genuine native recording custody".into(),
        },
        UniversalIntent::new(
            None,
            UniversalCommandKind::CreateFile,
            "recording.capture",
            participants.clone(),
        )
        .unwrap(),
        UniversalCommandArgs::Recording(proposal.clone()),
        participants,
        FullCapacityPlan::new(allocations).unwrap(),
        AdmissionClaimInventory::new(claims).unwrap(),
        1,
    )
    .unwrap();
    let operation = client
        .admit_prepared(prepared)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(operation, proposal.operation_id());
    let before = *client.shared.input_bytes.lock().unwrap();
    let factories = FACTORIES.load(Ordering::Acquire);
    let mut request = client
        .prepare_recording_producer_custody(operation, receipt.clone(), &authority)
        .unwrap();
    let facade = if lost_reply {
        drop(request);
        None
    } else {
        Some(
            request
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap(),
        )
    };
    // A small original point-read command is the completion barrier for an abandoned reply.
    // It fits while the first command still holds its transient debit; a second large
    // preparation cannot truthfully be queued until the first has shrunk to retained credit.
    assert_eq!(
        client
            .get(operation)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap()
            .unwrap()
            .operation_id,
        operation,
    );
    assert!(matches!(
        client
            .prepare_recording_producer_custody(operation, receipt.clone(), &authority)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap(),
        Err(OperationJournalOwnerError::RecordingProducerCustody(
            RecordingProducerCustodyError::Busy
        ))
    ));
    assert_eq!(FACTORIES.load(Ordering::Acquire), factories + 1);
    let retained = *client.shared.input_bytes.lock().unwrap();
    assert_eq!(retained.remaining[0], before.remaining[0] - INPUT_BYTES);
    assert_eq!(retained.publication_remaining, before.publication_remaining);
    if let Some(facade) = facade {
        assert_eq!(facade.operation_id(), operation);
        assert_eq!(
            facade.proposal_hash(),
            *blake3::hash(&proposal.encode().unwrap()).as_bytes()
        );
        facade
            .revalidate()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(owner.begin_command_drain());
        assert!(matches!(
            facade.revalidate(),
            Err(OperationJournalOwnerQueueError::Closed)
        ));
    } else {
        assert!(owner.begin_command_drain());
    }
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), retained);
    assert!(owner.shutdown());
    assert_eq!(
        *client.shared.input_bytes.lock().unwrap(),
        InputBudget::shared_initial()
    );
    assert_eq!(FACTORIES.load(Ordering::Acquire), factories + 1);
    assert!(matches!(
        files.shutdown(),
        crate::file_operation_owner::FileOperationOwnerShutdown::DrainedAck
    ));
    drop(authority);
    drop(original);
    drop(source);
    drop(reserve);
    drop(guard);
    drop(root);
    drop(profile);
    drop(runtime);
}

#[cfg(target_os = "macos")]
#[test]
fn recording_custody_native_production_original_slot_currentness_and_drain() {
    native_original_recording_custody(false);
}
#[cfg(target_os = "macos")]
#[test]
fn recording_custody_native_production_lost_reply_keeps_original_slot_and_credit() {
    native_original_recording_custody(true);
}

#[test]
fn recording_original_destination_refuses_unrelated_matching_source() {
    use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
    use crate::operation_journal::participants::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantId,
    };
    use serde_json::Value;
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let profile = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(profile.path().to_owned()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let media = tempfile::tempdir().unwrap();
    let source = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
        media.path(),
        &guard,
        "recording-held-original",
    )
    .unwrap();
    let authority = source.prepare_source_authority().unwrap();
    let envelope = authority.journal_request().unwrap();
    let request = envelope.request;
    let _confirmation = request.accept_confirmation(envelope.confirmation).unwrap();
    let epoch = u64::try_from(request.lease_identity().epoch()).unwrap();

    // These proposals are stored observations only. The held Source request is genuine;
    // this test never installs a journal receipt, capacity provider or physical grant.
    let mut wire = serde_json::to_value(
        super::super::super::recording_commands::recording_proposal_for_test(2, 10),
    )
    .unwrap();
    for descriptor in wire["participants"]["descriptors"].as_array_mut().unwrap() {
        if descriptor["source_fence"].is_object() {
            descriptor["source_fence"]["root_identity"] = request.media_root().identity().into();
            descriptor["source_fence"]["database_identity"] =
                request.database_file_identity().into();
            descriptor["source_fence"]["lease_epoch"] = epoch.into();
        }
    }
    wire["destination"]["parent"]["expectation"]["filesystem_identity"] =
        request.media_root().identity().into();
    let valid = RecordingProposal::decode(&serde_json::to_vec(&wire).unwrap()).unwrap();
    assert!(matches_original_destination(&valid, &request, epoch));

    for (field, value) in [
        ("root_identity", Value::from("foreign-root")),
        ("database_identity", Value::from("foreign-database")),
        ("lease_epoch", Value::from(epoch.checked_add(1).unwrap())),
    ] {
        let mut changed = wire.clone();
        // The destination Source differs; the unrelated Harvest/Projection/Readiness
        // descriptors still match, reproducing the original any-participant predicate.
        changed["participants"]["descriptors"][0]["source_fence"][field] = value.clone();
        if field == "root_identity" {
            changed["destination"]["parent"]["expectation"]["filesystem_identity"] = value;
        }
        let changed = RecordingProposal::decode(&serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(
            changed
                .participants()
                .descriptors()
                .iter()
                .filter_map(|d| d.source_fence())
                .any(|f| {
                    f.root_identity() == request.media_root().identity()
                        && f.database_identity() == request.database_file_identity()
                        && f.lease_epoch() == epoch
                })
        );
        assert!(
            !matches_original_destination(&changed, &request, epoch),
            "{field}"
        );
        assert_eq!(
            RecordingProposal::decode(&changed.encode().unwrap()).unwrap(),
            changed
        );
    }
    // A second required SourceDatabase A matches the genuine request. Destination B
    // has a different valid source/root/database/epoch: A must not authorize B.
    let media_b = tempfile::tempdir().unwrap();
    let source_b = crate::sample_sources::SourceDatabase::open_for_catalog_with_profile_guard(
        media_b.path(),
        &guard,
        "recording-held-destination-b",
    )
    .unwrap();
    let authority_b = source_b.prepare_source_authority().unwrap();
    let envelope_b = authority_b.journal_request().unwrap();
    let request_b = envelope_b.request;
    let _confirmation_b = request_b
        .accept_confirmation(envelope_b.confirmation)
        .unwrap();
    let epoch_b = u64::try_from(request_b.lease_identity().epoch()).unwrap();
    let mut two_sources = wire.clone();
    let mut source_a = two_sources["participants"]["descriptors"][0].clone();
    let source_a_id = ParticipantId::new();
    source_a["id"] = serde_json::to_value(source_a_id).unwrap();
    source_a["source_fence"]["source_id"] = Value::from("source-a");
    two_sources["participants"]["descriptors"]
        .as_array_mut()
        .unwrap()
        .push(source_a);
    let checkpoint =
        ParticipantCheckpoint::new(source_a_id, ParticipantCheckpointState::Pending, None, None)
            .unwrap();
    two_sources["participants"]["checkpoints"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(checkpoint).unwrap());
    let destination_b = &mut two_sources["participants"]["descriptors"][0]["source_fence"];
    destination_b["source_id"] = Value::from("source-b");
    destination_b["root_identity"] = request_b.media_root().identity().into();
    destination_b["database_identity"] = request_b.database_file_identity().into();
    destination_b["lease_epoch"] = Value::from(epoch_b);
    two_sources["destination"]["parent"]["source_id"] = Value::from("source-b");
    two_sources["destination"]["parent"]["expectation"]["filesystem_identity"] =
        request_b.media_root().identity().into();
    let two_sources =
        RecordingProposal::decode(&serde_json::to_vec(&two_sources).unwrap()).unwrap();
    assert!(two_sources.participants().descriptors().iter().any(|d| {
        d.kind() == ParticipantKind::SourceDatabase
            && d.id() == source_a_id
            && d.source_fence().is_some_and(|f| {
                f.root_identity() == request.media_root().identity()
                    && f.database_identity() == request.database_file_identity()
                    && f.lease_epoch() == epoch
            })
    }));
    assert!(!matches_original_destination(&two_sources, &request, epoch));
    assert!(matches_original_destination(
        &two_sources,
        &request_b,
        epoch_b
    ));
    assert!(!matches_original_destination(
        &valid,
        &request,
        epoch.checked_add(1).unwrap()
    ));
}
