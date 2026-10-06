//! Fixed pacing geometry supplements the genuine idle worker regression.
use super::*;

#[test]
fn original_catalog_idle_pacing_excludes_closing_and_parked_preparations() {
    let start = Instant::now();
    let mut pacing = Pacing::new(start);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let admission = Mutex::new(AdmissionState::new());
    let sources = HashMap::new();
    assert!(matches!(
        pacing.poll(start + INTERVAL, &receive, &admission, &sources, true),
        Poll::Wait
    ));
    assert_eq!(pacing.since, start + INTERVAL);
    assert!(matches!(
        pacing.poll(
            start + INTERVAL + Duration::from_millis(25),
            &receive,
            &admission,
            &sources,
            false
        ),
        Poll::Wait
    ));
    admission.lock().unwrap().shutdown = true;
    assert!(matches!(
        pacing.poll(start + INTERVAL * 3, &receive, &admission, &sources, false),
        Poll::Wait
    ));
    assert_eq!(pacing.since, start + INTERVAL);
    drop(send);
}

#[test]
fn original_catalog_idle_pacing_paces_empty_maps_and_detects_disconnect() {
    let start = Instant::now();
    let mut pacing = Pacing::new(start);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let admission = Mutex::new(AdmissionState::new());
    let sources = HashMap::new();
    assert!(matches!(
        pacing.poll(start + INTERVAL, &receive, &admission, &sources, false),
        Poll::Wait
    ));
    assert_eq!(pacing.since, start + INTERVAL);
    assert_eq!(pacing.next, 0);
    drop(send);
    assert!(matches!(
        pacing.poll(start + INTERVAL * 2, &receive, &admission, &sources, false),
        Poll::Disconnected
    ));
}

#[test]
fn original_catalog_idle_pacing_prefers_queued_command_under_original_submission_gate() {
    let start = Instant::now();
    let mut pacing = Pacing::new(start);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let admission = Mutex::new(AdmissionState::new());
    let sources = HashMap::new();
    send.send(QueuedCatalogCommand {
        command: super::super::CatalogCommand::RequestAudit {
            source_id: SourceId::from_string("original-queued"),
            lifecycle_generation: 7,
        },
        sent_at: start,
    })
    .unwrap();
    admission.lock().unwrap().queued_commands = 1;
    let Poll::Command(packet) =
        pacing.poll(start + INTERVAL, &receive, &admission, &sources, false)
    else {
        panic!("queued original command must take precedence");
    };
    assert!(matches!(
        packet.command,
        super::super::CatalogCommand::RequestAudit {
            lifecycle_generation: 7,
            ..
        }
    ));
    assert_eq!(pacing.since, start);
    assert_eq!(pacing.next, 0);
}
