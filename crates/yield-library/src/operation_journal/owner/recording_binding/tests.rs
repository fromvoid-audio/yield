//! Actual profile/journal/source refusal checks. Unissued receipts are adversarial inputs only;
//! they never install a registry entry or qualify a physical recording route.
use super::*;
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
        InputBudget::new()
    );
}
