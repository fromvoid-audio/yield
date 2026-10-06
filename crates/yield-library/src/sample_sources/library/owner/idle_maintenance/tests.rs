use super::super::{WriterCommand, dispatch_metrics::DispatchMetrics};
use super::*;
use std::sync::{Arc, mpsc};

#[test]
fn idle_pacing_waits_resets_and_skips_missed_ticks_without_catchup() {
    let now = Instant::now();
    let mut pacing = Pacing::new(now);
    let (_sender, receiver) = mpsc::sync_channel(1);
    let lifecycle = AtomicU8::new(WriterLifecycle::Available as u8);
    let gate = Mutex::new(());
    for delta in [Duration::ZERO, INTERVAL - Duration::from_nanos(1)] {
        assert!(matches!(
            pacing.poll(now + delta, &receiver, &lifecycle, &gate),
            Poll::Wait
        ));
    }
    assert!(matches!(
        pacing.poll(now + INTERVAL, &receiver, &lifecycle, &gate),
        Poll::Maintain
    ));
    assert!(matches!(
        pacing.poll(now + INTERVAL, &receiver, &lifecycle, &gate),
        Poll::Wait
    ));
    pacing.activity(now + INTERVAL + Duration::from_secs(1));
    assert!(matches!(
        pacing.poll(now + INTERVAL * 2, &receiver, &lifecycle, &gate),
        Poll::Wait
    ));
    let late = now + INTERVAL * 20;
    assert!(matches!(
        pacing.poll(late, &receiver, &lifecycle, &gate),
        Poll::Maintain
    ));
    assert!(matches!(
        pacing.poll(late, &receiver, &lifecycle, &gate),
        Poll::Wait
    ));
    assert!(matches!(
        pacing.poll(now, &receiver, &lifecycle, &gate),
        Poll::Wait
    ));
}

#[test]
fn idle_admission_prefers_arrived_command_preserves_dispatch_and_refuses_closing() {
    let now = Instant::now();
    let mut pacing = Pacing::new(now);
    let (sender, receiver) = mpsc::sync_channel(1);
    let metrics = Arc::new(DispatchMetrics::default());
    let (reply, _) = mpsc::sync_channel(1);
    assert!(
        metrics
            .try_send(&sender, WriterCommand::LoadUiPreferences { result: reply })
            .is_ok()
    );
    let lifecycle = AtomicU8::new(WriterLifecycle::Closing as u8);
    let gate = Mutex::new(());
    assert!(matches!(
        pacing.poll(now + INTERVAL, &receiver, &lifecycle, &gate),
        Poll::Wait
    ));
    assert_eq!(metrics.snapshot().queued, 1);
    lifecycle.store(WriterLifecycle::Available as u8, Ordering::Release);
    let Poll::Command(queued) = pacing.poll(now + INTERVAL, &receiver, &lifecycle, &gate) else {
        panic!("queued command must win")
    };
    let (_command, dispatch) = queued.received();
    dispatch.record();
    let observed = metrics.snapshot();
    assert_eq!(observed.queued, 0);
    assert_eq!(observed.dequeued, 1);
    assert_eq!(observed.abandoned, 0);
    drop(sender);
    assert!(matches!(
        pacing.poll(now + INTERVAL, &receiver, &lifecycle, &gate),
        Poll::Disconnected
    ));
}

#[test]
fn idle_global_wal_below_threshold_observation_is_paced_on_original_database() {
    use super::super::LibraryDatabase;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use crate::test_runtime::TestRuntimeGuard;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::AtomicUsize;
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("idle-below-threshold");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let database = LibraryDatabase::open_for_profile_guard(&guard, None).unwrap();
    let observed = Arc::new(AtomicUsize::new(0));
    let hook_observed = observed.clone();
    database
        .connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Pragma {
                    pragma_name: "journal_mode",
                    ..
                }
            ) {
                hook_observed.fetch_add(1, Ordering::SeqCst);
            }
            Authorization::Allow
        }));
    let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
    let lifecycle = std::sync::atomic::AtomicU8::new(WriterLifecycle::Available as u8);
    let gate = std::sync::Mutex::new(());
    let now = Instant::now();
    let mut pacing = Pacing::new(now);
    for tick in 0..=640 {
        if matches!(
            pacing.poll(
                now + Duration::from_millis(tick * 25),
                &receiver,
                &lifecycle,
                &gate
            ),
            Poll::Maintain
        ) {
            database.maintain_while_idle(&guard).unwrap();
        }
    }
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert!(!database.ordinary_commit_pending_for_test());
}
