use super::super::{OperationJournalClient, OwnerCommandReceivers, OwnerLifecycle, OwnerShared};
use super::*;
use crate::operation_journal::OperationId;
fn client() -> (OperationJournalClient, OwnerCommandReceivers) {
    let (senders, receivers) = OwnerCommandReceivers::channel();
    let shared = Arc::new(OwnerShared::new(senders));
    shared
        .lifecycle
        .store(OwnerLifecycle::Available as u8, Ordering::Release);
    (OperationJournalClient { shared }, receivers)
}
#[test]
fn original_journal_queue_dispatch_and_receiver_drop_account_once() {
    let (client, receivers) = client();
    let first = client.get(OperationId::new()).unwrap();
    let lost = client.get(OperationId::new()).unwrap();
    drop(lost);
    assert_eq!(client.queue_metrics_snapshot().unwrap().ordinary.backlog, 2);
    drop(receivers.recv().unwrap());
    let middle = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(
        (
            middle.accepted,
            middle.dequeued,
            middle.abandoned,
            middle.backlog
        ),
        (2, 1, 0, 1)
    );
    assert_eq!(middle.dispatch_wait_buckets.iter().sum::<u64>(), 1);
    drop(receivers);
    let after = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(
        (
            after.accepted,
            after.dequeued,
            after.abandoned,
            after.backlog
        ),
        (2, 1, 1, 0)
    );
    drop(first);
    assert_eq!(client.queue_metrics_snapshot().unwrap().ordinary, after);
}
#[test]
fn original_journal_queue_full_and_disconnected_never_count_as_accepted() {
    let (client, receivers) = client();
    let mut replies = Vec::new();
    for _ in 0..super::super::ORDINARY_COMMAND_CAPACITY {
        replies.push(client.get(OperationId::new()).unwrap());
    }
    assert!(matches!(
        client.get(OperationId::new()),
        Err(super::super::OperationJournalOwnerQueueError::Full)
    ));
    let before = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(before.backlog as usize, replies.len());
    drop(receivers);
    drop(replies);
    assert!(matches!(
        client.get(OperationId::new()),
        Err(super::super::OperationJournalOwnerQueueError::Closed)
    ));
    let after = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(after.accepted, before.accepted);
    assert_eq!(after.abandoned, before.accepted);
    assert_eq!(after.backlog, 0);
}

#[test]
fn original_journal_queue_control_priority_and_nonblocking_snapshot() {
    let (client, receivers) = client();
    let ordinary = client.get(OperationId::new()).unwrap();
    let control = client
        .request_cancel_before_publish(
            OperationId::new(),
            1,
            crate::operation_journal::CancellationRequestObservation::new(56).unwrap(),
        )
        .unwrap();
    let before = client.queue_metrics_snapshot().unwrap();
    assert_eq!((before.ordinary.backlog, before.control.backlog), (1, 1));
    drop(receivers.recv().unwrap());
    let after = client.queue_metrics_snapshot().unwrap();
    assert_eq!((after.ordinary.dequeued, after.control.dequeued), (0, 1));
    assert_eq!((after.ordinary.backlog, after.control.backlog), (1, 0));
    {
        let _accounting = client.shared.queue_metrics.0.lock().unwrap();
        assert!(client.queue_metrics_snapshot().is_none());
    }
    drop(receivers);
    assert_eq!(
        client.queue_metrics_snapshot().unwrap().ordinary.abandoned,
        1
    );
    drop((ordinary, control));
}

#[test]
fn original_journal_queue_actual_send_receive_race_accounts_every_ticket() {
    let (client, receivers) = client();
    let consumer = std::thread::spawn(move || {
        for _ in 0..2_000 {
            drop(receivers.recv().unwrap());
        }
    });
    for _ in 0..2_000 {
        loop {
            match client.get(OperationId::new()) {
                Ok(reply) => {
                    drop(reply);
                    break;
                }
                Err(super::super::OperationJournalOwnerQueueError::Full) => {
                    std::thread::yield_now()
                }
                Err(error) => panic!("unexpected original queue error: {error}"),
            }
        }
    }
    consumer.join().unwrap();
    let after = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(
        (
            after.accepted,
            after.dequeued,
            after.abandoned,
            after.backlog
        ),
        (2_000, 2_000, 0, 0)
    );
    assert!(after.peak_backlog <= super::super::ORDINARY_COMMAND_CAPACITY as u64 + 1);
    assert_eq!(after.dispatch_wait_buckets.iter().sum::<u64>(), 2_000);
}

#[test]
fn original_journal_queue_real_boundaries_saturate_fixed_counters() {
    let (client, receivers) = client();
    {
        let mut state = client.shared.queue_metrics.0.lock().unwrap();
        state[0].accepted = u64::MAX;
        state[0].dequeued = u64::MAX;
        state[0].abandoned = u64::MAX;
        state[0].peak_backlog = u64::MAX;
        state[0].dispatch_wait_buckets = [u64::MAX; 5];
    }
    let first = client.get(OperationId::new()).unwrap();
    drop(receivers.recv().unwrap());
    let second = client.get(OperationId::new()).unwrap();
    drop(receivers);
    let after = client.queue_metrics_snapshot().unwrap().ordinary;
    assert_eq!(
        (
            after.accepted,
            after.dequeued,
            after.abandoned,
            after.backlog
        ),
        (u64::MAX, u64::MAX, u64::MAX, 0)
    );
    assert_eq!(after.dispatch_wait_buckets, [u64::MAX; 5]);
    assert!(std::mem::size_of::<QueueTicket>() <= 64);
    assert!(std::mem::size_of::<QueueMetrics>() <= 256);
    drop((first, second));
}
