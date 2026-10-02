use crate::operation_journal::{JournalRetentionKind, JournalStatusFamily, RecoveryStatusOutcome};

#[test]
fn recovery_status_mixed_families_redact_retained_keys_and_survive_reopen() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let legacy = intent_record(OperationId::new());
    store.insert_intent(&legacy).unwrap();
    let full = full_progress_fixture();
    let full_id = full.snapshot().intent().operation_id();
    insert_full_raw(
        &store,
        &full_id.to_string(),
        2,
        &encode_full_progress(&full).unwrap(),
    );
    insert_full_raw(
        &store,
        "private/path\ninvalid-sql-key",
        3,
        b"secret-record-bytes",
    );
    let page = store.scan_recovery_status_page(None).unwrap();
    assert_eq!(page.entries().len(), 3);
    assert_eq!(page.entries()[0].family(), JournalStatusFamily::Legacy);
    assert_eq!(page.entries()[0].operation_id(), Some(legacy.operation_id));
    assert_eq!(
        page.entries()[0].outcome(),
        RecoveryStatusOutcome::Legacy {
            phase: legacy.phase,
            disposition: legacy.disposition,
        }
    );
    assert_eq!(
        page.entries()[1].family(),
        JournalStatusFamily::FullProgress
    );
    assert_eq!(page.entries()[1].operation_id(), Some(full_id));
    assert_eq!(
        page.entries()[1].outcome(),
        RecoveryStatusOutcome::FullProgress {
            phase: full.snapshot().phase(),
            disposition: full.snapshot().disposition(),
        }
    );
    assert_eq!(page.entries()[2].operation_id(), None);
    assert_eq!(
        page.entries()[2].outcome(),
        RecoveryStatusOutcome::Retained(JournalRetentionKind::TypeInvalid)
    );
    let debug = format!("{page:?}");
    for secret in [
        "private/path",
        "invalid-sql-key",
        "secret-record-bytes",
        "capacity_plan",
        "payload",
    ] {
        assert!(!debug.contains(secret), "compact page leaked {secret}");
    }
    assert!(page.is_done());
    assert!(page.encoded_bytes() <= MAX_RECOVERY_BYTES_PER_PAGE);
    drop(store);
    let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(reopened.scan_recovery_status_page(None).unwrap(), page);
}

#[test]
fn recovery_status_pages_preserve_combined_cursor_and_reject_ambiguous_sequences() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    for _ in 0..MAX_RECOVERY_ROWS_PER_PAGE {
        store
            .insert_intent(&intent_record(OperationId::new()))
            .unwrap();
    }
    let full = full_progress_fixture();
    insert_full_raw(
        &store,
        &full.snapshot().intent().operation_id().to_string(),
        17,
        &encode_full_progress(&full).unwrap(),
    );
    let first = store.scan_recovery_status_page(None).unwrap();
    assert_eq!(first.entries().len(), MAX_RECOVERY_ROWS_PER_PAGE);
    assert!(!first.is_done());
    assert_eq!(first.next_cursor().unwrap().sequence(), 16);
    let second = store
        .scan_recovery_status_page(first.next_cursor())
        .unwrap();
    assert_eq!(second.entries().len(), 1);
    assert_eq!(second.entries()[0].sequence(), 17);
    assert_eq!(
        second.entries()[0].family(),
        JournalStatusFamily::FullProgress
    );
    assert!(second.is_done());
    assert!(second.next_cursor().is_none());
    // Global sequence ambiguity must fail the entire page rather than dropping a family.
    insert_full_raw(&store, &OperationId::new().to_string(), 1, b"{}");
    assert!(matches!(
        store.scan_recovery_status_page(None),
        Err(RecoveryScanError::RequiresAudit { .. })
    ));
}

#[test]
fn recovery_status_retains_cross_family_identity_and_corrupt_payload() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let full = full_progress_fixture();
    let id = full.snapshot().intent().operation_id();
    store.insert_intent(&intent_record(id)).unwrap();
    insert_full_raw(
        &store,
        &id.to_string(),
        2,
        &encode_full_progress(&full).unwrap(),
    );
    insert_full_raw(&store, &OperationId::new().to_string(), 3, b"{broken");
    let page = store.scan_recovery_status_page(None).unwrap();
    assert_eq!(page.entries().len(), 3);
    assert!(
        page.entries().iter().all(
            |e| e.outcome() == RecoveryStatusOutcome::Retained(JournalRetentionKind::Malformed)
        )
    );
}

#[test]
fn recovery_status_preserves_guarded_terminal_dispositions_without_publication_aliases() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let snapshots =
        crate::operation_journal::work_progress::test_guarded_terminal_store_snapshots();
    for (index, snapshot) in snapshots.iter().enumerate() {
        let record = StoredFullProgressRecord::new(snapshot.clone()).unwrap();
        insert_full_raw(
            &store,
            &snapshot.intent().operation_id().to_string(),
            index as i64 + 1,
            &encode_full_progress(&record).unwrap(),
        );
    }
    let page = store.scan_recovery_status_page(None).unwrap();
    assert_eq!(page.entries().len(), snapshots.len());
    for (entry, snapshot) in page.entries().iter().zip(&snapshots) {
        assert_eq!(
            entry.outcome(),
            RecoveryStatusOutcome::FullProgress {
                phase: snapshot.phase(),
                disposition: snapshot.disposition(),
            }
        );
    }
    assert!(page.entries().iter().any(|entry| matches!(
        entry.outcome(),
        RecoveryStatusOutcome::FullProgress {
            disposition: crate::operation_journal::FullProgressDisposition::FailedDataLossRisk,
            ..
        }
    )));
}
