//! Keep an original opened reserve in the preinstalled recovery sink during unwinding.
//! These stack guards allocate no backing and confer no authority or clean handoff.
use super::{BoundReserve, OwnedReserveAllocation, ReserveBootstrapProof};
use std::ops::{Deref, DerefMut};

pub(super) struct OwnedCustody<'a, 'p, 's> {
    reserve: Option<OwnedReserveAllocation>,
    proof: &'a ReserveBootstrapProof<'p, 's>,
}
impl<'a, 'p, 's> OwnedCustody<'a, 'p, 's> {
    pub(super) fn new(
        reserve: OwnedReserveAllocation,
        proof: &'a ReserveBootstrapProof<'p, 's>,
    ) -> Self {
        Self {
            reserve: Some(reserve),
            proof,
        }
    }
    pub(super) fn take(mut self) -> OwnedReserveAllocation {
        self.reserve
            .take()
            .expect("original reserve transfers once")
    }
}
impl Deref for OwnedCustody<'_, '_, '_> {
    type Target = OwnedReserveAllocation;
    fn deref(&self) -> &Self::Target {
        self.reserve
            .as_ref()
            .expect("original reserve remains held")
    }
}
impl DerefMut for OwnedCustody<'_, '_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.reserve
            .as_mut()
            .expect("original reserve remains held")
    }
}
impl Drop for OwnedCustody<'_, '_, '_> {
    fn drop(&mut self) {
        if let Some(reserve) = self.reserve.take() {
            // Original Current/Legacy sinks are installed before opening the child. Other
            // recovery consumers keep their existing descriptor drop behavior.
            self.proof.retain_reopened(reserve);
        }
    }
}

pub(super) struct BoundCustody<'a, 'r, 'p, 's> {
    reserve: Option<BoundReserve<'r>>,
    proof: Option<&'a ReserveBootstrapProof<'p, 's>>,
}
impl<'a, 'r, 'p, 's> BoundCustody<'a, 'r, 'p, 's> {
    pub(super) fn new(
        reserve: BoundReserve<'r>,
        proof: Option<&'a ReserveBootstrapProof<'p, 's>>,
    ) -> Self {
        Self {
            reserve: Some(reserve),
            proof,
        }
    }
    pub(super) fn take(mut self) -> BoundReserve<'r> {
        self.reserve
            .take()
            .expect("original bound reserve transfers once")
    }
}
impl<'r> Deref for BoundCustody<'_, 'r, '_, '_> {
    type Target = BoundReserve<'r>;
    fn deref(&self) -> &Self::Target {
        self.reserve
            .as_ref()
            .expect("original bound reserve remains held")
    }
}
impl Drop for BoundCustody<'_, '_, '_, '_> {
    fn drop(&mut self) {
        if let (Some(proof), Some(reserve)) = (self.proof, self.reserve.take()) {
            // A failed original root duplication itself returns the exact protected file in
            // the namespace failure capsule; retain_bound preserves that alternate backing.
            proof.retain_bound(reserve);
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::operation_journal) enum CurrentReserveBoundary {
    Created,
    CreatedBound,
    RecoveryReopened,
    CleanRebindOpened,
    CleanRebound,
}

#[cfg(test)]
pub(super) type CurrentReserveHook = Box<dyn FnMut(CurrentReserveBoundary, &std::fs::File) + Send>;

#[cfg(all(test, unix))]
mod tests {
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
                crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file)
                    .unwrap();
            let reserve = crate::operation_journal::capacity_reserve_namespace::unqualified_current_bound_fixture(
                request.media_root(), &profile, file,
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
                crate::filesystem_identity::stable_filesystem_identity_from_open_file(file)
                    .unwrap(),
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
}
