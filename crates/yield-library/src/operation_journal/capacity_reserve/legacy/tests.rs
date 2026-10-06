use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{DB_FILE_NAME, SourceDatabase},
    test_runtime::TestRuntimeGuard,
};
use std::{
    cell::Cell,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

// Callback evidence only. Real small file writes do not qualify the protected floor.
struct FileEffects {
    lose_after: usize,
    sidecar: PathBuf,
    calls: [Cell<usize>; 3],
}
impl FileEffects {
    fn lose(&self, stage: usize) {
        if self.lose_after == stage {
            std::fs::write(&self.sidecar, b"retained source evidence").unwrap();
        }
    }
}
impl ReserveBackend for FileEffects {
    fn preallocate(&self, file: &File, _bytes: u64) -> Result<u64, ReserveFailureReason> {
        self.calls[0].set(self.calls[0].get() + 1);
        let mut descriptor = file;
        descriptor.write_all(&[0x63; 4096]).unwrap();
        self.lose(1);
        // Logical callback result only; every case must refuse before successful qualification.
        Ok(PROTECTED_FREE_SPACE_FLOOR)
    }
    fn set_len(&self, file: &File, _bytes: u64) -> Result<(), ReserveFailureReason> {
        self.calls[1].set(self.calls[1].get() + 1);
        file.set_len(8192).unwrap();
        self.lose(2);
        Ok(())
    }
    fn sync(&self, file: &File) -> Result<(), ReserveFailureReason> {
        self.calls[2].set(self.calls[2].get() + 1);
        file.sync_all().unwrap();
        self.lose(3);
        Ok(())
    }
    fn final_observation(&self, _file: &File) -> Result<ReserveObservation, ReserveFailureReason> {
        panic!("lost genuine source proof must refuse before qualification observation")
    }
}
impl ReserveBackend for &FileEffects {
    fn preallocate(&self, file: &File, bytes: u64) -> Result<u64, ReserveFailureReason> {
        (**self).preallocate(file, bytes)
    }
    fn set_len(&self, file: &File, bytes: u64) -> Result<(), ReserveFailureReason> {
        (**self).set_len(file, bytes)
    }
    fn sync(&self, file: &File) -> Result<(), ReserveFailureReason> {
        (**self).sync(file)
    }
    fn final_observation(&self, file: &File) -> Result<ReserveObservation, ReserveFailureReason> {
        (**self).final_observation(file)
    }
}

#[test]
fn legacy_reserve_each_lost_proof_boundary_refuses_next_effect_and_retains_exact_file_extent() {
    for lose_after in 0..4 {
        let _test = TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let _root = AppRootGuard::set(temp.path().to_owned()).unwrap();
        let child = temp.path().join("legacy");
        std::fs::create_dir(&child).unwrap();
        let source_path = child.join(DB_FILE_NAME);
        rusqlite::Connection::open(&source_path)
            .unwrap()
            .execute_batch(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/profile-v11/source.sql"
            )))
            .unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let mut source =
            SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
        let original = std::fs::read(&source_path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        source
            .prepare_no_effect_writable_until(&profile, deadline)
            .unwrap();
        let proof = source.reserve_use_proof(&profile, deadline).unwrap();
        let effects = FileEffects {
            lose_after,
            sidecar: child.join(format!("{DB_FILE_NAME}-journal")),
            calls: std::array::from_fn(|_| Cell::new(0)),
        };
        effects.lose(0);
        let path = media.path().join("affected-reserve");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let identity = stable_filesystem_identity_from_open_file(&file).unwrap();
        let failure = reserve_with_backend(
            file,
            LegacyBackend {
                proof: &proof,
                backend: &effects,
            },
        )
        .unwrap_err();
        assert_eq!(failure.reason(), &ReserveFailureReason::LegacyCustodyClosed);
        assert_eq!(
            failure.stage(),
            [
                ReserveFailureStage::Preallocate,
                ReserveFailureStage::SetLength,
                ReserveFailureStage::Sync,
                ReserveFailureStage::Verification
            ][lose_after]
        );
        assert_eq!(
            effects.calls.each_ref().map(|c| c.get()),
            std::array::from_fn(|i| usize::from(i < lose_after))
        );
        let held = failure.into_file();
        assert_eq!(
            stable_filesystem_identity_from_open_file(&held).unwrap(),
            identity
        );
        assert_eq!(
            held.metadata().unwrap().len(),
            [0, 4096, 8192, 8192][lose_after]
        );
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            held.metadata().unwrap().len() as usize
        );
        assert_eq!(std::fs::read(&source_path).unwrap(), original);
        assert_eq!(
            std::fs::read(&effects.sidecar).unwrap(),
            b"retained source evidence"
        );
        drop(held);
        assert!(path.exists()); // No failure Drop cleans up or repeats effects.
    }
}
