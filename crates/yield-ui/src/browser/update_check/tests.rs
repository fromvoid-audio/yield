use super::*;
use crate::update_check::{UpdateBuild, UpdateChannel, UpdateChecker, UpdatePlatform};
use gpui::TestAppContext;

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
