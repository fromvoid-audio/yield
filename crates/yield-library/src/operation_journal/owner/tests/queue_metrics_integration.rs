// Actual original worker/profile-owned SQLite; no physical/admission fixture authority.
use super::*;
#[test]
fn original_journal_queue_metrics_lost_reply_still_dispatches_and_retains_actual_record() {
    let fixture = fixture("journal-queue-metrics-lost");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let (mut owner, ready, release) =
        OperationJournalOwner::start_with_command_gate_for_test(store);
    wait_until_available(&owner);
    let client = owner.client();
    let operation = OperationId::new();
    let request = client.persist_intent(intent_record(operation)).unwrap();
    ready.wait();
    drop(request);
    let mut read = client.get(operation).unwrap();
    let before = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(
        (before.accepted, before.dequeued, before.backlog),
        (2, 1, 1)
    );
    release.wait();
    assert_eq!(
        read.recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .unwrap()
            .operation_id,
        operation
    );
    assert!(owner.shutdown());
    let after = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(
        (
            after.accepted,
            after.dequeued,
            after.abandoned,
            after.backlog
        ),
        (2, 2, 0, 0)
    );
    let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(
        reopened.get(operation).unwrap().unwrap().operation_id,
        operation
    );
}
#[test]
fn original_journal_queue_metrics_worker_panic_abandons_both_original_classes() {
    let fixture = fixture("journal-queue-metrics-panic");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let (mut owner, ready, release) =
        OperationJournalOwner::start_with_worker_panic_for_test(store);
    wait_until_available(&owner);
    let client = owner.client();
    let first = client.get(OperationId::new()).unwrap();
    ready.wait();
    let second = client.get(OperationId::new()).unwrap();
    let lost = client.get(OperationId::new()).unwrap();
    drop(lost);
    let control = client
        .request_cancel_before_publish(
            OperationId::new(),
            1,
            CancellationRequestObservation::new(56).unwrap(),
        )
        .unwrap();
    release.wait();
    assert!(!owner.shutdown());
    let after = client.queue_metrics_snapshot().unwrap();
    assert_eq!(
        (
            after.ordinary.accepted,
            after.ordinary.dequeued,
            after.ordinary.abandoned,
            after.ordinary.backlog
        ),
        (3, 1, 2, 0)
    );
    assert_eq!(
        (
            after.control.accepted,
            after.control.dequeued,
            after.control.abandoned,
            after.control.backlog
        ),
        (1, 0, 1, 0)
    );
    drop((first, second, control));
}
