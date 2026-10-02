use super::*;
use crate::update_check::{UpdateBuild, UpdateChannel, UpdateChecker, UpdatePlatform};
use gpui::TestAppContext;

#[gpui::test]
fn production_catalog_runs_in_actual_settings_worker_and_retry_replaces_offline(
    cx: &mut TestAppContext,
) {
    use crate::update_catalog_service::tests::{body, fixture_service, server};
    use std::net::TcpListener;
    use std::time::Duration;
    use yield_platform::update_catalog::CatalogService;
    cx.set_global(crate::theme::ActiveTheme::default());
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let offline = CatalogService::for_loopback_tests(address, Duration::from_secs(1)).unwrap();
    cx.update(|_, app| {
        app.set_global(fixture_service("19.1.0", UpdateChannel::Stable, offline));
        browser.update(app, |view, cx| {
            view.theme_settings_open = true;
            view.check_for_updates(cx);
            assert_eq!(view.general_settings.updates.state, State::Checking);
        });
    });
    cx.run_until_parked();
    cx.update(|_, app| {
        assert_eq!(
            browser.read(app).general_settings.updates.state,
            State::Offline
        )
    });
    for (status, bytes, expected) in [
        (
            "200 OK",
            body(
                "19.1.0",
                44,
                &"01".repeat(20),
                "yield-19.1.0-macos-aarch64.zip",
            ),
            State::Current,
        ),
        (
            "200 OK",
            body(
                "19.1.0",
                45,
                &"02".repeat(20),
                "yield-19.1.0-macos-aarch64.zip",
            ),
            State::Newer(UpdateVersion(19, 1, 0), 45),
        ),
        ("500 Error", Vec::new(), State::Failed),
        ("200 OK", br#"{"releases":[]}"#.to_vec(), State::Unavailable),
    ] {
        let (catalog, thread) = server(status, bytes);
        cx.update(|_, app| {
            app.set_global(fixture_service("19.1.0", UpdateChannel::Stable, catalog));
            browser.update(app, |view, cx| {
                view.check_for_updates(cx);
                let generation = view.general_settings.updates.generation;
                view.check_for_updates(cx);
                assert_eq!(view.general_settings.updates.generation, generation);
                assert_eq!(view.general_settings.updates.state, State::Checking);
                assert!(view.general_settings.updates.task.is_some());
            });
        });
        cx.run_until_parked();
        thread.join().unwrap();
        cx.update(|_, app| {
            assert_eq!(browser.read(app).general_settings.updates.state, expected);
            assert!(browser.read(app).general_settings.updates.task.is_none());
        });
        cx.refresh().unwrap();
        assert!(cx.debug_bounds("update-check-panel").is_some());
    }
}

fn request() -> UpdateCheckRequest {
    UpdateCheckRequest {
        installed: UpdateVersion(0, 19, 1),
        build: UpdateBuild {
            number: 1,
            commit: [1; 20],
        },
        channel: UpdateChannel::Stable,
        platform: UpdatePlatform::MacArm64,
    }
}
fn candidate(version: UpdateVersion) -> UpdateCheckOutcome {
    UpdateCheckOutcome::Candidate {
        version,
        build: if version == request().installed {
            request().build
        } else {
            UpdateBuild {
                number: 2,
                commit: [2; 20],
            }
        },
        channel: UpdateChannel::Stable,
        platform: UpdatePlatform::MacArm64,
    }
}

#[test]
fn update_check_fixture_truthful_current_newer_offline_failure_and_lane_rejection() {
    let request = request();
    for (outcome, state, text) in [
        (candidate(request.installed), State::Current, "up to date"),
        (
            candidate(UpdateVersion(0, 20, 0)),
            State::Newer(UpdateVersion(0, 20, 0), 2),
            "Automatic installation is unavailable",
        ),
        (
            UpdateCheckOutcome::Offline,
            State::Offline,
            "Reconnect and try again",
        ),
        (
            UpdateCheckOutcome::Failed,
            State::Failed,
            "installed app is unchanged",
        ),
        (
            UpdateCheckOutcome::Unavailable,
            State::Unavailable,
            "unavailable",
        ),
        (
            candidate(UpdateVersion(0, 18, 0)),
            State::Mismatch,
            "did not match",
        ),
        (
            UpdateCheckOutcome::Candidate {
                version: request.installed,
                build: request.build,
                channel: UpdateChannel::Nightly,
                platform: request.platform,
            },
            State::Mismatch,
            "did not match",
        ),
        (
            UpdateCheckOutcome::Candidate {
                build: UpdateBuild {
                    number: 1,
                    commit: [2; 20],
                },
                version: request.installed,
                channel: request.channel,
                platform: request.platform,
            },
            State::Mismatch,
            "did not match",
        ),
        (
            UpdateCheckOutcome::Candidate {
                version: request.installed,
                build: request.build,
                channel: request.channel,
                platform: UpdatePlatform::WindowsX64,
            },
            State::Mismatch,
            "did not match",
        ),
    ] {
        let mut check = UpdateCheck {
            generation: 1,
            state: State::Checking,
            ..Default::default()
        };
        assert!(check.finish(1, request, outcome));
        assert_eq!(check.state, state);
        assert!(state.message().contains(text));
        assert!(
            !check.finish(1, request, UpdateCheckOutcome::Failed),
            "terminal duplicate rejected"
        );
        assert_eq!(check.state, state);
    }
    for channel in [UpdateChannel::Nightly, UpdateChannel::Rc] {
        let mut check = UpdateCheck {
            generation: 1,
            state: State::Checking,
            ..Default::default()
        };
        let request = UpdateCheckRequest { channel, ..request };
        assert!(check.finish(
            1,
            request,
            UpdateCheckOutcome::Candidate {
                version: request.installed,
                build: request.build,
                channel,
                platform: request.platform
            }
        ));
        assert_eq!(
            check.state,
            State::Current,
            "exact build identity is required even when version core is unchanged"
        );
    }
}

#[test]
fn update_check_late_completion_cancel_and_retry_preserve_newer_state() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut check = UpdateCheck {
        generation: 1,
        state: State::Checking,
        cancellation: Some(Cancellation(cancel.clone())),
        ..Default::default()
    };
    check.close();
    assert!(cancel.load(Ordering::Acquire));
    assert!(!check.finish(1, request(), candidate(request().installed)));
    assert_eq!(check.state, State::Idle);
    check.generation = 2;
    check.cancellation = Some(Cancellation(Arc::new(AtomicBool::new(false))));
    check.state = State::Checking;
    assert!(!check.finish(1, request(), UpdateCheckOutcome::Failed));
    assert!(check.finish(2, request(), UpdateCheckOutcome::Offline));
    assert!(!check.finish(2, request(), candidate(UpdateVersion(9, 0, 0))));
    assert_eq!(check.state, State::Offline);
}

#[test]
fn update_check_same_version_build_changes_and_unknown_identity_never_fabricate_current() {
    let request = request();
    for (build, expected) in [
        (
            UpdateBuild {
                number: 2,
                commit: [2; 20],
            },
            State::Newer(request.installed, 2),
        ),
        (
            UpdateBuild {
                number: 1,
                commit: [2; 20],
            },
            State::Mismatch,
        ),
        (
            UpdateBuild {
                number: 2,
                commit: request.build.commit,
            },
            State::Mismatch,
        ),
        (
            UpdateBuild {
                number: 0,
                commit: [0; 20],
            },
            State::Mismatch,
        ),
    ] {
        let mut check = UpdateCheck {
            generation: 1,
            state: State::Checking,
            ..Default::default()
        };
        assert!(check.finish(
            1,
            request,
            UpdateCheckOutcome::Candidate {
                version: request.installed,
                build,
                channel: request.channel,
                platform: request.platform,
            }
        ));
        assert_eq!(check.state, expected);
    }
    let mut check = UpdateCheck {
        generation: 1,
        state: State::Checking,
        ..Default::default()
    };
    assert!(check.finish(
        1,
        UpdateCheckRequest {
            build: UpdateBuild {
                number: 0,
                commit: [0; 20]
            },
            ..request
        },
        candidate(request.installed)
    ));
    assert_eq!(check.state, State::Unavailable);
}

struct FixtureChecker {
    outcome: UpdateCheckOutcome,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl UpdateChecker for FixtureChecker {
    fn check(&self, request: UpdateCheckRequest, cancelled: &AtomicBool) -> UpdateCheckOutcome {
        assert_eq!(request.installed, UpdateVersion(0, 19, 1));
        assert!(!cancelled.load(Ordering::Acquire));
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.outcome
    }
}

#[gpui::test]
fn update_check_actual_settings_worker_renders_fixture_results_and_coalesces(
    cx: &mut TestAppContext,
) {
    cx.set_global(crate::theme::ActiveTheme::default());
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    cx.update(|_, app| {
        browser.update(app, |view, cx| {
            view.theme_settings_open = true;
            view.check_for_updates(cx);
            assert_eq!(view.general_settings.updates.state, State::Unavailable);
            assert!(view.general_settings.updates.task.is_none());
        })
    });
    for (outcome, expected) in [
        (candidate(request().installed), State::Current),
        (
            candidate(UpdateVersion(0, 20, 0)),
            State::Newer(UpdateVersion(0, 20, 0), 2),
        ),
        (UpdateCheckOutcome::Offline, State::Offline),
        (UpdateCheckOutcome::Failed, State::Failed),
    ] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        cx.update(|_, app| {
            app.set_global(UpdateCheckService {
                channel: UpdateChannel::Stable,
                platform: UpdatePlatform::MacArm64,
                installed_build: request().build,
                installed_version: request().installed,
                checker: Arc::new(FixtureChecker {
                    outcome,
                    calls: calls.clone(),
                }),
            });
            browser.update(app, |view, cx| {
                view.check_for_updates(cx);
                view.check_for_updates(cx);
                assert_eq!(view.general_settings.updates.state, State::Checking);
                assert!(view.general_settings.updates.task.is_some());
            });
        });
        cx.run_until_parked();
        cx.update(|_, app| {
            let view = browser.read(app);
            assert_eq!(view.general_settings.updates.state, expected);
            assert!(view.general_settings.updates.task.is_none());
            assert_eq!(calls.load(Ordering::Acquire), 1);
        });
        cx.refresh().unwrap();
        assert!(cx.debug_bounds("update-check-panel").is_some());
        assert!(cx.debug_bounds("check-for-updates").is_some());
    }
}

#[gpui::test]
fn update_check_actual_worker_rejects_closed_panel_and_replaced_service(cx: &mut TestAppContext) {
    cx.set_global(crate::theme::ActiveTheme::default());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let service = UpdateCheckService {
        channel: UpdateChannel::Stable,
        platform: UpdatePlatform::MacArm64,
        installed_build: request().build,
        installed_version: request().installed,
        checker: Arc::new(FixtureChecker {
            outcome: candidate(request().installed),
            calls,
        }),
    };
    cx.set_global(service.clone());
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    cx.update(|window, app| {
        browser.update(app, |view, cx| {
            view.theme_settings_open = true;
            view.check_for_updates(cx);
            view.close_general_settings(window, cx);
            assert!(
                view.general_settings.updates.task.is_some(),
                "custody retained until worker returns"
            );
            view.theme_settings_open = true;
            let generation = view.general_settings.updates.generation;
            view.check_for_updates(cx);
            assert_eq!(
                view.general_settings.updates.generation, generation,
                "no second worker during drain"
            );
        })
    });
    cx.run_until_parked();
    cx.update(|_, app| {
        browser.update(app, |view, cx| {
            assert_eq!(view.general_settings.updates.state, State::Idle);
            assert!(view.general_settings.updates.task.is_none());
            view.check_for_updates(cx);
        })
    });
    // A completion from this prior service cannot describe a newly selected lane/checker.
    cx.update(|_, app| {
        app.set_global(UpdateCheckService {
            channel: UpdateChannel::Nightly,
            ..service
        })
    });
    cx.run_until_parked();
    cx.update(|_, app| {
        assert_eq!(
            browser.read(app).general_settings.updates.state,
            State::Unavailable
        );
        assert!(browser.read(app).general_settings.updates.task.is_none());
    });
}

#[test]
fn update_check_completed_projection_never_reuses_another_service_lane() {
    let service = UpdateCheckService {
        channel: UpdateChannel::Stable,
        platform: UpdatePlatform::MacArm64,
        installed_build: request().build,
        installed_version: request().installed,
        checker: Arc::new(FixtureChecker {
            outcome: candidate(request().installed),
            calls: Default::default(),
        }),
    };
    let check = UpdateCheck {
        state: State::Current,
        service: Some(service.clone()),
        ..Default::default()
    };
    assert_eq!(check.projected_state(Some(&service)), State::Current);
    assert_eq!(check.projected_state(None), State::Unavailable);
    let changed_lane = UpdateCheckService {
        platform: UpdatePlatform::WindowsX64,
        ..service.clone()
    };
    assert_eq!(
        check.projected_state(Some(&changed_lane)),
        State::Unavailable
    );
    let changed_checker = UpdateCheckService {
        checker: Arc::new(FixtureChecker {
            outcome: UpdateCheckOutcome::Failed,
            calls: Default::default(),
        }),
        ..service
    };
    assert_eq!(
        check.projected_state(Some(&changed_checker)),
        State::Unavailable
    );
}

fn run_catalog_case(
    cx: &mut TestAppContext,
    installed: &'static str,
    lane: UpdateChannel,
    releases: Vec<String>,
    expected: State,
) {
    use crate::update_catalog_service::tests::{fixture_service, server};
    let bytes = format!(r#"{{"releases":[{}]}}"#, releases.join(",")).into_bytes();
    let (catalog, thread) = server("200 OK", bytes);
    cx.set_global(crate::theme::ActiveTheme::default());
    let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
    cx.update(|_, app| {
        app.set_global(fixture_service(installed, lane, catalog));
        browser.update(app, |view, cx| {
            view.theme_settings_open = true;
            view.check_for_updates(cx);
            assert_eq!(view.general_settings.updates.state, State::Checking);
        });
    });
    cx.run_until_parked();
    thread.join().unwrap();
    cx.update(|_, app| {
        let updates = &browser.read(app).general_settings.updates;
        assert_eq!(updates.state, expected, "installed {installed}");
        assert!(updates.task.is_none());
        if let State::Newer(version, _) = expected {
            assert!(updates.state.message().contains(&version.to_string()));
        }
    });
    cx.refresh().unwrap();
    assert!(cx.debug_bounds("update-check-panel").is_some());
}

fn catalog_release(version: &str, build: u64, commit: &str, asset: &str) -> String {
    let bytes = crate::update_catalog_service::tests::body(version, build, commit, asset);
    String::from_utf8(bytes)
        .unwrap()
        .strip_prefix(r#"{"releases":["#)
        .unwrap()
        .strip_suffix("]}")
        .unwrap()
        .to_owned()
}

#[gpui::test]
fn renumbering_legacy_install_reaches_actual_catalog_adapter_and_browser_worker(
    cx: &mut TestAppContext,
) {
    run_catalog_case(
        cx,
        "19.1.9",
        UpdateChannel::Stable,
        vec![catalog_release(
            "0.19.1",
            45,
            &"02".repeat(20),
            "yield-0.19.1-macos-aarch64.zip",
        )],
        State::Newer(UpdateVersion(0, 19, 1), 45),
    );
}

#[gpui::test]
fn renumbering_mixed_catalog_cannot_mask_new_line_with_higher_historical_build(
    cx: &mut TestAppContext,
) {
    run_catalog_case(
        cx,
        "0.19.1",
        UpdateChannel::Stable,
        vec![
            catalog_release(
                "0.20.0",
                45,
                &"02".repeat(20),
                "yield-0.20.0-macos-aarch64.zip",
            ),
            catalog_release(
                "19.1.99",
                999,
                &"03".repeat(20),
                "yield-19.1.99-macos-aarch64.zip",
            ),
        ],
        State::Newer(UpdateVersion(0, 20, 0), 45),
    );
}

#[gpui::test]
fn renumbering_actual_worker_preserves_ordinary_order_and_ignores_only_colliding_line(
    cx: &mut TestAppContext,
) {
    for (installed, candidate, expected) in [
        ("19.1.9", "0.19.0", State::Failed),
        ("19.2.0", "0.20.0", State::Failed),
        ("19.0.0", "0.20.0", State::Failed),
        ("20.0.0", "0.20.0", State::Failed),
        ("0.20.0", "0.19.1", State::Failed),
        (
            "0.19.1",
            "19.2.0",
            State::Newer(UpdateVersion(19, 2, 0), 45),
        ),
        (
            "18.9.0",
            "19.1.0",
            State::Newer(UpdateVersion(19, 1, 0), 45),
        ),
        (
            "0.19.0",
            "19.1.0",
            State::Newer(UpdateVersion(19, 1, 0), 45),
        ),
    ] {
        run_catalog_case(
            cx,
            installed,
            UpdateChannel::Stable,
            vec![catalog_release(
                candidate,
                45,
                &"02".repeat(20),
                &format!("yield-{candidate}-macos-aarch64.zip"),
            )],
            expected,
        );
    }
    for releases in [
        vec![catalog_release(
            "19.1.99",
            999,
            &"03".repeat(20),
            "yield-19.1.99-macos-aarch64.zip",
        )],
        vec![
            catalog_release(
                "19.1.99",
                999,
                &"03".repeat(20),
                "yield-19.1.99-macos-aarch64.zip",
            ),
            catalog_release(
                "0.19.1",
                44,
                &"01".repeat(20),
                "yield-0.19.1-macos-aarch64.zip",
            ),
        ],
    ] {
        let expected = if releases.len() == 1 {
            State::Unavailable
        } else {
            State::Current
        };
        run_catalog_case(cx, "0.19.1", UpdateChannel::Stable, releases, expected);
    }
}

#[gpui::test]
fn renumbering_actual_worker_cannot_reset_build_or_reuse_full_commit_identity(
    cx: &mut TestAppContext,
) {
    for (build, commit, expected) in [
        (0, "02".repeat(20), State::Failed),
        (43, "02".repeat(20), State::Failed),
        (44, "02".repeat(20), State::Failed),
        (45, "01".repeat(20), State::Failed),
        (45, "00".repeat(20), State::Failed),
        (45, "02020202".into(), State::Failed),
        (
            45,
            "01".repeat(4) + &"02".repeat(16),
            State::Newer(UpdateVersion(0, 19, 1), 45),
        ),
    ] {
        run_catalog_case(
            cx,
            "19.1.9",
            UpdateChannel::Stable,
            vec![catalog_release(
                "0.19.1",
                build,
                &commit,
                "yield-0.19.1-macos-aarch64.zip",
            )],
            expected,
        );
    }
    run_catalog_case(
        cx,
        "19.1.9",
        UpdateChannel::Stable,
        vec![catalog_release(
            "0.19.1",
            45,
            &"02".repeat(20),
            "yield-0.19.1-windows-x86_64.zip",
        )],
        State::Unavailable,
    );
}

#[gpui::test]
fn renumbering_actual_worker_keeps_full_prerelease_lane_and_nightly_identity(
    cx: &mut TestAppContext,
) {
    for (installed, candidate, lane, build, commit, expected) in [
        (
            "19.1.9-rc.9",
            "0.19.1-rc.2",
            UpdateChannel::Rc,
            45,
            "02".repeat(20),
            State::Newer(UpdateVersion(0, 19, 1), 45),
        ),
        (
            "0.19.1-rc.2",
            "0.19.1-rc.1",
            UpdateChannel::Rc,
            45,
            "02".repeat(20),
            State::Failed,
        ),
        (
            "0.19.1-rc.2",
            "0.19.1-rc.2",
            UpdateChannel::Rc,
            44,
            "01".repeat(20),
            State::Current,
        ),
        (
            "0.19.1-rc.2",
            "0.19.1-rc.3",
            UpdateChannel::Rc,
            45,
            "02".repeat(20),
            State::Newer(UpdateVersion(0, 19, 1), 45),
        ),
        (
            "19.1.9",
            "0.19.1-rc.2",
            UpdateChannel::Stable,
            45,
            "02".repeat(20),
            State::Unavailable,
        ),
        (
            "19.1.9-nightly.2026-10-02+01010101",
            "0.19.1-nightly.2026-10-03+02020202",
            UpdateChannel::Nightly,
            45,
            "02".repeat(20),
            State::Newer(UpdateVersion(0, 19, 1), 45),
        ),
        (
            "0.19.1-nightly.2026-10-02+01010101",
            "0.19.1-nightly.2026-10-01+02020202",
            UpdateChannel::Nightly,
            45,
            "02".repeat(20),
            State::Failed,
        ),
        (
            "19.1.9-nightly.2026-10-02+01010101",
            "0.19.1-nightly.2026-02-30+02020202",
            UpdateChannel::Nightly,
            45,
            "02".repeat(20),
            State::Failed,
        ),
        (
            "19.1.9-nightly.2026-10-02+01010101",
            "0.19.1-nightly.2026-10-03+abcdef01",
            UpdateChannel::Nightly,
            45,
            "02".repeat(20),
            State::Failed,
        ),
    ] {
        let semantic = semver::Version::parse(candidate).unwrap();
        let asset = if candidate.contains("nightly") {
            "yield-nightly-macos-aarch64.zip".into()
        } else if !semantic.pre.is_empty() {
            format!(
                "yield-{}.{}.{}-{}-macos-aarch64.zip",
                semantic.major, semantic.minor, semantic.patch, semantic.pre
            )
        } else {
            format!("yield-{candidate}-macos-aarch64.zip")
        };
        run_catalog_case(
            cx,
            installed,
            lane,
            vec![catalog_release(candidate, build, &commit, &asset)],
            expected,
        );
    }
}
