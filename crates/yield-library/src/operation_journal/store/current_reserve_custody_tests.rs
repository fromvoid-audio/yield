use super::super::LegacyReserveFailureSink;
use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::SourceDatabase,
};
use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

#[test]
fn current_original_opened_unwind_custody_component_keeps_descriptor_until_sink_drop() {
    for bound in [false, true] {
        let app = tempfile::tempdir().unwrap();
        let _app = AppRootGuard::set(app.path().to_owned()).unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let media = tempfile::tempdir().unwrap();
        let database = tempfile::tempdir().unwrap();
        let source = SourceDatabase::open_for_source_write_with_profile_guard(
            media.path(),
            &profile,
            database.path(),
        )
        .unwrap();
        let (request, guard) = source.prepare_source_authority().unwrap().split();
        let original = request
            .accept_confirmation(guard.confirm().unwrap())
            .unwrap();
        let scope = request
            .reserve_use(request.media_root(), &original, &profile)
            .unwrap();
        let sink = LegacyReserveFailureSink::default();
        let proof = ReserveBootstrapProof::ScopedCurrent {
            source: &scope,
            failure: &sink,
        };
        let file = tempfile::tempfile().unwrap();
        file.set_len(4096).unwrap();
        let fd = file.as_raw_fd();
        let metadata = file.metadata().unwrap();
        let native = (metadata.dev(), metadata.ino());
        let identity =
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file).unwrap();
        let reserve =
            crate::operation_journal::capacity_reserve_namespace::unqualified_current_bound_fixture(
                request.media_root(),
                &profile,
                file,
            );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if bound {
                let _held = BoundCustody::new(reserve, Some(&proof));
                panic!("component fault after original bound descriptor opened");
            } else {
                let _held = OwnedCustody::new(reserve.into_owned().unwrap(), &proof);
                panic!("component fault after original owned descriptor opened");
            }
        }));
        assert!(result.is_err());
        let held = sink.reserve.borrow();
        let file = held.as_ref().unwrap().file_for_custody_test();
        assert_eq!(file.as_raw_fd(), fd);
        assert_eq!(file.metadata().unwrap().len(), 4096);
        assert_eq!(
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(file).unwrap(),
            identity
        );
        drop(held);
        drop(sink);
        let mut closed = std::mem::MaybeUninit::<libc::stat>::uninit();
        let live = unsafe { libc::fstat(fd, closed.as_mut_ptr()) } == 0;
        if live {
            let closed = unsafe { closed.assume_init() };
            assert_ne!((closed.st_dev as u64, closed.st_ino as u64), native);
        }
        // This tests descriptor custody, never native physical/namespace qualification.
        assert!(
            request
                .media_root()
                .validate_profile_guard(&profile)
                .is_ok()
        );
    }
}
