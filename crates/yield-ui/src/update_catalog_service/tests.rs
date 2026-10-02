use super::*;
use std::net::TcpListener;
use std::time::Duration;

fn context(version: &'static str, channel: UpdateChannel) -> InstalledContext {
    InstalledContext {
        request: UpdateCheckRequest {
            installed: UpdateVersion(19, 1, 0),
            build: UpdateBuild {
                number: 44,
                commit: [1; 20],
            },
            channel,
            platform: UpdatePlatform::MacArm64,
        },
        full_version: version,
        semantic_version: semver::Version::parse(version).unwrap(),
    }
}

pub(crate) fn fixture_service(
    version: &'static str,
    channel: UpdateChannel,
    catalog: CatalogService,
) -> UpdateCheckService {
    let installed = context(version, channel);
    let request = installed.request;
    UpdateCheckService {
        channel,
        platform: request.platform,
        installed_version: request.installed,
        installed_build: request.build,
        checker: Arc::new(ProductionChecker { installed, catalog }),
    }
}

pub(crate) fn body(version: &str, number: u64, commit: &str, asset: &str) -> Vec<u8> {
    format!(r#"{{"releases":[{{"build_id":"yield-{version}","build_number":{number},"version":"{version}","commit":"{commit}","files":[{{"name":"{asset}"}}]}}]}}"#).into_bytes()
}

pub(crate) fn server(status: &str, body: Vec<u8>) -> (CatalogService, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let catalog =
        CatalogService::for_loopback_tests(listener.local_addr().unwrap(), Duration::from_secs(1))
            .unwrap();
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let thread = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|s| s == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() <= 4096);
        }
        assert!(request.starts_with(b"GET /releases HTTP/1.1\r\n"));
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(&body);
    });
    (catalog, thread)
}

fn candidate(version: &str, number: u64, commit: [u8; 20], channel: Channel) -> Candidate {
    Candidate {
        version: [19, 1, 0],
        release_version: version.into(),
        build_number: number,
        commit,
        channel,
        platform: Platform::MacArm64,
    }
}

#[test]
fn complete_prerelease_identity_is_checked_before_core_only_ui_projection() {
    for (lane, transport_lane, installed, same_core_other, older, newer) in [
        (
            UpdateChannel::Rc,
            Channel::Rc,
            "19.1.0-rc.2",
            "19.1.0-rc.3",
            "19.1.0-rc.1",
            "19.1.0-rc.3",
        ),
        (
            UpdateChannel::Nightly,
            Channel::Nightly,
            "19.1.0-nightly.2026-10-02+01010101",
            "19.1.0-nightly.2026-10-03+01010101",
            "19.1.0-nightly.2026-10-01+02020202",
            "19.1.0-nightly.2026-10-03+02020202",
        ),
    ] {
        let installed = context(installed, lane);
        let request = installed.request;
        let checker = ProductionChecker {
            installed,
            catalog: CatalogService::default(),
        };
        assert!(matches!(
            checker.classify(
                request,
                candidate(checker.installed.full_version, 44, [1; 20], transport_lane)
            ),
            UpdateCheckOutcome::Candidate { .. }
        ));
        assert_eq!(
            checker.classify(
                request,
                candidate(same_core_other, 44, [1; 20], transport_lane)
            ),
            UpdateCheckOutcome::Failed
        );
        assert_eq!(
            checker.classify(request, candidate(older, 45, [2; 20], transport_lane)),
            UpdateCheckOutcome::Failed
        );
        assert!(matches!(
            checker.classify(request, candidate(newer, 45, [2; 20], transport_lane)),
            UpdateCheckOutcome::Candidate {
                build: UpdateBuild { number: 45, .. },
                ..
            }
        ));
        assert_eq!(
            checker.classify(request, candidate(newer, 45, [1; 20], transport_lane)),
            UpdateCheckOutcome::Failed
        );
    }
}

#[test]
fn immutable_installed_request_substitution_and_cancellation_reject_before_io() {
    let installed = context("19.1.0", UpdateChannel::Stable);
    let request = installed.request;
    let checker = ProductionChecker {
        installed,
        catalog: CatalogService::default(),
    };
    for substituted in [
        UpdateCheckRequest {
            installed: UpdateVersion(19, 2, 0),
            ..request
        },
        UpdateCheckRequest {
            build: UpdateBuild {
                number: 45,
                commit: [1; 20],
            },
            ..request
        },
        UpdateCheckRequest {
            platform: UpdatePlatform::WindowsX64,
            ..request
        },
    ] {
        assert_eq!(
            checker.check(substituted, &AtomicBool::new(false)),
            UpdateCheckOutcome::Failed
        );
    }
    assert_eq!(
        checker.check(request, &AtomicBool::new(true)),
        UpdateCheckOutcome::Unavailable
    );
}

#[test]
fn actual_transport_adapter_preserves_complete_rc_result_and_incomplete_identity() {
    let commit = "01".repeat(20);
    let (catalog, thread) = server(
        "200 OK",
        body(
            "19.1.0-rc.2",
            44,
            &commit,
            "yield-19.1.0-rc.2-macos-aarch64.zip",
        ),
    );
    let service = fixture_service("19.1.0-rc.2", UpdateChannel::Rc, catalog);
    let request = context("19.1.0-rc.2", UpdateChannel::Rc).request;
    assert!(matches!(
        service.checker.check(request, &AtomicBool::new(false)),
        UpdateCheckOutcome::Candidate {
            build: UpdateBuild { number: 44, .. },
            ..
        }
    ));
    thread.join().unwrap();
    let (catalog,thread)=server("200 OK",br#"{"releases":[{"build_id":"yield-19.1.0","build_number":44,"version":"19.1.0","files":[{"name":"yield-19.1.0-macos-aarch64.zip"}]}]}"#.to_vec());
    let service = fixture_service("19.1.0", UpdateChannel::Stable, catalog);
    assert_eq!(
        service.checker.check(
            context("19.1.0", UpdateChannel::Stable).request,
            &AtomicBool::new(false)
        ),
        UpdateCheckOutcome::Unavailable
    );
    thread.join().unwrap();
}

#[gpui::test]
fn startup_registration_requires_compiled_identity_and_retains_exact_fixture_context(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|app| {
        register_context(app, None, CatalogService::default());
        assert!(app.try_global::<UpdateCheckService>().is_none());
    });
    cx.update(|app| {
        register_context(
            app,
            Some(context("19.1.0-rc.2", UpdateChannel::Rc)),
            CatalogService::default(),
        );
        let service = app.try_global::<UpdateCheckService>().unwrap();
        assert_eq!(service.installed_build.number, 44);
        assert_eq!(service.channel, UpdateChannel::Rc);
    });
    // The ordinary Linux development artifact has no qualified packaged release target.
    #[cfg(target_os = "linux")]
    assert!(InstalledContext::compiled().is_none());
}
