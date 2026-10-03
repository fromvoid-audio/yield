//! Actual bounded HTTP and production selection paths with private metadata fixtures.
//! These declarations are not signed byte witnesses or native artifact qualification.
use super::*;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
fn installed(version: &str, platform: Platform) -> Installed {
    let semantic = semver::Version::parse(version).unwrap();
    Installed {
        core: [
            semantic.major as u32,
            semantic.minor as u32,
            semantic.patch as u32,
        ],
        version: semantic,
        full_version: version.into(),
        number: 7,
        commit: [0xab; 20],
        platform,
    }
}
fn release(version: &str, lane: Channel, number: u64) -> Value {
    let version_parsed = semver::Version::parse(version).unwrap();
    let files = catalog::inventory(&version_parsed, lane)
        .unwrap()
        .into_iter()
        .map(|name| {
            json!({"name":name,"sha256":"ab".repeat(32),"size_bytes":64,
            "url":format!("/yield/assets/{name}?download_token=discarded&x=1")})
        })
        .collect::<Vec<_>>();
    json!({"build_id":format!("yield-{version}"), "build_number":number,"version":version,
        "commit":COMMIT,"channel":crate::update_catalog::channel_name(lane),"files":files})
}
fn bytes(releases: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({"releases":releases})).unwrap()
}
fn gate(id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"build_id":id,"download_token":"private +/% token"})).unwrap()
}
fn error<T>(result: Result<T, SelectionError>) -> SelectionError {
    match result {
        Ok(_) => panic!("unexpected accepted selection"),
        Err(e) => e,
    }
}
fn http(body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn actual_http_chunk_decoder_and_eof_reader_preserve_complete_selection() {
    let row = release("0.19.2", Channel::Stable, 8);
    let catalog = bytes(vec![row.clone()]);
    let gate = gate(row["build_id"].as_str().unwrap());
    let mut chunked = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in catalog.chunks(97) {
        chunked.extend_from_slice(format!("{:x};fixture=yes\r\n", chunk.len()).as_bytes());
        chunked.extend_from_slice(chunk);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let mut eof =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n".to_vec();
    eof.extend_from_slice(&gate);
    let (transport, server) = serve(vec![chunked, eof]);
    let service = PayloadSelectionService::new(installed("0.19.1", Platform::MacArm64), transport);
    let selected = service
        .select(Channel::Stable, SelectionCancellation::default())
        .unwrap();
    assert_eq!(selected.identity().unwrap().version, "0.19.2");
    assert_eq!(selected.inventory().unwrap().count(), 5);
    assert_eq!(server.join().unwrap().len(), 2);
}
fn serve(responses: Vec<Vec<u8>>) -> (protocol::Transport, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing expected HTTP request");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut block = [0; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let n = socket.read(&mut block).unwrap();
                assert!(n > 0 && request.len() + n <= 4096);
                request.extend_from_slice(&block[..n]);
            }
            requests.push(String::from_utf8(request).unwrap());
            let _ = socket.write_all(&response); // Early rejection can close before an oversized body.
        }
        requests
    });
    (protocol::Transport::Loopback(address), worker)
}

#[test]
fn real_http_all_lanes_targets_retain_complete_identity_and_exact_gate_requests() {
    for (channel, version) in [
        (Channel::Stable, "0.19.2"),
        (Channel::Rc, "0.19.2-rc.3"),
        (Channel::Nightly, "0.19.2-nightly.2026-10-03+01234567"),
    ] {
        for platform in [Platform::MacArm64, Platform::MacX64, Platform::WindowsX64] {
            let row = release(version, channel, 8);
            let id = row["build_id"].as_str().unwrap();
            let catalog = bytes(vec![row.clone()]);
            let (transport, server) = serve(vec![http(&catalog), http(&gate(id))]);
            let service = PayloadSelectionService::new(installed("0.19.1", platform), transport);
            let selected = service
                .select(channel, SelectionCancellation::default())
                .unwrap();
            assert!(service.owns(&selected));
            let identity = selected.identity().unwrap();
            assert_eq!(identity.version, version);
            assert_eq!(identity.build_id, id);
            assert_eq!(identity.build_number, 8);
            assert_eq!(identity.platform, platform);
            assert_eq!(identity.channel, channel);
            assert_eq!(
                identity.commit,
                crate::update_catalog::full_commit(COMMIT).unwrap()
            );
            assert!(!identity.current);
            let inventory = selected.inventory().unwrap().collect::<Vec<_>>();
            assert_eq!(inventory.len(), 5);
            assert!(
                inventory
                    .iter()
                    .all(|f| f.size_bytes == 64 && f.sha256 == [0xab; 32])
            );
            assert!(selected.matches_catalog(&catalog).unwrap());
            assert!(
                selected
                    .request_lengths()
                    .unwrap()
                    .into_iter()
                    .all(|n| n <= 20 * 1024)
            );
            for (index, request) in selected.requests.iter().enumerate() {
                let url = url::Url::parse(protocol::test_request(request)).unwrap();
                assert_eq!(
                    url.origin().ascii_serialization(),
                    "https://portalsurfer.org"
                );
                let tokens = url
                    .query_pairs()
                    .filter(|(k, _)| k == "download_token")
                    .collect::<Vec<_>>();
                assert_eq!(tokens.len(), 1);
                assert_eq!(tokens[0].1, "private +/% token");
                if index == 0 {
                    assert!(
                        url.path().ends_with(
                            &crate::update_catalog::asset_name(
                                &semver::Version::parse(version).unwrap(),
                                channel,
                                platform
                            )
                            .unwrap()
                        )
                    );
                }
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].starts_with("GET /yield/api/v1/releases HTTP/1.1\r\n"));
            let expected = protocol::gate_url(id).unwrap();
            let expected = url::Url::parse(&expected).unwrap();
            assert!(requests[1].starts_with(&format!(
                "GET {}?donation_amount=0.00 HTTP/1.1\r\n",
                expected.path()
            )));
            assert!(
                requests
                    .iter()
                    .all(|r| !r.to_ascii_lowercase().contains("authorization:")
                        && !r.contains("X-Yield-Release-Verification")
                        && !r.contains("download_token="))
            );
        }
    }
}

#[test]
fn original_private_issuer_generation_cancellation_and_drop_cannot_be_reconstructed() {
    let row = release("0.19.2", Channel::Stable, 8);
    let catalog = bytes(vec![row.clone()]);
    let id = row["build_id"].as_str().unwrap();
    let (transport, server) = serve(vec![
        http(&catalog),
        http(&gate(id)),
        http(&catalog),
        http(&gate(id)),
    ]);
    let service = PayloadSelectionService::new(installed("0.19.1", Platform::MacArm64), transport);
    let other = PayloadSelectionService::new(
        installed("0.19.1", Platform::MacArm64),
        protocol::Transport::Production,
    );
    let first_cancel = SelectionCancellation::default();
    let first = service
        .select(Channel::Stable, first_cancel.clone())
        .unwrap();
    assert!(!other.owns(&first));
    let deadline = Instant::now() + Duration::from_secs(2);
    while service.issuer.busy.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "selection worker did not quiesce"
        );
        thread::yield_now();
    }
    let second_cancel = SelectionCancellation::default();
    let second = service
        .select(Channel::Stable, second_cancel.clone())
        .unwrap();
    assert!(!service.owns(&first));
    assert!(service.owns(&second));
    assert_eq!(error(first.identity()), SelectionError::Retired);
    first_cancel.cancel();
    assert!(service.owns(&second));
    second_cancel.cancel();
    assert_eq!(error(second.identity()), SelectionError::Retired);
    assert_eq!(
        error(second.matches_catalog(&catalog)),
        SelectionError::Retired
    );
    assert_eq!(
        error(service.select(Channel::Stable, second_cancel)),
        SelectionError::Retired
    );
    server.join().unwrap();

    let (transport, server) = serve(vec![http(&catalog), http(&gate(id))]);
    let service = PayloadSelectionService::new(installed("0.19.1", Platform::MacArm64), transport);
    let original = service
        .select(Channel::Stable, SelectionCancellation::default())
        .unwrap();
    drop(service);
    assert_eq!(error(original.inventory()), SelectionError::Retired);
    server.join().unwrap();
}

#[test]
fn fresh_metadata_cannot_change_the_original_selected_full_identity_or_inventory() {
    let row = release("0.19.2-rc.2", Channel::Rc, 8);
    let input = bytes(vec![row.clone()]);
    let (transport, server) = serve(vec![
        http(&input),
        http(&gate(row["build_id"].as_str().unwrap())),
    ]);
    let service = PayloadSelectionService::new(installed("0.19.1", Platform::MacArm64), transport);
    let selected = service
        .select(Channel::Rc, SelectionCancellation::default())
        .unwrap();
    for field in ["sha256", "size_bytes", "url"] {
        let mut changed = row.clone();
        changed["files"][0][field] = match field {
            "sha256" => json!("cd".repeat(32)),
            "size_bytes" => json!(65),
            _ => json!("/yield/different.zip"),
        };
        assert!(!selected.matches_catalog(&bytes(vec![changed])).unwrap());
        assert_eq!(selected.identity().unwrap().version, "0.19.2-rc.2");
    }
    assert!(
        !selected
            .matches_catalog(&bytes(vec![release("0.19.2-rc.3", Channel::Rc, 9)]))
            .unwrap()
    );
    service.retire();
    assert_eq!(error(selected.request_lengths()), SelectionError::Retired);
    server.join().unwrap();
}

#[test]
fn immutable_installed_policy_preserves_current_renumbering_and_rejects_all_other_rollbacks() {
    let current = installed("0.19.1", Platform::MacArm64);
    let mut row = release("0.19.1", Channel::Stable, 7);
    row["commit"] = json!("ab".repeat(20));
    let selected = catalog::select(&bytes(vec![row.clone()]), &current, Channel::Stable).unwrap();
    assert!(selected.current(&current));
    row["commit"] = json!(COMMIT);
    assert_eq!(
        error(catalog::select(
            &bytes(vec![row]),
            &current,
            Channel::Stable
        )),
        SelectionError::Invalid
    );
    let mixed = bytes(vec![
        release("19.1.9", Channel::Stable, 999),
        release("0.19.2", Channel::Stable, 8),
    ]);
    assert_eq!(
        catalog::select(&mixed, &current, Channel::Stable)
            .unwrap()
            .version,
        "0.19.2"
    );
    assert_eq!(
        catalog::select(
            &bytes(vec![release("0.19.1", Channel::Stable, 8)]),
            &installed("19.1.9", Platform::MacArm64),
            Channel::Stable
        )
        .unwrap()
        .version,
        "0.19.1"
    );
    for (installed_version, candidate) in [
        ("19.2.0", "0.19.1"),
        ("19.0.0", "0.19.1"),
        ("0.19.2", "0.19.1"),
        ("0.19.1-rc.3", "0.19.1-rc.2"),
    ] {
        let lane = if candidate.contains("rc.") {
            Channel::Rc
        } else {
            Channel::Stable
        };
        assert_eq!(
            error(catalog::select(
                &bytes(vec![release(candidate, lane, 8)]),
                &installed(installed_version, Platform::MacArm64),
                lane
            )),
            SelectionError::Invalid
        );
    }
    assert!(
        catalog::select(
            &bytes(vec![release("19.2.0", Channel::Stable, 8)]),
            &installed("19.1.9", Platform::MacArm64),
            Channel::Stable
        )
        .is_ok()
    );
}

#[test]
fn incomplete_wrong_duplicate_or_aliased_assets_fail_in_the_real_catalog_parser() {
    let original = release("0.19.2", Channel::Stable, 8);
    let context = installed("0.19.1", Platform::MacArm64);
    for case in 0..15 {
        let mut row = original.clone();
        match case {
            0 => {
                row["files"].as_array_mut().unwrap().pop();
            }
            1 => row["files"][1] = row["files"][0].clone(),
            2 => row["files"][0]["name"] = json!("wrong.zip"),
            3 => row["files"][0]["sha256"] = json!("bad"),
            4 => row["files"][0]["size_bytes"] = json!(0),
            5 => row["files"][0]["size_bytes"] = json!(u64::MAX),
            6 => row["files"][0]["url"] = json!("https://evil.example/yield/file.zip"),
            7 => row["files"][0]["download_url"] = json!("/yield/different.zip"),
            8 => row["files"][1]["url"] = row["files"][0]["url"].clone(),
            9 => row["commit"] = json!("0000000000000000000000000000000000000000"),
            10 => row["build_number"] = json!(0),
            11 => row["build_id"] = json!("yield-wrong-id"),
            12 => row["channel"] = json!("rc"),
            13 => {
                row["commit"] = Value::Null;
            }
            _ => row["files"][0]["url"] = Value::Null,
        }
        assert_eq!(
            error(catalog::select(
                &bytes(vec![row]),
                &context,
                Channel::Stable
            )),
            SelectionError::Invalid,
            "case {case}"
        );
    }
    assert_eq!(
        error(catalog::select(
            &bytes(vec![original.clone(), original]),
            &context,
            Channel::Stable
        )),
        SelectionError::Invalid
    );
}

#[test]
fn unsafe_unnormalized_urls_reject_before_any_token_is_attached() {
    for input in [
        "http://portalsurfer.org/yield/a",
        "https://evil.example/yield/a",
        "https://user@portalsurfer.org/yield/a",
        "https://portalsurfer.org:444/yield/a",
        "/yield/../a",
        "/yield/%2e%2e/a",
        "/yield/%252e%252e/a",
        "/yield/a%2fb",
        "/yield/a%5Cb",
        "/yield/a%00b",
        "/yield/a\\b",
        "/yield/a#fragment",
        "/yield/a#",
        "/other/a",
        "https:/portalsurfer.org/yield/a",
        "/yield/a%",
        "/yield/a%zz",
        "/yield/a\nb",
        "/yield/%3fdownload_token=stolen",
        "/yield/a?%2564ownload_token=stolen",
    ] {
        assert!(
            protocol::asset_url(input).is_err(),
            "accepted unsafe spelling"
        );
    }
    assert!(protocol::asset_url("https://portalsurfer.org/yield/a?x=1").is_ok());
    assert!(protocol::asset_url("yield/a?x=1").is_ok());
    let a = protocol::asset_url("/yield/a%2ezip?x=1&download_token=old").unwrap();
    let b = protocol::asset_url("/yield/a.zip?download_token=other&%78=1").unwrap();
    assert!(protocol::same_resource(&a, &b));
}

#[test]
fn gate_build_token_substitution_and_actual_encoded_request_limits() {
    let row = release("0.19.2", Channel::Stable, 8);
    let context = installed("0.19.1", Platform::MacArm64);
    let selected = catalog::select(&bytes(vec![row]), &context, Channel::Stable).unwrap();
    for value in [
        json!({"build_id":"other","download_token":"token"}),
        json!({"build_id":selected.build_id,"download_token":""}),
        json!({"build_id":selected.build_id,"download_token":"x".repeat(4097)}),
        json!({"build_id":selected.build_id}),
        json!({"build_id":selected.build_id,"download_token":9}),
    ] {
        assert_eq!(
            error(protocol::gate_requests(
                &serde_json::to_vec(&value).unwrap(),
                &selected,
                Channel::Stable,
                Platform::MacArm64
            )),
            SelectionError::Invalid
        );
    }
    let token = "/".repeat(4096);
    let gate =
        serde_json::to_vec(&json!({"build_id":selected.build_id,"download_token":token})).unwrap();
    let requests =
        protocol::gate_requests(&gate, &selected, Channel::Stable, Platform::MacArm64).unwrap();
    for request in &requests {
        assert!(request.len() <= 20 * 1024);
        let parsed = url::Url::parse(protocol::test_request(request)).unwrap();
        assert_eq!(
            parsed
                .query_pairs()
                .filter(|(k, _)| k == "download_token")
                .count(),
            1
        );
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(k, _)| k == "download_token")
                .unwrap()
                .1,
            token
        );
    }
}

#[test]
fn bounded_streaming_schema_rejects_rows_files_text_depth_and_duplicate_identity_fields() {
    let row = release("0.19.2", Channel::Stable, 8);
    let context = installed("0.19.1", Platform::MacArm64);
    let mut files = row.clone();
    files["files"] = json!(vec![row["files"][0].clone(); 17]);
    let mut text = row.clone();
    text["build_id"] = json!("x".repeat(161));
    let mut url = row.clone();
    url["files"][0]["url"] = json!("x".repeat(4097));
    for input in [
        bytes(vec![
            json!({"build_id":"x","build_number":1,"version":"0.19.1","files":[]});
            513
        ]),
        bytes(vec![files]),
        bytes(vec![text]),
        bytes(vec![url]),
        vec![b' '; 512 * 1024 + 1],
        format!("{}0{}", "[".repeat(17), "]".repeat(17)).into_bytes(),
        b"{\"releases\":[],\"releases\":[]}".to_vec(),
    ] {
        assert!(catalog::select(&input, &context, Channel::Stable).is_err());
    }
    assert!(catalog::select(&bytes(vec![row]), &context, Channel::Stable).is_ok());
}

#[test]
fn final_request_budget_counts_both_existing_query_and_token_encoding_expansion() {
    let context = installed("0.19.1", Platform::MacArm64);
    let mut row = release("0.19.2", Channel::Stable, 8);
    row["files"][0]["url"] = json!(format!("/yield/a?x={}", "~".repeat(3900)));
    let selected = catalog::select(&bytes(vec![row]), &context, Channel::Stable).unwrap();
    let gate = serde_json::to_vec(
        &json!({"build_id":selected.build_id,"download_token":"/".repeat(4096)}),
    )
    .unwrap();
    assert_eq!(
        error(protocol::gate_requests(
            &gate,
            &selected,
            Channel::Stable,
            Platform::MacArm64
        )),
        SelectionError::Invalid
    );
}

#[test]
fn actual_http_rejects_redirects_metadata_limits_and_short_bodies_without_retry_or_gate() {
    let catalog = bytes(vec![release("0.19.2", Channel::Stable, 8)]);
    let short = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        catalog.len() + 1
    );
    let mut short = short.into_bytes();
    short.extend_from_slice(&catalog);
    let oversized=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Large: {}\r\nContent-Length: 0\r\n\r\n","x".repeat(8192)).into_bytes();
    for response in [short,oversized,
        b"HTTP/1.1 301 Redirect\r\nLocation: https://evil.example/yield/a\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 524289\r\n\r\n".to_vec()] {
        let (transport,server)=serve(vec![response]);
        let service=PayloadSelectionService::new(installed("0.19.1",Platform::MacArm64),transport);
        assert_eq!(error(service.select(Channel::Stable,SelectionCancellation::default())),SelectionError::Invalid);
        assert_eq!(server.join().unwrap().len(),1);
    }
}

#[test]
fn exact_catalog_row_limit_accepts_complete_inventory_without_preallocating_a_513th_row() {
    let context = installed("0.19.1", Platform::MacArm64);
    let rows = (2u32..=513)
        .map(|patch| {
            let mut row = release(
                &format!("0.19.{patch}"),
                Channel::Stable,
                u64::from(patch) + 7,
            );
            for (i, file) in row["files"].as_array_mut().unwrap().iter_mut().enumerate() {
                file["url"] = json!(format!("/yield/f{i}"));
            }
            row
        })
        .collect::<Vec<_>>();
    let accepted = bytes(rows.clone());
    assert!(accepted.len() <= 512 * 1024);
    assert_eq!(
        catalog::select(&accepted, &context, Channel::Stable)
            .unwrap()
            .build_number,
        520
    );
    let mut over = rows;
    over.push(release("0.19.514", Channel::Stable, 521));
    let rejected = bytes(over);
    assert!(rejected.len() <= 512 * 1024);
    assert_eq!(
        error(catalog::select(&rejected, &context, Channel::Stable)),
        SelectionError::Invalid
    );
}

#[test]
fn old_identityless_nightly_does_not_masquerade_as_current_or_mask_a_newer_complete_build() {
    let modern = release("0.19.2-nightly.2026-10-03+01234567", Channel::Nightly, 8);
    let context = installed("0.19.1", Platform::MacArm64);
    let old = json!({"build_id":"yield-nightly-b1-01234567","build_number":1,"version":"nightly",
        "files":[{"name":"yield-nightly-macos-aarch64.zip"}]});
    assert!(
        catalog::select(
            &bytes(vec![old.clone(), modern.clone()]),
            &context,
            Channel::Nightly
        )
        .is_ok()
    );
    let mut unknown = old;
    unknown["build_number"] = json!(9);
    assert_eq!(
        error(catalog::select(
            &bytes(vec![unknown, modern]),
            &context,
            Channel::Nightly
        )),
        SelectionError::Unavailable
    );
}

#[test]
fn bad_gate_is_rejected_by_the_actual_http_worker_without_returning_selection() {
    let row = release("0.19.2", Channel::Stable, 8);
    let input = bytes(vec![row]);
    let (transport, server) = serve(vec![http(&input), http(&gate("other-build"))]);
    let service = PayloadSelectionService::new(installed("0.19.1", Platform::MacArm64), transport);
    assert_eq!(
        error(service.select(Channel::Stable, SelectionCancellation::default())),
        SelectionError::Invalid
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn cancellation_retains_admission_until_actual_blocked_http_worker_stops() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).unwrap() > 0);
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let _ = socket.write_all(&http(b"{\"releases\":[]}"));
    });
    let service = Arc::new(PayloadSelectionService::new(
        installed("0.19.1", Platform::MacArm64),
        protocol::Transport::Loopback(address),
    ));
    let cancellation = SelectionCancellation::default();
    let worker_service = service.clone();
    let worker_cancel = cancellation.clone();
    let caller =
        thread::spawn(move || error(worker_service.select(Channel::Stable, worker_cancel)));
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    cancellation.cancel();
    assert_eq!(caller.join().unwrap(), SelectionError::Retired);
    assert!(service.issuer.busy.load(Ordering::Acquire));
    assert_eq!(
        error(service.select(Channel::Stable, SelectionCancellation::default())),
        SelectionError::Unavailable
    );
    release_tx.send(()).unwrap();
    server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while service.issuer.busy.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

#[test]
fn compiled_factory_cannot_fill_missing_context_from_runtime_environment() {
    if yield_core::ReleaseBuildIdentity::compiled().is_err() {
        assert!(PayloadSelectionService::compiled().is_none());
    }
}
