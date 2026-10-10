//! Actual loopback HTTP -> selection -> real verifier -> serial retained owner, with private keys.
use super::super::tests::{Fixture, packed, python_zip};
use super::*;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};
const TOKEN: &str = "owner-wire-secret +/% token";
fn error<T>(result: Result<T, SelectionError>) -> SelectionError {
    match result {
        Ok(_) => panic!("unexpected accepted attempt"),
        Err(e) => e,
    }
}
fn http(bytes: &[u8], mime: &str) -> Vec<u8> {
    let mut response=format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).into_bytes();
    response.extend_from_slice(bytes);
    response
}
fn records(f: &Fixture) -> Vec<Vec<u8>> {
    let selected = &f.selected;
    let files=selected.release.files.iter().map(|file|json!({"name":file.name,"sha256":file.sha256.iter().map(|b|format!("{b:02x}")).collect::<String>(),"size_bytes":file.size_bytes,"url":file.url.as_str()})).collect::<Vec<_>>();
    let catalog=serde_json::to_vec(&json!({"releases":[{"build_id":selected.release.build_id,"version":selected.release.version,"build_number":selected.release.build_number,
        "commit":selected.release.commit.iter().map(|b|format!("{b:02x}")).collect::<String>(),"channel":crate::update_catalog::channel_name(selected.channel),"files":files}]})).unwrap();
    let gate =
        serde_json::to_vec(&json!({"build_id":selected.release.build_id,"download_token":TOKEN}))
            .unwrap();
    vec![
        http(&catalog, "application/json"),
        http(&gate, "application/json"),
        http(&f.checksum, "text/plain"),
        http(&f.signature, "application/pgp-signature"),
        http(&f.archive, "application/zip"),
    ]
}
fn read_request(socket: &mut TcpStream) -> String {
    // Darwin can inherit the listener's nonblocking mode. The fixture's existing read timeout
    // bounds blocking IO; it must not fail merely because the client's headers are not ready yet.
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    while !bytes.ends_with(b"\r\n\r\n") {
        let n = socket.read(&mut chunk).unwrap();
        assert!(n > 0 && bytes.len() + n <= 24 * 1024);
        bytes.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(bytes).unwrap()
}
fn server(responses: Vec<Vec<u8>>) -> (protocol::Transport, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(s) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing owner request");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("fixture accept {e}"),
                }
            };
            requests.push(read_request(&mut socket));
            let _ = socket.write_all(&response);
        }
        requests
    });
    (protocol::Transport::Loopback(address), handle)
}
fn fixture_service(
    f: &Fixture,
    transport: protocol::Transport,
    engine: Arc<engine::Engine>,
) -> PayloadStagingService {
    PayloadStagingService::new(
        f.selected.installed.clone(),
        transport,
        engine,
        Key::Fixture(f.key.verifying_key()),
    )
}
fn wait(attempt: &PayloadStageAttempt, phase: PayloadStagePhase) -> PayloadStageObservation {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = attempt.observe().unwrap();
        if state.phase == phase {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "phase {:?} never reached from {:?}",
            phase,
            state.phase
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn admitted(engine: &engine::Engine) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while engine.busy.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "actual owner did not release admission"
        );
        thread::yield_now();
    }
}
fn witness(f: &Fixture) -> Arc<TestWitness> {
    Arc::new(TestWitness {
        expected: f.archive.clone(),
        retained: AtomicBool::new(false),
        retired_thread: Mutex::new(None),
    })
}

#[test]
fn actual_five_requests_real_verification_all_lanes_targets_and_off_ui_retained_retirement() {
    for (lane, version) in [
        (Channel::Stable, "0.19.2"),
        (Channel::Rc, "0.19.2-rc.3"),
        (Channel::Nightly, "0.19.2-nightly.2026-10-03+01234567"),
    ] {
        for platform in [Platform::MacArm64, Platform::MacX64, Platform::WindowsX64] {
            let f = Fixture::new(packed(version, lane, platform), version, lane, platform);
            let responses = records(&f);
            let inbound = responses.iter().map(Vec::len).sum::<usize>();
            let (transport, server) = server(responses);
            let engine = engine::Engine::new().unwrap();
            let mut service = fixture_service(&f, transport, engine.clone());
            let witness = witness(&f);
            Arc::get_mut(&mut service.facade).unwrap().witness = Some(witness.clone());
            let attempt = service
                .start(lane, SelectionCancellation::default())
                .unwrap();
            let state = wait(&attempt, PayloadStagePhase::Staged);
            assert!(service.owns(&attempt));
            assert_eq!(state.identity.unwrap().version, version);
            assert_eq!(state.archive_len, Some(f.archive.len()));
            assert!(witness.retained.load(Ordering::Acquire));
            assert_eq!(
                attempt.client.job.traffic.requests.load(Ordering::Acquire),
                5
            );
            assert_eq!(
                attempt.client.job.traffic.plaintext.load(Ordering::Acquire),
                inbound as u64
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 5);
            for (index, request) in requests.iter().enumerate() {
                assert!(
                    !request.to_ascii_lowercase().contains("authorization:")
                        && !request.contains("Cookie:")
                        && !request.contains("Range:")
                );
                if index < 2 {
                    assert!(!request.contains("download_token="));
                } else {
                    let target = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let url =
                        url::Url::parse(&format!("https://portalsurfer.org{target}")).unwrap();
                    assert_eq!(
                        url.query_pairs()
                            .filter(|(k, _)| k == "download_token")
                            .collect::<Vec<_>>(),
                        vec![("download_token".into(), TOKEN.into())]
                    );
                }
            }
            let replacement = fixture_service(&f, protocol::Transport::Production, engine.clone());
            assert!(!replacement.owns(&attempt));
            assert_eq!(
                error(replacement.start(lane, SelectionCancellation::default())),
                SelectionError::Unavailable
            );
            attempt.retire();
            wait(&attempt, PayloadStagePhase::Unstaged);
            admitted(&engine);
            assert_ne!(
                witness.retired_thread.lock().unwrap().unwrap(),
                thread::current().id()
            );
            assert!(attempt.observe().unwrap().archive_len.is_none());
        }
    }
}

#[test]
fn corrupt_mismatched_signed_archive_wrong_gate_short_body_and_redirect_never_stage() {
    for attack in 0..5 {
        let bytes = python_zip(if attack == 1 {
            json!({"manifest":{"version":"0.19.2-rc.1"}})
        } else {
            json!({})
        });
        let f = Fixture::new(bytes, "0.19.2", Channel::Stable, Platform::WindowsX64);
        let mut responses = records(&f);
        let mut expected_requests = 5;
        match attack {
            0 => {
                *responses[4].last_mut().unwrap() ^= 1;
            }
            1 => {}
            2 => {
                responses[1] = http(
                    &serde_json::to_vec(&json!({"build_id":"wrong-build","download_token":TOKEN}))
                        .unwrap(),
                    "application/json",
                );
                responses.truncate(2);
                expected_requests = 2;
            }
            3 => {
                responses[4].pop();
            }
            _ => {
                responses[2]=b"HTTP/1.1 302 Redirect\r\nLocation: https://evil.example/yield/a\r\nContent-Length: 0\r\n\r\n".to_vec();
                responses.truncate(3);
                expected_requests = 3;
            }
        }
        let (transport, server) = server(responses);
        let engine = engine::Engine::new().unwrap();
        let service = fixture_service(&f, transport, engine.clone());
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        let state = wait(&attempt, PayloadStagePhase::Failed);
        admitted(&engine);
        assert!(state.archive_len.is_none() && state.archive_sha256.is_none());
        assert_eq!(server.join().unwrap().len(), expected_requests);
    }
}

#[test]
fn real_unknown_length_and_chunked_asset_bodies_remain_bounded_and_retained_exactly() {
    for chunked in [false, true] {
        let f = Fixture::new(
            python_zip(json!({})),
            "0.19.2",
            Channel::Stable,
            Platform::WindowsX64,
        );
        let mut responses = records(&f);
        let mut zip = b"HTTP/1.1 200 OK\r\nConnection: close\r\n".to_vec();
        if chunked {
            zip.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            for chunk in f.archive.chunks(31) {
                zip.extend_from_slice(format!("{:x};fixture=yes\r\n", chunk.len()).as_bytes());
                zip.extend_from_slice(chunk);
                zip.extend_from_slice(b"\r\n");
            }
            zip.extend_from_slice(b"0\r\n\r\n");
        } else {
            zip.extend_from_slice(b"\r\n");
            zip.extend_from_slice(&f.archive);
        }
        responses[4] = zip;
        let (transport, server) = server(responses);
        let engine = engine::Engine::new().unwrap();
        let mut service = fixture_service(&f, transport, engine.clone());
        let witness = witness(&f);
        Arc::get_mut(&mut service.facade).unwrap().witness = Some(witness.clone());
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        wait(&attempt, PayloadStagePhase::Staged);
        assert!(witness.retained.load(Ordering::Acquire));
        server.join().unwrap();
        attempt.retire();
        wait(&attempt, PayloadStagePhase::Unstaged);
        admitted(&engine);
    }
}

#[test]
fn process_global_engine_and_missing_compiled_context_do_not_create_independent_facade_admission() {
    assert!(Arc::ptr_eq(
        &engine::Engine::global().unwrap(),
        &engine::Engine::global().unwrap()
    ));
    if Installed::compiled().is_none() {
        assert!(PayloadStagingService::compiled().is_none());
    }
}

// The owner is inside a real socket read, not a mocked pending future. The server releases
// that read only after the test has checked replacement admission and the Stopping observation.
fn blocked_catalog(
    response: Vec<u8>,
) -> (
    protocol::Transport,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let request = read_request(&mut socket);
        assert!(request.starts_with("GET /"));
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
        socket.write_all(&response[..split]).unwrap();
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let _ = socket.write_all(&response[split..]);
    });
    (
        protocol::Transport::Loopback(address),
        entered,
        release,
        handle,
    )
}

#[test]
fn cancellation_drop_and_deadline_keep_real_blocked_io_owned_until_confirmed_retirement() {
    for mode in 0..3 {
        let f = Fixture::new(
            python_zip(json!({})),
            "0.19.2",
            Channel::Stable,
            Platform::WindowsX64,
        );
        let (transport, entered, release, server) = blocked_catalog(records(&f).remove(0));
        let engine = engine::Engine::new().unwrap();
        let service = fixture_service(&f, transport, engine.clone());
        let cancel = SelectionCancellation::default();
        let attempt = service.start(Channel::Stable, cancel.clone()).unwrap();
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !(attempt.client.job.traffic.reading.load(Ordering::Acquire)
            && attempt.client.job.traffic.plaintext.load(Ordering::Acquire) > 0)
        {
            assert!(
                Instant::now() < deadline,
                "owner never entered the actual body socket read"
            );
            thread::yield_now();
        }
        let job = attempt.client.job.clone();
        let replacement = fixture_service(&f, protocol::Transport::Production, engine.clone());
        match mode {
            0 => {
                cancel.cancel();
                attempt.retire();
                attempt.retire();
            }
            1 => {
                service.retire();
            }
            2 => {
                drop(attempt);
            }
            _ => unreachable!(),
        }
        // A separate observation control here would keep the last UI caller alive in mode 2;
        // inspect the private job instead, and use the public observation in all other modes.
        assert!(!job.confirmed.load(Ordering::Acquire));
        assert!(engine.busy.load(Ordering::Acquire));
        for _ in 0..25 {
            assert_eq!(
                error(replacement.start(Channel::Stable, SelectionCancellation::default())),
                SelectionError::Unavailable
            );
        }
        if mode != 2 {
            let observation = PayloadStageAttempt {
                client: Arc::new(Client { job: job.clone() }),
            }
            .observe()
            .unwrap();
            assert_eq!(observation.phase, PayloadStagePhase::Stopping);
            assert!(observation.archive_len.is_none());
        }
        release.send(()).unwrap();
        server.join().unwrap();
        admitted(&engine);
        assert!(job.confirmed.load(Ordering::Acquire));
        assert_eq!(job.state.lock().unwrap().phase, PayloadStagePhase::Unstaged);
        assert_eq!(job.traffic.requests.load(Ordering::Acquire), 1);
        // Once actual old resources retire, a replacement runs the full real pipeline normally.
        let (transport, next_server) = self::server(records(&f));
        let next = fixture_service(&f, transport, engine.clone());
        let next_attempt = next
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        wait(&next_attempt, PayloadStagePhase::Staged);
        next_server.join().unwrap();
        assert!(!next.owns(&PayloadStageAttempt {
            client: Arc::new(Client { job: job.clone() })
        }));
        next_attempt.retire();
        wait(&next_attempt, PayloadStagePhase::Unstaged);
        admitted(&engine);
    }
}

#[test]
fn facade_and_last_caller_drop_retire_staged_bytes_on_owner_without_ui_deallocation() {
    for drop_facade in [false, true] {
        let f = Fixture::new(
            python_zip(json!({})),
            "0.19.2",
            Channel::Stable,
            Platform::WindowsX64,
        );
        let (transport, server) = server(records(&f));
        let engine = engine::Engine::new().unwrap();
        let mut service = fixture_service(&f, transport, engine.clone());
        let witness = witness(&f);
        Arc::get_mut(&mut service.facade).unwrap().witness = Some(witness.clone());
        let clone = service.clone();
        drop(clone); // a shared service remains live until its last facade
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        let job = attempt.client.job.clone();
        wait(&attempt, PayloadStagePhase::Staged);
        server.join().unwrap();
        let caller = attempt.clone();
        drop(caller);
        assert!(service.owns(&attempt));
        if drop_facade {
            drop(service);
        } else {
            drop(attempt);
        }
        admitted(&engine);
        assert!(job.confirmed.load(Ordering::Acquire));
        assert!(job.state.lock().unwrap().archive_len.is_none());
        assert_ne!(
            witness.retired_thread.lock().unwrap().unwrap(),
            thread::current().id()
        );
    }
}

#[test]
fn actual_owner_rejects_response_metadata_encoding_and_body_budget_attacks() {
    let f = Fixture::new(
        python_zip(json!({})),
        "0.19.2",
        Channel::Stable,
        Platform::WindowsX64,
    );
    for attack in 0..7 {
        let mut responses = records(&f);
        responses[4] = match attack {
            0 => format!(
                "HTTP/1.1 200 OK\r\nX-Large: {}\r\nContent-Length: {}\r\n\r\n",
                "x".repeat(8193),
                f.archive.len()
            )
            .into_bytes(),
            1 => format!(
                "HTTP/1.1 200 OK\r\n{}Content-Length: {}\r\n\r\n",
                (0..101)
                    .map(|n| format!("X-{n}: a\r\n"))
                    .collect::<String>(),
                f.archive.len()
            )
            .into_bytes(),
            2 => format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                f.archive.len()
            )
            .into_bytes(),
            3 => format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: {}\r\n\r\n",
                f.archive.len()
            )
            .into_bytes(),
            4 => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                f.archive.len() + 1
            )
            .into_bytes(),
            5 => {
                let mut bytes = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
                bytes.extend_from_slice(&f.archive);
                bytes.push(0);
                bytes
            }
            _ => format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;{}\r\nx\r\n0\r\n\r\n",
                "x".repeat(8193)
            )
            .into_bytes(),
        };
        let (transport, server) = server(responses);
        let engine = engine::Engine::new().unwrap();
        let service = fixture_service(&f, transport, engine.clone());
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        wait(&attempt, PayloadStagePhase::Failed);
        server.join().unwrap();
        admitted(&engine);
        assert!(attempt.observe().unwrap().archive_sha256.is_none());
    }
}

struct WireLog(Mutex<Vec<String>>);
impl log::Log for WireLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().starts_with("ureq")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let mut records = self.0.lock().unwrap();
            if records.len() < 4096 {
                records.push(format!("{} {}", record.target(), record.args()));
            }
        }
    }
    fn flush(&self) {}
}
#[test]
fn actual_ureq_debug_logging_never_receives_tokenized_urls_but_wire_has_exact_token() {
    static LOGGER: WireLog = WireLog(Mutex::new(Vec::new()));
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let f = Fixture::new(
        python_zip(json!({})),
        "0.19.2",
        Channel::Stable,
        Platform::WindowsX64,
    );
    let (transport, server) = server(records(&f));
    let engine = engine::Engine::new().unwrap();
    let service = fixture_service(&f, transport, engine.clone());
    let attempt = service
        .start(Channel::Stable, SelectionCancellation::default())
        .unwrap();
    wait(&attempt, PayloadStagePhase::Staged);
    let requests = server.join().unwrap();
    assert!(requests[4].contains("download_token="));
    let log = LOGGER.0.lock().unwrap().join("\n");
    assert!(
        log.contains("sending request") && log.contains("writing prelude"),
        "the actual client debug logging must be exercised"
    );
    let target = requests[4]
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .split('?')
        .next()
        .unwrap();
    assert!(log.contains(target));
    assert!(
        !log.contains("download_token")
            && !log.contains(TOKEN)
            && !log.contains("owner-wire-secret")
    );
    attempt.retire();
    wait(&attempt, PayloadStagePhase::Unstaged);
    admitted(&engine);
}

#[test]
fn real_http_reads_enforce_nonresettable_aggregate_transfer_ceilings() {
    for plaintext in [false, true] {
        let f = Fixture::new(
            python_zip(json!({})),
            "0.19.2",
            Channel::Stable,
            Platform::WindowsX64,
        );
        let response = records(&f).remove(0);
        let (transport, entered, release, server) = blocked_catalog(response);
        let engine = engine::Engine::new().unwrap();
        let service = fixture_service(&f, transport, engine.clone());
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        // Seed the private accumulator at its real production ceiling while the read is blocked.
        // The actual socket body must be denied by the transport before it reaches JSON parsing.
        let counter = if plaintext {
            &attempt.client.job.traffic.plaintext
        } else {
            &attempt.client.job.traffic.body
        };
        let limit = if plaintext {
            protocol::staging::PLAINTEXT_LIMIT
        } else {
            protocol::staging::BODY_LIMIT
        };
        counter.store(limit, Ordering::Release);
        release.send(()).unwrap();
        server.join().unwrap();
        wait(&attempt, PayloadStagePhase::Failed);
        admitted(&engine);
        assert_eq!(counter.load(Ordering::Acquire), limit);
        assert_eq!(
            attempt.client.job.traffic.requests.load(Ordering::Acquire),
            1
        );
    }
}

#[test]
fn actual_socket_whole_deadline_retires_io_before_admission_is_released() {
    let f = Fixture::new(
        python_zip(json!({})),
        "0.19.2",
        Channel::Stable,
        Platform::WindowsX64,
    );
    let (transport, entered, release, server) = blocked_catalog(records(&f).remove(0));
    let engine = engine::Engine::new().unwrap();
    let service = fixture_service(&f, transport, engine.clone());
    let attempt = service
        .start_before(
            Channel::Stable,
            SelectionCancellation::default(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(engine.busy.load(Ordering::Acquire));
    assert!(!attempt.client.job.confirmed.load(Ordering::Acquire));
    wait(&attempt, PayloadStagePhase::Unstaged);
    admitted(&engine);
    assert!(attempt.client.job.confirmed.load(Ordering::Acquire));
    assert!(attempt.observe().unwrap().archive_len.is_none());
    assert_eq!(
        attempt.client.job.traffic.requests.load(Ordering::Acquire),
        1
    );
    release.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn actual_owner_rejects_aggregate_headers_trailers_and_encoded_gate_before_staging() {
    for attack in 0..3 {
        let mut f = Fixture::new(
            python_zip(json!({})),
            "0.19.2",
            Channel::Stable,
            Platform::WindowsX64,
        );
        if attack == 2 {
            f.selected.release.files[0].url = url::Url::parse(&format!(
                "https://portalsurfer.org/yield/a?x={}",
                "~".repeat(3900)
            ))
            .unwrap();
        }
        let mut responses = records(&f);
        if attack == 0 {
            responses[4] = format!(
                "HTTP/1.1 200 OK\r\n{}Content-Length: {}\r\n\r\n",
                (0..5)
                    .map(|i| format!("X-{i}: {}\r\n", "x".repeat(7000)))
                    .collect::<String>(),
                f.archive.len()
            )
            .into_bytes();
        } else if attack == 1 {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                f.archive.len()
            )
            .into_bytes();
            response.extend_from_slice(&f.archive);
            response.extend_from_slice(b"\r\n0\r\n");
            response
                .extend_from_slice(format!("X-Trailer: {}\r\n\r\n", "x".repeat(8193)).as_bytes());
            responses[4] = response;
        } else {
            responses[1]=http(&serde_json::to_vec(&json!({"build_id":f.selected.release.build_id,"download_token":"/".repeat(4096)})).unwrap(),"application/json");
            responses.truncate(2);
        }
        let (transport, server) = server(responses);
        let engine = engine::Engine::new().unwrap();
        let service = fixture_service(&f, transport, engine.clone());
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        wait(&attempt, PayloadStagePhase::Failed);
        admitted(&engine);
        assert_eq!(
            server.join().unwrap().len(),
            if attack == 2 { 2 } else { 5 }
        );
        assert!(attempt.observe().unwrap().archive_len.is_none());
    }
}

#[test]
fn actual_valid_corrupt_and_mismatched_owner_never_touch_installed_profile_or_source_sentinels() {
    let temp = tempfile::tempdir().unwrap();
    let sentinels = ["installed/yield", "profile/settings", "source/library"];
    for path in sentinels {
        let path = temp.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"preserved source bytes").unwrap();
    }
    for attack in 0..3 {
        let f = Fixture::new(
            python_zip(if attack == 2 {
                json!({"manifest":{"version":"0.19.2-rc.1"}})
            } else {
                json!({})
            }),
            "0.19.2",
            Channel::Stable,
            Platform::WindowsX64,
        );
        let mut responses = records(&f);
        if attack == 1 {
            *responses[4].last_mut().unwrap() ^= 1;
        }
        let (transport, server) = server(responses);
        let engine = engine::Engine::new().unwrap();
        let service = fixture_service(&f, transport, engine.clone());
        let attempt = service
            .start(Channel::Stable, SelectionCancellation::default())
            .unwrap();
        wait(
            &attempt,
            if attack == 0 {
                PayloadStagePhase::Staged
            } else {
                PayloadStagePhase::Failed
            },
        );
        server.join().unwrap();
        if attack == 0 {
            attempt.retire();
            wait(&attempt, PayloadStagePhase::Unstaged);
        }
        admitted(&engine);
        for path in sentinels {
            assert_eq!(
                std::fs::read(temp.path().join(path)).unwrap(),
                b"preserved source bytes"
            );
        }
        // No staging, extraction or cleanup destinations are even accepted by the owner API.
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 3);
    }
}
