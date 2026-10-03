//! Actual GPUI -> finite loopback HTTP -> crypto/ZIP verifier -> retained owner.
#[cfg(test)]
mod integration {
    use super::super::*;
    use crate::update_payload_service::tests::{OWNER_TEST, blocked, install, responses, serve};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use sha2::{Digest, Sha256};
    use std::time::Instant;
    use yield_platform::update_catalog::Channel;

    fn tick(cx: &mut VisualTestContext) {
        cx.executor().allow_parking();
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
    }
    fn wait(browser: &Entity<Browser>, cx: &mut VisualTestContext, phase: Phase) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            tick(cx);
            let actual = cx.update(|_, app| browser.read(app).general_settings.payload.phase());
            if actual == Some(phase) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "actual GPUI phase {actual:?} never reached {phase:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn stage(browser: &Entity<Browser>, cx: &mut VisualTestContext) {
        cx.update(|_, app| {
            browser.update(app, |view, cx| {
                view.theme_settings_open = true;
                view.stage_update_payload(cx);
            })
        });
    }
    fn retire(browser: &Entity<Browser>, cx: &mut VisualTestContext) {
        cx.update(|_, app| browser.update(app, |view, cx| view.retire_update_payload(cx)));
        wait(browser, cx, Phase::Unstaged);
    }
    #[gpui::test]
    fn actual_gpui_retains_full_stable_rc_nightly_verified_identity_and_retires_with_unchanged_sentinels(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let temp = tempfile::tempdir().unwrap();
        let paths = ["installed/yield.exe", "profile/settings", "source/library"];
        for path in paths {
            let p = temp.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"preserved").unwrap();
        }
        for (lane, version) in [
            (Channel::Stable, "0.19.2"),
            (Channel::Rc, "0.19.2-rc.3"),
            (Channel::Nightly, "0.19.2-nightly.2026-10-03+01234567"),
        ] {
            let records = responses(version, lane, false);
            let split = records[4]
                .windows(4)
                .position(|b| b == b"\r\n\r\n")
                .unwrap()
                + 4;
            let expected_len = records[4].len() - split;
            let expected_hash: [u8; 32] = Sha256::digest(&records[4][split..]).into();
            let (service, server) = serve(records, lane);
            let original = service.clone();
            cx.update(|_, app| install(app, service));
            stage(&browser, cx);
            cx.update(|_, app| {
                browser.update(app, |view, cx| {
                    let generation = view.general_settings.payload.generation;
                    view.stage_update_payload(cx);
                    assert_eq!(view.general_settings.payload.generation, generation);
                })
            });
            wait(&browser, cx, Phase::Staged);
            assert_eq!(server.join().unwrap().len(), 5);
            cx.update(|_, app| {
                let payload = &browser.read(app).general_settings.payload;
                let observation = payload.observation.as_ref().unwrap();
                assert_eq!(observation.identity.as_ref().unwrap().version, version);
                assert_eq!(observation.archive_len, Some(expected_len));
                assert_eq!(observation.archive_sha256, Some(expected_hash));
                let attempt = payload.attempt.as_ref().unwrap();
                assert!(original.context.owner.owns(attempt));
                assert!(payload.message(Some(&original), true).contains(version));
                assert!(
                    payload
                        .message(Some(&original), true)
                        .contains("Installation and relaunch are unavailable")
                );
            });
            cx.refresh().unwrap();
            assert!(cx.debug_bounds("update-payload-panel").is_some());
            assert!(cx.debug_bounds("verify-update-download").is_some());
            assert!(cx.debug_bounds("retire-update-download").is_some());
            // Actual close handler requests original-byte retirement even if reopened before delivery.
            cx.update(|window, app| {
                browser.update(app, |view, cx| view.close_general_settings(window, cx))
            });
            wait(&browser, cx, Phase::Unstaged);
            for path in paths {
                assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), b"preserved");
            }
            assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 3);
        }
    }

    #[gpui::test]
    fn actual_gpui_corrupt_signed_mismatch_offline_failure_and_retry_never_fabricate_staging(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        for attack in 0..4 {
            let mut records = responses("0.19.2", Channel::Stable, attack == 1);
            if attack == 0 {
                *records[4].last_mut().unwrap() ^= 1;
            }
            if attack == 2 {
                records[0] = b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n".to_vec();
                records.truncate(1);
            }
            if attack == 3 {
                records.clear();
            }
            let (service, server) = serve(records, Channel::Stable);
            if attack == 3 {
                server.join().unwrap();
                cx.update(|_, app| install(app, service));
                stage(&browser, cx);
                wait(&browser, cx, Phase::Offline);
            } else {
                cx.update(|_, app| install(app, service));
                stage(&browser, cx);
                wait(&browser, cx, Phase::Failed);
                server.join().unwrap();
            }
            cx.update(|_, app| {
                let payload = &browser.read(app).general_settings.payload;
                assert!(payload.attempt.is_none());
                assert!(payload.observation.as_ref().unwrap().archive_len.is_none());
            });
        }
        let (service, server) = serve(responses("0.19.2", Channel::Stable, false), Channel::Stable);
        cx.update(|_, app| install(app, service));
        stage(&browser, cx);
        wait(&browser, cx, Phase::Staged);
        server.join().unwrap();
        retire(&browser, cx);
    }

    #[gpui::test]
    fn actual_gpui_blocked_old_io_close_and_facade_replacement_reject_second_view_until_retired(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (old, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let (service, entered, release, server) = blocked(
            responses("0.19.2", Channel::Stable, false).remove(0),
            Channel::Stable,
        );
        cx.update(|_, app| install(app, service));
        stage(&old, cx);
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cx.update(|_, app| {
            old.read(app)
                .general_settings
                .payload
                .attempt
                .as_ref()
                .unwrap()
                .loopback_read_pending()
        }) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        cx.update(|window, app| {
            old.update(app, |view, cx| view.close_general_settings(window, cx))
        });
        wait(&old, cx, Phase::Stopping);
        let (replacement, next_server) =
            serve(responses("0.19.2", Channel::Stable, false), Channel::Stable);
        cx.update(|_, app| install(app, replacement));
        let mut other = cx.cx.clone();
        let (new, new_cx) = other.add_window_view(|_, cx| Browser::new(cx));
        for _ in 0..10 {
            stage(&new, new_cx);
            new_cx.update(|_, app| {
                assert_eq!(
                    new.read(app).general_settings.payload.phase(),
                    Some(Phase::Unavailable)
                )
            });
        }
        cx.update(|_, app| {
            let payload = &old.read(app).general_settings.payload;
            assert_eq!(payload.phase(), Some(Phase::Stopping));
            assert!(
                payload
                    .attempt
                    .as_ref()
                    .unwrap()
                    .observe()
                    .unwrap()
                    .archive_len
                    .is_none()
            );
        });
        release.send(()).unwrap();
        server.join().unwrap();
        wait(&old, cx, Phase::Unstaged);
        stage(&new, new_cx);
        wait(&new, new_cx, Phase::Staged);
        assert_eq!(next_server.join().unwrap().len(), 5);
        retire(&new, new_cx);
    }

    #[gpui::test]
    fn actual_gpui_lane_replacement_stale_generation_and_browser_drop_retire_original_bytes(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let (service, server) = serve(responses("0.19.2-rc.3", Channel::Rc, false), Channel::Rc);
        let owner = service.clone();
        cx.update(|_, app| install(app, service));
        stage(&browser, cx);
        wait(&browser, cx, Phase::Staged);
        server.join().unwrap();
        // Same original context with a changed lane still invalidates retained presentation/custody.
        cx.update(|_, app| {
            let mut replacement = owner.clone();
            replacement.channel = Channel::Nightly;
            app.set_global(replacement);
        });
        wait(&browser, cx, Phase::Unstaged);
        let (service, server) = serve(responses("0.19.2", Channel::Stable, false), Channel::Stable);
        let owner = service.clone();
        cx.update(|_, app| install(app, service));
        stage(&browser, cx);
        wait(&browser, cx, Phase::Staged);
        server.join().unwrap();
        // Invalidate request generation while actual verified bytes exist. The old consumer retires
        // its captured original attempt without delivering a stale Staged result to the replacement.
        let old_attempt = cx.update(|_, app| {
            browser.update(app, |view, _| {
                view.general_settings.payload.generation += 1;
                view.general_settings
                    .payload
                    .attempt
                    .as_ref()
                    .unwrap()
                    .clone()
            })
        });
        tick(cx);
        let deadline = Instant::now() + Duration::from_secs(3);
        while old_attempt.observe().unwrap().phase != Phase::Unstaged {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!owner.context.owner.owns(&old_attempt));
        // Drop the actual state holder, including its GPUI task, while a fresh Staged owner exists.
        cx.update(|_, app| {
            browser.update(app, |view, _| {
                view.general_settings.payload = UpdatePayload::default();
            })
        });
        let (service, server) = serve(responses("0.19.2", Channel::Stable, false), Channel::Stable);
        cx.update(|_, app| install(app, service));
        stage(&browser, cx);
        wait(&browser, cx, Phase::Staged);
        server.join().unwrap();
        let attempt = cx.update(|_, app| {
            browser.update(app, |view, _| {
                let attempt = view
                    .general_settings
                    .payload
                    .attempt
                    .as_ref()
                    .unwrap()
                    .clone();
                view.general_settings.payload = UpdatePayload::default();
                attempt
            })
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while attempt.observe().unwrap().phase != Phase::Unstaged {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        cx.update(|_, app| assert!(browser.read(app).general_settings.payload.attempt.is_none()));
    }

    #[gpui::test]
    fn actual_old_gpui_task_cannot_clear_new_request_when_counter_is_reused(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let mut records = responses("0.19.2", Channel::Stable, false);
        records.extend(responses("0.19.2", Channel::Stable, false));
        let (service, server) = serve(records, Channel::Stable);
        cx.update(|_, app| install(app, service));
        stage(&browser, cx);
        wait(&browser, cx, Phase::Staged);
        // Retain the actual old async task outside the replaced state so a late callback really
        // executes. Resetting the holder reuses generation 1, while the new opaque request differs.
        let (old_task, old_attempt) = cx.update(|_, app| {
            browser.update(app, |view, _| {
                let old_task = view.general_settings.payload.task.take().unwrap();
                let old_attempt = view
                    .general_settings
                    .payload
                    .attempt
                    .as_ref()
                    .unwrap()
                    .clone();
                assert_eq!(view.general_settings.payload.generation, 1);
                view.general_settings.payload = UpdatePayload::default();
                (old_task, old_attempt)
            })
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while old_attempt.observe().unwrap().phase != Phase::Unstaged {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // Source acknowledgement precedes releasing admission by a few instructions. Retry only an
        // unavailable start; this never relaxes owner exclusion or manufactures a retirement flag.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            stage(&browser, cx);
            let started =
                cx.update(|_, app| browser.read(app).general_settings.payload.attempt.is_some());
            if started {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        wait(&browser, cx, Phase::Staged);
        assert_eq!(server.join().unwrap().len(), 10);
        cx.update(|_, app| {
            let state = &browser.read(app).general_settings.payload;
            assert_eq!(state.generation, 1);
            assert!(state.task.is_some());
            assert!(
                state
                    .service
                    .as_ref()
                    .unwrap()
                    .context
                    .owner
                    .owns(state.attempt.as_ref().unwrap())
            );
        });
        drop(old_task);
        retire(&browser, cx);
    }

    #[gpui::test]
    fn actual_gpui_target_guard_retires_real_verified_bytes_before_rendering_staged(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let (mut service, server) =
            serve(responses("0.19.2", Channel::Stable, false), Channel::Stable);
        // Deliberate private caller-contract violation: the real owner still has its immutable Windows
        // target, while the UI context is wrong. This is not a source/key/selection verifier override.
        Arc::get_mut(&mut service.context)
            .unwrap()
            .installed
            .platform = yield_platform::update_catalog::Platform::MacArm64;
        cx.update(|_, app| install(app, service));
        stage(&browser, cx);
        wait(&browser, cx, Phase::Unstaged);
        assert_eq!(server.join().unwrap().len(), 5);
        cx.update(|_, app| {
            let payload = &browser.read(app).general_settings.payload;
            assert!(payload.observation.as_ref().unwrap().archive_len.is_none());
            assert!(
                !payload
                    .message(app.try_global::<UpdatePayloadService>(), true)
                    .contains("is verified and retained")
            );
        });
    }

    #[gpui::test]
    fn shared_pointer_and_native_ax_dispatch_use_real_owner_and_fresh_enablement(
        cx: &mut TestAppContext,
    ) {
        let _guard = OWNER_TEST.lock().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let (service, server) = serve(responses("0.19.2", Channel::Stable, false), Channel::Stable);
        cx.update(|_, app| install(app, service));
        cx.update(|_, app| {
            browser.update(app, |view, cx| {
                assert!(!view.perform_payload_action(DOWNLOAD_ID, cx));
                view.theme_settings_open = true;
                assert_eq!(view.payload_control_enabled(DOWNLOAD_ID, cx), Some(true));
                assert!(!view.perform_payload_action(RETIRE_ID, cx));
                assert!(view.perform_payload_action(DOWNLOAD_ID, cx));
                assert!(!view.perform_payload_action(DOWNLOAD_ID, cx));
                assert_eq!(view.payload_control_enabled(RETIRE_ID, cx), Some(true));
            })
        });
        wait(&browser, cx, Phase::Staged);
        assert_eq!(server.join().unwrap().len(), 5);
        cx.update(|_, app| {
            browser.update(app, |view, cx| {
                assert!(view.perform_payload_action(RETIRE_ID, cx));
            })
        });
        wait(&browser, cx, Phase::Unstaged);
        cx.update(|_, app| {
            browser.update(app, |view, cx| {
                assert_eq!(view.payload_control_enabled(DOWNLOAD_ID, cx), Some(true));
                assert_eq!(view.payload_control_enabled(RETIRE_ID, cx), Some(false));
            })
        });
    }
}
