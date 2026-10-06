use super::*;

fn install_original_provider_image(
    store: &OperationJournalStore,
    volume: VolumeIdentity,
    allocated_bytes: u64,
) -> (
    crate::operation_journal::store::OrdinaryClaimRebuildSnapshot,
    usize,
    usize,
) {
    let operation = OperationId::new();
    let wire = format!(
        r#"{{"schema_version":1,"operation_id":"{operation}","intent":{{"actor":"User","kind":"FileHistory","label":"provider retained image fixture"}},"phase":"IntentDurable","disposition":"None","payload":null,"created_unix_ms":1,"updated_unix_ms":1}}"#
    );
    store
        .replace_historical_row_fixture_for_test(operation, 1, wire.as_bytes())
        .unwrap();
    store
        .install_ordinary_collection_volume_fixture_for_test(
            operation,
            64,
            false,
            volume,
            allocated_bytes,
        )
        .unwrap();
    assert!(store.observe_historical_claim_for_test(operation).unwrap());
    let snapshot = store.rebuild_admission_snapshot().unwrap();
    assert_eq!(snapshot.claims().len(), 64);
    assert!(!snapshot.proves_durable_zero());
    assert!(matches!(
        snapshot.readiness(),
        CapacityRebuildReadiness::Ready
    ));
    let (_, _, collection) = store
        .ordinary_collection_allocation_probe_for_test(true)
        .unwrap();
    let graphs = CapacityProvider::reconciliation_debit_for_test(&snapshot);
    (snapshot, collection, graphs)
}

#[test]
fn provider_reconciliation_actual_queued_component_funds_before_graph_and_never_admits() {
    for pressure in [false, true] {
        let fixture = fixture("provider-reconciliation-original-queue");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let (original, collection, graphs) =
            install_original_provider_image(&store, VolumeIdentity { device: 1 }, 1);
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        owner
            .shared
            .provider_reconciliation_component
            .store(true, Ordering::Release);
        let prepared = prepared_copy_input(
            crate::operation_journal::FullCapacityAllocationClass::DestinationStaging,
            VolumeIdentity { device: 1 },
            1,
        );
        let request_bytes = bounded_prepared_admission_input(&prepared).unwrap();
        let available = owner.shared.input_bytes.lock().unwrap().remaining[0];
        let held = pressure.then(|| {
            owner
                .shared
                .reserve_input(
                    InputClass::Ordinary,
                    available
                        - request_bytes
                        - HISTORICAL_REPLY_BYTES
                        - crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES
                        - collection
                        - graphs
                        + 1,
                )
                .unwrap()
        });
        let mut request = owner.client().admit_prepared(prepared).unwrap();
        let error = request
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        let reason = error.to_string();
        if pressure {
            assert!(
                reason.contains("capacity reconciliation resource envelope exceeded"),
                "{reason}"
            );
        } else {
            assert!(
                reason
                    .contains("funded provider component verified; physical admission unsupported"),
                "{reason}"
            );
        }
        assert_eq!(owner.prepared_admission_id_allocations(), 0);
        drop(error);
        drop(request);
        drop(held);
        assert!(owner.shutdown());
        assert_eq!(
            owner.shared.input_bytes.lock().unwrap().remaining[0],
            InputClass::Ordinary.limit()
        );
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        assert_eq!(store.scan_recovery_page(None).unwrap().entries().len(), 1);
        assert_eq!(store.rebuild_admission_snapshot().unwrap(), original);
    }
}

#[cfg(target_os = "macos")]
fn native_original_queue_pressure(production_bootstrap: bool) {
    let fixture = fixture("provider-reconciliation-native-original-queue");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let journal_descriptor = fs::File::open(
        fixture
            .owner
            .profile_root()
            .join(crate::operation_journal::JOURNAL_DATABASE_ROOT_NAME)
            .join(crate::operation_journal::JOURNAL_DATABASE_FILE_NAME),
    )
    .unwrap();
    let original_facts = descriptor_capacity_facts(&journal_descriptor).unwrap();
    let (original, collection, graphs) = install_original_provider_image(
        &store,
        original_facts.identity,
        original_facts.allocation_unit,
    );
    drop(journal_descriptor);
    let mut owner = if production_bootstrap {
        OperationJournalOwner::start(store).unwrap()
    } else {
        let registry = fixture_volume_registry(&fixture);
        OperationJournalOwner::start_with_capacity_fixture_for_test(store, registry).unwrap()
    };
    wait_until_available(&owner);
    assert!(
        !owner
            .shared
            .provider_reconciliation_component
            .load(Ordering::Acquire)
    );
    let reserve = fixture
        .owner
        .profile_root()
        .join("operation_journal")
        .join(crate::operation_journal::capacity_reserve_namespace::RESERVE_FILE_NAME);
    let descriptor = fs::File::open(reserve).unwrap();
    let facts = descriptor_capacity_facts(&descriptor).unwrap();
    let prepared = prepared_copy_input(
        crate::operation_journal::FullCapacityAllocationClass::DestinationStaging,
        facts.identity,
        facts.allocation_unit,
    );
    let request_bytes = bounded_prepared_admission_input(&prepared).unwrap();
    let available = owner.shared.input_bytes.lock().unwrap().remaining[0];
    let held = owner
        .shared
        .reserve_input(
            InputClass::Ordinary,
            available
                - request_bytes
                - HISTORICAL_REPLY_BYTES
                - crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES
                - collection
                - graphs
                + 1,
        )
        .unwrap();
    let mut request = owner.client().admit_prepared(prepared).unwrap();
    let error = request
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("capacity reconciliation resource envelope exceeded"),
        "{error}"
    );
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    drop(error);
    drop(request);
    drop(held);
    drop(descriptor);
    assert!(owner.shutdown());
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        InputClass::Ordinary.limit()
    );
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(store.scan_recovery_page(None).unwrap().entries().len(), 1);
    assert_eq!(store.rebuild_admission_snapshot().unwrap(), original);
}

#[cfg(target_os = "macos")]
#[test]
fn provider_reconciliation_native_fixture_original_queue_refuses_exact_graph_pressure() {
    native_original_queue_pressure(false);
}

#[cfg(target_os = "macos")]
#[test]
fn provider_reconciliation_native_production_original_queue_refuses_exact_graph_pressure() {
    native_original_queue_pressure(true);
}
