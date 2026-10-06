//! Resource-required production bootstrap and genuine journal registration.
//! This body is typechecked on all test targets; no test-capacity startup or forged receipt.
use super::*;
use crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs;
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{
    FullCapacityAllocation, FullCapacityAllocationClass as Class, FullCapacityPlan, ParticipantId,
};
use crate::sample_sources::db::{
    SourceRegistryReferenceExpected, SourceRegistryReferenceProposal,
    SourceRegistryReferenceTransition as Transition,
};
use crate::sample_sources::library::{
    GlobalLibraryWriter, GlobalLibraryWriterStatus, GlobalPreparationRequest,
};
use crate::sample_sources::wal_transport::{SourceWalProducerOwner, SourceWalTransport};
use crate::sample_sources::{SourceDatabase, SourceWriter};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
pub(super) fn whole_journal_image(client: &OperationJournalClient) -> ([u64; 5], Option<i64>) {
    client
        .submit_control(CONTROL_INPUT_BYTES, |input, result| {
            JournalCommand::InspectRetirementWholeJournal { input, result }
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
}
#[test]
#[ignore = "requires a native qualified bootstrap, protected mounted roots and SDK/runtime; no fixture qualification"]
fn native_retirement_inventory_genuine_registered_original_consumer() {
    let fixture = fixture("native-retirement-inventory");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut journal = OperationJournalOwner::start(store).unwrap();
    wait_until_available(&journal);
    let mut global = GlobalLibraryWriter::start(&fixture.owner).unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while global.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(1));
    }
    let media = fixture._base.path().join("native-retirement-source");
    fs::create_dir(&media).unwrap();
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(
            &media,
            &fixture.owner,
            "native-retirement-source",
        )
        .unwrap(),
        SourceId::from_string("native-retirement-source"),
        1,
    )
    .unwrap();
    let (requester, mut inbox) = SourceWalTransport::channel();
    let (capture_tx, capture_rx) = std::sync::mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let client = journal.client();
    let worker = thread::spawn(move || {
        let authority = writer
            .catalog_database()
            .prepare_source_authority()
            .unwrap();
        // The actual journal installs roots/reserve/source custody and consumes the original
        // confirmation. No local accept_confirmation or fixture provider is used.
        let receipt = client
            .register_source_authority(authority.journal_request().unwrap())
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let reference = writer
            .capture_registry_reference_observation(Some(&authority))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let allocation = writer
            .capture_lease_maintenance_allocation(deadline)
            .unwrap();
        let prepared = writer
            .prepare_lease_maintenance(allocation.clone(), deadline)
            .unwrap();
        let binding = prepared.capture_lease_binding(&authority).unwrap();
        let fence = binding.stored_fence().clone();
        let mut writer = Some(prepared.abort().unwrap());
        drop(binding);
        capture_tx
            .send((allocation, fence, deadline, reference))
            .unwrap();
        let mut held: Option<SourceWalProducerOwner> = None;
        let mut reused = false;
        loop {
            if stopped.load(Ordering::Acquire) {
                inbox.close();
            }
            while let Some(command) = inbox.try_next_any() {
                let Some(original) = writer.take() else {
                    command.reject(
                        crate::sample_sources::wal_transport::SourceWalTransportRejection::Busy,
                    );
                    continue;
                };
                let mut owner = command.acquire(original);
                if let Err(reason) = owner.publish_prepared(Some(&authority)) {
                    owner.request_abort(Some(reason));
                }
                assert!(held.replace(owner).is_none());
            }
            if let Some(mut owner) = held.take() {
                if stopped.load(Ordering::Acquire)
                    || owner.is_cancelled()
                    || !owner.is_prepared()
                    || owner.rejection().is_some()
                    || Instant::now() >= owner.deadline()
                    || owner.revalidate(Some(&authority)).is_err()
                {
                    owner.request_abort(None);
                    match owner.into_writer() {
                        Ok(original) => {
                            writer = Some(original);
                            reused = true;
                        }
                        Err(original) => held = Some(original),
                    }
                } else {
                    held = Some(owner);
                }
            }
            if stopped.load(Ordering::Acquire) && held.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            reused,
            "actual Source writer must be restored by original NoAttempt cleanup"
        );
        drop(writer);
        drop(authority);
        drop(receipt);
    });
    let (allocation, fence, deadline, reference) =
        capture_rx.recv_timeout(Duration::from_secs(7)).unwrap();
    let source_id = ParticipantId::new();
    let global_id = ParticipantId::new();
    let operation = OperationId::new();
    // Frozen metadata expectations, never proof that a global transition occurred.
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
        source_id,
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
        .map(|d| {
            ParticipantCheckpoint::new(d.id(), ParticipantCheckpointState::Pending, None, None)
                .unwrap()
        })
        .collect();
    let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
    let args = UniversalCommandArgs::SourceRegistryRetirement(retirement.clone());
    let request =
        GlobalPreparationRequest::new(global.preparation_owner(), [9; 32], deadline, 1, 16 * 1024)
            .unwrap()
            .with_lifecycle_policy();
    let mut pending = global.submit_preparation(request).unwrap();
    let probe = pending
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let global_allocation = probe.observation().lifecycle_allocation().unwrap();
    let source_caps = fence.write_ceilings();
    let global_caps = [
        global_allocation.main(),
        global_allocation.wal(),
        global_allocation.shm(),
    ]
    .map(|o| o.write_ceiling());
    let global_device = global_allocation.main().device();
    let global_unit = global_allocation.main().allocation_unit();
    let allocations = [
        (
            Class::SourceDatabaseCommit,
            fence.device(),
            fence.allocation_unit(),
            source_caps[0],
        ),
        (
            Class::SourceWalShm,
            fence.device(),
            fence.allocation_unit(),
            source_caps[1].checked_add(source_caps[2]).unwrap(),
        ),
        (
            Class::GlobalDatabaseCommit,
            global_device,
            global_unit,
            global_caps[0].checked_mul(2).unwrap(),
        ),
        (
            Class::GlobalWalShm,
            global_device,
            global_unit,
            global_caps[1]
                .checked_add(global_caps[2])
                .unwrap()
                .checked_mul(2)
                .unwrap(),
        ),
        (
            Class::JournalRecord,
            global_device,
            global_unit,
            global_unit.checked_mul(2).unwrap(),
        ),
    ]
    .into_iter()
    .map(|(class, device, unit, bytes)| {
        FullCapacityAllocation::new(
            VolumeIdentity { device },
            unit,
            class,
            bytes,
            bytes,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap()
    })
    .collect();
    let plan = FullCapacityPlan::new(allocations).unwrap();
    let claims = AdmissionClaimInventory::new(
        plan.allocations()
            .iter()
            .map(|a| {
                AdmissionClaim::new(
                    AdmissionClaimId::new(),
                    a.volume().clone(),
                    a.class(),
                    a.allocated_bytes(),
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    probe
        .abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    drop(pending);
    let main = claims
        .claims()
        .iter()
        .find(|c| c.allocation_class() == Class::SourceDatabaseCommit)
        .unwrap()
        .claim_id();
    let side = claims
        .claims()
        .iter()
        .find(|c| c.allocation_class() == Class::SourceWalShm)
        .unwrap()
        .claim_id();
    let input = WalPreparationInput::from_universal(
        &args,
        &participants,
        &plan,
        &claims,
        vec![WalPreparationSource::new(
            requester,
            source_id,
            main,
            side,
            SourceWalPreparationInput::Lease(allocation),
        )],
        Some(global.wal_preparation_client()),
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
    let context = loop {
        match handle.status() {
            WalPreparationStatus::Ready(view) => break view,
            WalPreparationStatus::Preparing => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            other => panic!("original native preparation refused: {other:?}"),
        }
    };
    let prepared = PreparedAdmissionInput::try_new(
        OperationIntent {
            actor: crate::operation_journal::model::OperationActor::User,
            kind: crate::operation_journal::model::OperationKind::SourceRegistryRetirement,
            label: "Native original retirement inventory".into(),
        },
        UniversalIntent::new(
            None,
            crate::operation_journal::model::UniversalCommandKind::SourceRegistryRetirement,
            "operation.status.source_registry_retirement",
            participants.clone(),
        )
        .unwrap(),
        args,
        participants,
        plan,
        claims.clone(),
        1,
    )
    .unwrap();
    let validation_id = prepared.validation_operation_id();
    let record = prepared_record(validation_id, &prepared).unwrap();
    let whole_before = whole_journal_image(&journal.client());
    let ids_before = journal
        .prepared_admission_id_allocations
        .load(Ordering::Relaxed);
    let result = journal
        .client()
        .admit_prepared(prepared)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(
        matches!(result,Err(OperationJournalOwnerError::PreparedAdmission(PreparedAdmissionError::Unsupported{ref reason})) if reason.contains("original owners acknowledged")),
        "{result:?}"
    );
    let inventory = handle
        .retirement_inventory_for_tests()
        .expect("genuine native original ACK must produce historical observation");
    assert_eq!(inventory.inventory().bindings().len(), 2);
    assert_eq!(inventory.inventory().slices().len(), 6);
    assert_eq!(whole_journal_image(&journal.client()), whole_before);
    assert_eq!(
        journal
            .prepared_admission_id_allocations
            .load(Ordering::Relaxed),
        ids_before
    );
    for absent in [operation, validation_id] {
        assert!(
            journal
                .client()
                .get(absent)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .is_none()
        );
    }
    let outside = rusqlite::Connection::open(
        fixture
            .owner
            .profile_root()
            .join(crate::sample_sources::library::LIBRARY_DB_FILE_NAME),
    )
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
    loop {
        match handle.status() {
            WalPreparationStatus::Aborted => break,
            WalPreparationStatus::Aborting | WalPreparationStatus::Ready(_) => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            other => panic!("native original cleanup unconfirmed: {other:?}"),
        }
    }
    drop(handle);
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(global.shutdown());
    // Registered-source lifecycle can conservatively deny full clean handoff; memory
    // release after the genuine worker join does not waive that refusal or settle it.
    let _clean_handoff = journal.shutdown();
    inventory.inventory().validate_record(&record).unwrap();
    inventory
        .inventory()
        .validate_against(record.full_capacity_plan.as_ref().unwrap(), &claims)
        .unwrap();
    let canonical = crate::operation_journal::database_claims::command_bytes(&record).unwrap();
    let mut streamed = Vec::new();
    retirement
        .write_codec_normalized_command(&mut streamed)
        .unwrap();
    assert_eq!(streamed, canonical);
    let context_alias = context.clone();
    native_concurrent_drop(context, context_alias);
    assert!(budget.used() > 0);
    let alias = inventory.clone();
    native_concurrent_drop(inventory, alias);
    assert_eq!(budget.used(), 0);
    drop(budget);
    assert_eq!(
        *journal.shared.input_bytes.lock().unwrap(),
        InputBudget::new()
    );
}
fn native_concurrent_drop<T: Send + 'static>(a: T, b: T) {
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = Arc::clone(&barrier);
    let first = thread::spawn(move || {
        barrier.wait();
        drop(a);
    });
    let second = thread::spawn(move || {
        other.wait();
        drop(b);
    });
    first.join().unwrap();
    second.join().unwrap();
}
