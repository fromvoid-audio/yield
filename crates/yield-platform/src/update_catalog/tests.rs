use super::*;
use serde_json::json;
use std::net::TcpListener;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn release(version: &str, number: u64, asset: &str) -> serde_json::Value {
    json!({"build_id":format!("yield-b{number}-{SHA}"), "build_number":number,
        "version":version,"files":[{"name":asset}]})
}

fn catalog(releases: Vec<serde_json::Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({"releases":releases})).unwrap()
}

fn serve(
    status: &str,
    extra: &str,
    body: Vec<u8>,
    delay: Duration,
) -> (CatalogService, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/releases", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let thread = std::thread::spawn(move || {
        use std::io::Write;
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut bytes = [0u8; 4096];
        let n = stream.read(&mut bytes).unwrap();
        let request = String::from_utf8_lossy(&bytes[..n]);
        assert!(request.starts_with("GET /releases HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        std::thread::sleep(delay);
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&body);
    });
    (
        CatalogService {
            busy: Arc::new(AtomicBool::new(false)),
            endpoint,
            deadline: Duration::from_millis(200),
        },
        thread,
    )
}

fn fetch_local(status: &str, extra: &str, bytes: Vec<u8>) -> Outcome {
    let (service, thread) = serve(status, extra, bytes, Duration::ZERO);
    let outcome = service.check(Channel::Stable, Platform::MacArm64, &AtomicBool::new(false));
    thread.join().unwrap();
    outcome
}

#[test]
fn production_transport_reads_exact_matching_identity_and_asset() {
    let expected = Candidate {
        version: [19, 1, 0],
        release_version: "19.1.0".into(),
        build_number: 44,
        commit: full_commit(SHA).unwrap(),
        channel: Channel::Stable,
        platform: Platform::MacArm64,
    };
    let body = catalog(vec![release(
        "19.1.0",
        44,
        "yield-19.1.0-macos-aarch64.zip",
    )]);
    assert_eq!(
        fetch_local("200 OK", "", body),
        Outcome::Candidate(expected)
    );
}

#[test]
fn substitution_wrong_platform_and_no_candidate_never_report_current() {
    let mut item = release("19.1.0", 44, "other-19.1.0-macos-aarch64.zip");
    assert_eq!(
        fetch_local("200 OK", "", catalog(vec![item.clone()])),
        Outcome::Unavailable
    );
    item["files"][0]["name"] = json!("yield-19.1.0-windows-x86_64.zip");
    assert_eq!(
        fetch_local("200 OK", "", catalog(vec![item])),
        Outcome::Unavailable
    );
    assert_eq!(
        fetch_local("200 OK", "", catalog(vec![])),
        Outcome::Unavailable
    );
}

#[test]
fn lanes_require_exact_version_asset_and_full_commit() {
    for (channel, version, asset) in [
        (
            Channel::Rc,
            "19.1.0-rc.2",
            "yield-19.1.0-rc.2-macos-aarch64.zip",
        ),
        (
            Channel::Nightly,
            "19.1.0-nightly.44",
            "yield-nightly-macos-aarch64.zip",
        ),
    ] {
        let body = catalog(vec![release(version, 44, asset)]);
        assert!(matches!(
            select(&body, channel, Platform::MacArm64),
            Outcome::Candidate(_)
        ));
        assert_eq!(
            select(&body, Channel::Stable, Platform::MacArm64),
            Outcome::Unavailable
        );
    }
    let mut item = release("19.1.0", 44, "yield-19.1.0-macos-aarch64.zip");
    item["build_id"] = json!("yield-b44-0123456");
    assert_eq!(
        select(
            &catalog(vec![item.clone()]),
            Channel::Stable,
            Platform::MacArm64
        ),
        Outcome::Unavailable
    );
    item["commit"] = json!(SHA);
    assert!(matches!(
        select(
            &catalog(vec![item.clone()]),
            Channel::Stable,
            Platform::MacArm64
        ),
        Outcome::Candidate(_)
    ));
    item["commit"] = json!("abcdef0123456789abcdef0123456789abcdef01");
    assert_eq!(
        select(&catalog(vec![item]), Channel::Stable, Platform::MacArm64),
        Outcome::Failed
    );
}

#[test]
fn latest_incomplete_identity_does_not_fall_back_to_older_release() {
    let mut latest = release("19.1.0", 45, "yield-19.1.0-macos-aarch64.zip");
    latest["build_id"] = json!("yield-b45-abcdef0");
    let older = release("19.1.0", 44, "yield-19.1.0-macos-aarch64.zip");
    assert_eq!(
        select(
            &catalog(vec![older, latest]),
            Channel::Stable,
            Platform::MacArm64
        ),
        Outcome::Unavailable
    );
}

#[test]
fn complete_public_commit_field_preserves_rc_identity_and_all_supported_targets() {
    for (platform, asset) in [
        (Platform::MacArm64, "yield-19.1.0-rc.2-macos-aarch64.zip"),
        (Platform::MacX64, "yield-19.1.0-rc.2-macos-x86_64.zip"),
        (Platform::WindowsX64, "yield-19.1.0-rc.2-windows-x86_64.zip"),
    ] {
        let mut item = release("19.1.0-rc.2", 44, asset);
        item["build_id"] = json!("yield-19.1.0-rc.2");
        item["commit"] = json!(SHA);
        let Outcome::Candidate(candidate) = select(&catalog(vec![item]), Channel::Rc, platform)
        else {
            panic!("complete candidate required")
        };
        assert_eq!(candidate.release_version, "19.1.0-rc.2");
        assert_eq!(candidate.commit, full_commit(SHA).unwrap());
        assert_eq!(candidate.platform, platform);
    }
}

#[test]
fn reused_build_identity_and_explicit_lane_substitution_fail() {
    let first = release("19.1.0", 44, "yield-19.1.0-macos-aarch64.zip");
    let mut second = first.clone();
    second["build_id"] = json!("yield-b44-abcdef0123456789abcdef0123456789abcdef01");
    let newest = release("19.1.0", 45, "yield-19.1.0-macos-aarch64.zip");
    assert_eq!(
        select(
            &catalog(vec![newest, first.clone(), second]),
            Channel::Stable,
            Platform::MacArm64
        ),
        Outcome::Failed
    );
    let mut wrong_lane = first;
    wrong_lane["channel"] = json!("nightly");
    assert_eq!(
        select(
            &catalog(vec![wrong_lane]),
            Channel::Stable,
            Platform::MacArm64
        ),
        Outcome::Failed
    );
}

#[test]
fn malformed_oversized_nested_and_error_responses_fail() {
    assert_eq!(
        fetch_local("200 OK", "", b"{bad}".to_vec()),
        Outcome::Failed
    );
    assert_eq!(
        fetch_local("500 Error", "", catalog(vec![])),
        Outcome::Failed
    );
    assert_eq!(
        fetch_local("200 OK", "", vec![b' '; MAX_BYTES as usize + 1]),
        Outcome::Failed
    );
    assert_eq!(
        fetch_local("302 Found", "Location: http://127.0.0.1:9/evil\r\n", vec![]),
        Outcome::Failed
    );
    assert_eq!(
        fetch_local("200 OK", "Content-Encoding: gzip\r\n", vec![]),
        Outcome::Failed
    );
    let mut nested = vec![b'['; 17];
    nested.extend(vec![b']'; 17]);
    assert_eq!(fetch_local("200 OK", "", nested), Outcome::Failed);
    let mut release = release("19.1.0", 44, "yield-19.1.0-macos-aarch64.zip");
    release["build_id"] = json!("a".repeat(161));
    assert_eq!(
        fetch_local("200 OK", "", catalog(vec![release])),
        Outcome::Failed
    );
}

#[test]
fn offline_timeout_cancellation_and_single_inflight_are_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/releases", listener.local_addr().unwrap());
    drop(listener);
    let service = CatalogService {
        busy: Arc::new(AtomicBool::new(false)),
        endpoint,
        deadline: Duration::from_millis(200),
    };
    assert_eq!(
        service.check(Channel::Stable, Platform::MacArm64, &AtomicBool::new(false)),
        Outcome::Offline
    );
    let (service, thread) = serve("200 OK", "", catalog(vec![]), Duration::from_millis(350));
    let service = Arc::new(service);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = {
        let service = service.clone();
        let cancelled = cancelled.clone();
        std::thread::spawn(move || service.check(Channel::Stable, Platform::MacArm64, &cancelled))
    };
    let start = Instant::now();
    while !service.busy.load(Ordering::Acquire) {
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
    assert_eq!(
        service.check(Channel::Stable, Platform::MacArm64, &AtomicBool::new(false)),
        Outcome::Unavailable
    );
    cancelled.store(true, Ordering::Release);
    assert_eq!(worker.join().unwrap(), Outcome::Unavailable);
    // Cancellation never frees admission while the old transport may still run.
    assert_eq!(
        service.check(Channel::Stable, Platform::MacArm64, &AtomicBool::new(false)),
        Outcome::Unavailable
    );
    thread.join().unwrap();
    let (service, thread) = serve("200 OK", "", catalog(vec![]), Duration::from_millis(350));
    let start = Instant::now();
    assert_eq!(
        service.check(Channel::Stable, Platform::MacArm64, &AtomicBool::new(false)),
        Outcome::Failed
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    thread.join().unwrap();
}
