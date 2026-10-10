//! Actual original worker component, never a qualified startup or admission shortcut.
use super::*;

fn install_startup_fixture(
    store: &OperationJournalStore,
    rows: usize,
    volume: VolumeIdentity,
    unit: u64,
) {
    let operation = OperationId::new();
    let wire = format!(
        r#"{{"schema_version":1,"operation_id":"{operation}","intent":{{"actor":"User","kind":"FileHistory","label":"startup fixture"}},"phase":"IntentDurable","disposition":"None","payload":null,"created_unix_ms":1,"updated_unix_ms":1}}"#
    );
    store
        .replace_historical_row_fixture_for_test(operation, 1, wire.as_bytes())
        .unwrap();
    store
        .install_ordinary_collection_volume_fixture_for_test(operation, rows, false, volume, unit)
        .unwrap();
}

#[test]
fn capacity_startup_actual_worker_component_preserves_original_bytes_and_exact_refunds() {
    for pressure in [false, true] {
        let fixture = fixture("capacity-startup-worker-component");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        install_startup_fixture(&store, 64, VolumeIdentity { device: 1 }, 4096);
        let original = store.rebuild_ordinary_claim_snapshot().unwrap();
        let (commands, receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        shared.startup_component.store(true, Ordering::Release);
        let baseline = *shared.input_bytes.lock().unwrap();
        let pool = shared.input_bytes.clone();
        let ids = Arc::new(AtomicUsize::new(0));
        let copies = ids.clone();
        let scratch = super::super::super::capacity_provider::funded_startup_header_for_test();
        let available = baseline.remaining[0];
        let held = pressure.then(|| {
            shared
                .reserve_input(
                    InputClass::Ordinary,
                    available
                        - resource_custody::HEADER_BYTES
                        - std::mem::size_of::<InputPermit>()
                        - HISTORICAL_REPLY_BYTES
                        - crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES
                        - scratch
                        + 1,
                )
                .unwrap()
        });
        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || {
            run_owner(
                store,
                receivers,
                worker_shared,
                None,
                false,
                false,
                None,
                false,
                None,
                copies,
                Arc::new(AtomicUsize::new(0)),
            )
        });
        worker.join().expect("original startup worker join");
        assert_eq!(ids.load(Ordering::Acquire), 0);
        assert_eq!(
            shared.lifecycle.load(Ordering::Acquire),
            OwnerLifecycle::Unavailable as u8
        );
        if pressure {
            assert_eq!(
                shared.startup_component_claims.load(Ordering::Acquire),
                usize::MAX
            );
            assert_eq!(
                shared.unavailable.lock().unwrap().as_deref(),
                Some(startup_credit::REFUSAL)
            );
        } else {
            assert_eq!(shared.startup_component_claims.load(Ordering::Acquire), 64);
            assert_eq!(
                shared.unavailable.lock().unwrap().as_deref(),
                Some("funded startup component verified; physical admission unsupported")
            );
        }
        let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
        assert_eq!(
            reopened.rebuild_ordinary_claim_snapshot().unwrap(),
            original
        );
        drop(reopened);
        drop(held);
        drop(shared); // exact Shared diagnostic payload drops before detached debit
        assert_eq!(*pool.lock().unwrap(), baseline);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn capacity_startup_native_production_rebuilds_original_descriptor_bound_image() {
    let fixture = fixture("capacity-startup-native-production");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let descriptor = fs::File::open(
        fixture
            .owner
            .profile_root()
            .join(crate::operation_journal::JOURNAL_DATABASE_ROOT_NAME)
            .join(crate::operation_journal::JOURNAL_DATABASE_FILE_NAME),
    )
    .unwrap();
    let facts = descriptor_capacity_facts(&descriptor).unwrap();
    install_startup_fixture(&store, 1, facts.identity, facts.allocation_unit);
    let original = store.rebuild_ordinary_claim_snapshot().unwrap();
    drop(descriptor);
    // Original production registry/bootstrap; historical fixture claim is not qualified issuance.
    let mut owner = OperationJournalOwner::start(store).unwrap();
    wait_until_available(&owner);
    assert!(!owner.shared.startup_component.load(Ordering::Acquire));
    let mut request = owner.client().inspect_capacity_for_test().unwrap();
    let snapshot = request
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.claims.len(), 1);
    assert_eq!(snapshot.claims[0].0, original.claims()[0].claim_id());
    assert_eq!(&snapshot.claims[0].1, original.claims()[0].volume());
    assert_eq!(
        snapshot.claims[0].2,
        original.claims()[0].allocation_class()
    );
    assert_eq!(snapshot.claims[0].3, original.claims()[0].allocated_bytes());
    assert!(owner.shutdown());
    let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(
        reopened.rebuild_ordinary_claim_snapshot().unwrap(),
        original
    );
}
