//! Original counted custody through both metadata policy wrappers, not admission.
use super::tag_wal_preparation::{request, settings, writer};
use crate::sample_sources::SourceTagAssignmentMutation;
use crate::sample_sources::db::source_authority::SourceAuthorityRequest;
use rusqlite::ffi;
use std::{
    ffi::{CStr, c_char, c_void},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct Probe {
    request: Arc<SourceAuthorityRequest>,
    wrong_count: AtomicBool,
    restorations: AtomicUsize,
    deny_rollback: bool,
    deny_policy: bool,
}
unsafe extern "C" fn inspect(
    raw: *mut c_void,
    action: i32,
    a: *const c_char,
    b: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> i32 {
    let probe = unsafe { &*(raw as *const Probe) };
    let name = if a.is_null() {
        b"".as_slice()
    } else {
        unsafe { CStr::from_ptr(a) }.to_bytes()
    };
    if action == ffi::SQLITE_TRANSACTION && name == b"ROLLBACK"
        || action == ffi::SQLITE_PRAGMA && !b.is_null()
    {
        if probe.request.registered_uses_for_tests() != 1 {
            probe.wrong_count.store(true, Ordering::Release);
        }
        if action == ffi::SQLITE_PRAGMA {
            probe.restorations.fetch_add(1, Ordering::AcqRel);
        }
    }
    if probe.deny_rollback && action == ffi::SQLITE_TRANSACTION && name == b"ROLLBACK"
        || probe.deny_policy
            && action == ffi::SQLITE_PRAGMA
            && name == b"temp_store"
            && !b.is_null()
    {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}
fn install(handle: *mut ffi::sqlite3, probe: Option<&Probe>) {
    assert_eq!(
        unsafe {
            ffi::sqlite3_set_authorizer(
                handle,
                probe.map(|_| inspect as _),
                probe.map_or(std::ptr::null_mut(), |p| p as *const Probe as *mut c_void),
            )
        },
        ffi::SQLITE_OK
    );
}
// Installed after genuine retention: no unknown function participates in admission capture.
fn observe_close(
    handle: *mut ffi::sqlite3,
    request: &Arc<SourceAuthorityRequest>,
) -> Arc<AtomicUsize> {
    struct CloseProbe {
        request: Arc<SourceAuthorityRequest>,
        seen: Arc<AtomicUsize>,
    }
    impl Drop for CloseProbe {
        fn drop(&mut self) {
            self.seen
                .store(self.request.registered_uses_for_tests(), Ordering::SeqCst);
        }
    }
    unsafe extern "C" fn invoke(
        ctx: *mut ffi::sqlite3_context,
        _: i32,
        _: *mut *mut ffi::sqlite3_value,
    ) {
        unsafe {
            ffi::sqlite3_result_int(ctx, 0);
        }
    }
    unsafe extern "C" fn destroy(raw: *mut c_void) {
        unsafe {
            drop(Box::from_raw(raw.cast::<CloseProbe>()));
        }
    }
    let seen = Arc::new(AtomicUsize::new(usize::MAX));
    let data = Box::new(CloseProbe {
        request: Arc::clone(request),
        seen: Arc::clone(&seen),
    });
    // SQLite owns this Box and invokes its destructor at the actual ordinary connection close.
    assert_eq!(
        unsafe {
            ffi::sqlite3_create_function_v2(
                handle,
                c"metadata_registered_close".as_ptr(),
                0,
                ffi::SQLITE_UTF8,
                Box::into_raw(data).cast(),
                Some(invoke),
                None,
                None,
                Some(destroy),
            )
        },
        ffi::SQLITE_OK
    );
    seen
}
macro_rules! exercise {
    ($prepare:expr, $scenario:expr) => {{
        let (writer, _fixture, _) = writer();
        let original = settings(&writer);
        let handle = unsafe { writer.database_for_tests().connection.handle() };
        let mut authority = writer
            .catalog_database()
            .prepare_source_authority()
            .unwrap();
        let envelope = authority.journal_request().unwrap();
        let mut proof = Some(
            envelope
                .request
                .accept_confirmation(envelope.confirmation)
                .unwrap(),
        );
        let mut held = $prepare(writer);
        let binding = held
            .original_wal_preparation()
            .unwrap()
            .capture_wal_binding(&authority)
            .unwrap();
        held.retain_registered_wal_binding(&binding, &authority)
            .unwrap();
        assert_eq!(envelope.request.registered_uses_for_tests(), 1);
        let probe = Probe {
            request: Arc::clone(&envelope.request),
            wrong_count: AtomicBool::new(false),
            restorations: AtomicUsize::new(0),
            deny_rollback: $scenario == 1 || $scenario == 3,
            deny_policy: $scenario == 2,
        };
        install(handle, Some(&probe));
        match $scenario {
            0 => {
                let writer = held.abort().unwrap();
                assert!(!probe.wrong_count.load(Ordering::Acquire));
                assert!(probe.restorations.load(Ordering::Acquire) >= 4);
                assert_eq!(probe.request.registered_uses_for_tests(), 0);
                install(handle, None);
                assert_eq!(settings(&writer), original);
                drop(writer);
            }
            1 => {
                let failed = held.abort().err().expect("original cleanup must fail");
                assert_eq!(probe.request.registered_uses_for_tests(), 1);
                assert!(!probe.wrong_count.load(Ordering::Acquire));
                install(handle, None);
                let writer = failed.into_writer().unwrap();
                assert_eq!(probe.request.registered_uses_for_tests(), 0);
                assert_eq!(settings(&writer), original);
                drop(writer);
            }
            2 => {
                let failed = held.abort().err().expect("original cleanup must fail");
                assert_eq!(probe.request.registered_uses_for_tests(), 1);
                assert!(!probe.wrong_count.load(Ordering::Acquire));
                assert!(probe.restorations.load(Ordering::Acquire) > 0);
                install(handle, None);
                // Policy poisoning is sticky; clearing the SQLite hook does not authorize reuse.
                let failed = failed
                    .into_writer()
                    .err()
                    .expect("poisoned policy cannot permit reuse");
                assert_eq!(probe.request.registered_uses_for_tests(), 1);
                let seen = observe_close(handle, &probe.request);
                drop(failed);
                assert_eq!(seen.load(Ordering::SeqCst), 1);
                assert_eq!(probe.request.registered_uses_for_tests(), 0);
            }
            3 => {
                let failed = held
                    .abort()
                    .err()
                    .expect("rollback denial retains original owner");
                assert_eq!(probe.request.registered_uses_for_tests(), 1);
                assert!(failed.source_authority_close_ready());
                let seen = observe_close(handle, &probe.request);
                probe
                    .request
                    .begin_drain(proof.take().unwrap().into_drain_marker())
                    .unwrap();
                failed.close_for_retirement(&mut authority).unwrap();
                assert_eq!(seen.load(Ordering::SeqCst), 1);
                assert_eq!(probe.request.registered_uses_for_tests(), 0);
            }
            _ => unreachable!(),
        }
        drop(binding);
        drop(proof);
        drop(authority);
    }};
}
macro_rules! cases {
    ($name:ident, $scenario:expr) => {
        #[test]
        fn $name() {
            exercise!(
                |writer: crate::sample_sources::SourceWriter| {
                    let observation = writer
                        .capture_tag_admission_observation(
                            &writer.current_revision().unwrap(),
                            "old tag",
                            SourceTagAssignmentMutation::Delete,
                        )
                        .unwrap();
                    writer
                        .prepare_tag_wal_reservation(observation, request())
                        .unwrap()
                },
                $scenario
            );
            exercise!(
                |writer: crate::sample_sources::SourceWriter| {
                    let budget = writer
                        .capture_layout_metadata_budget(&writer.current_revision().unwrap())
                        .unwrap();
                    writer
                        .prepare_layout_wal_reservation(budget, request())
                        .unwrap()
                },
                $scenario
            );
        }
    };
}
cases!(
    registered_metadata_count_survives_complete_outer_policy_restoration,
    0
);
cases!(
    registered_metadata_count_survives_actual_rollback_failure_and_retry,
    1
);
cases!(
    registered_metadata_count_survives_sticky_outer_policy_failure_until_close,
    2
);

cases!(
    registered_metadata_draining_close_validates_exact_original_custody,
    3
);
