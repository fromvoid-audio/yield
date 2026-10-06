//! Sealed pre-lease bootstrap; failure backing cannot become an ownership capability.
use super::*;
use crate::app_dirs::{LegacyAppRootBacking, open_legacy_canonical_app_root};
use crate::sample_sources::db::LegacyReserveUseProof;

#[derive(Default)]
pub(crate) struct LegacyVolumeFailureBacking {
    // All affected lock/file backing closes before ancestor namespaces.
    failed_owner: Option<VolumeOwnershipCapability>,
    lock: Option<File>,
    registry: Option<RegistryRoot>,
    app: Option<Dir>,
    app_failure: Option<LegacyAppRootBacking>,
    marker: Option<OwnershipMarker>,
    #[cfg(target_os = "macos")]
    binding: Option<MacVolumeBinding>,
}
impl LegacyVolumeFailureBacking {
    pub(crate) fn metadata_bytes(&self) -> usize {
        // macOS binding is only File/u64/[u8;16], entirely in this header, no dynamic backing.
        #[cfg(target_os = "macos")]
        const _: () = assert!(std::mem::size_of::<MacVolumeBinding>() <= 64);
        let _backing = (&self.lock, &self.app);
        std::mem::size_of::<Self>()
            + self
                .failed_owner
                .as_ref()
                .and_then(VolumeOwnershipCapability::legacy_metadata_bytes)
                .unwrap_or(0)
            + self
                .registry
                .as_ref()
                .map_or(0, |r| r.path.capacity() + r.identity.capacity())
            + self
                .app_failure
                .as_ref()
                .map_or(0, LegacyAppRootBacking::metadata_bytes)
            + self
                .marker
                .as_ref()
                .map_or(0, |m| m.owner_profile_identity.capacity())
    }
}

pub(crate) fn acquire_legacy_for_descriptor(
    descriptor: &File,
    profile: &WritableProfileGuard,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<VolumeOwnershipCapability, LegacyVolumeFailureBacking> {
    acquire_legacy_inner(
        descriptor,
        profile,
        proof,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
}
#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum TestFault {
    AfterLock,
    AfterMarker,
    FinalValidation,
}
fn acquire_legacy_inner(
    descriptor: &File,
    profile: &WritableProfileGuard,
    proof: &LegacyReserveUseProof<'_>,
    #[cfg(test)] fixture_app: Option<(PathBuf, Dir)>,
    #[cfg(test)] fault: Option<TestFault>,
) -> Result<VolumeOwnershipCapability, LegacyVolumeFailureBacking> {
    let mut backing = LegacyVolumeFailureBacking::default();
    let result = (|| {
        proof.validate_for_bootstrap().map_err(|_| ())?;
        profile.validate_current().map_err(|_| ())?;
        validate_owner_profile_identity(profile.profile_root_identity()).map_err(|_| ())?;
        let facts = descriptor_capacity_facts(descriptor).map_err(|_| ())?;
        #[cfg(target_os = "macos")]
        {
            backing.binding =
                Some(MacVolumeBinding::acquire(descriptor, &facts.identity).map_err(|_| ())?);
        }
        #[cfg(test)]
        let opened = fixture_app.map_or_else(|| open_legacy_canonical_app_root(proof), Ok);
        #[cfg(not(test))]
        let opened = open_legacy_canonical_app_root(proof);
        let (app_path, app) = match opened {
            Ok(app) => app,
            Err(failure) => {
                backing.app_failure = Some(failure);
                return Err(());
            }
        };
        backing.app = Some(app);
        let registry_path = app_path.join(REGISTRY_DIRECTORY_NAME);
        let bounded =
            |p: &PathBuf| p.as_os_str().as_encoded_bytes().len() <= 4096 && p.capacity() <= 8192;
        if !bounded(&registry_path) {
            return Err(());
        }
        let app = backing.app.as_ref().unwrap();
        let registry = match app.open_dir_nofollow(REGISTRY_DIRECTORY_NAME) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                proof.validate_for_bootstrap().map_err(|_| ())?;
                app.create_dir(REGISTRY_DIRECTORY_NAME)
                    .or_else(|e| {
                        if e.kind() == io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    })
                    .map_err(|_| ())?;
                app.open_dir_nofollow(REGISTRY_DIRECTORY_NAME)
                    .map_err(|_| ())?
            }
            Err(_) => return Err(()),
        };
        // Store the exact opened namespace before any subsequent refusal or sync.
        backing.registry = Some(RegistryRoot {
            path: registry_path,
            identity: String::new(),
            capability: registry,
        });
        let registry = backing.registry.as_mut().unwrap();
        registry.identity = identity_from_dir(&registry.capability).ok_or(())?;
        if registry.identity.capacity() > MAX_IDENTITY_BYTES {
            return Err(());
        }
        proof.validate_for_bootstrap().map_err(|_| ())?;
        sync_directory(app, &app_path).map_err(|_| ())?;
        registry.validate().map_err(|_| ())?;
        let name = lock_file_name(&facts.identity).map_err(|_| ())?;
        let lock_path = registry.path.join(&name);
        if !bounded(&lock_path) {
            return Err(());
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);
        proof.validate_for_bootstrap().map_err(|_| ())?;
        backing.lock = Some(
            registry
                .capability
                .open_with(Path::new(&name), &options)
                .map_err(|_| ())?
                .into_std(),
        );
        #[cfg(test)]
        if fault == Some(TestFault::AfterLock) {
            return Err(());
        }
        let lock = backing.lock.as_ref().unwrap();
        validate_lock_file(lock, &lock_path).map_err(|_| ())?;
        proof.validate_for_bootstrap().map_err(|_| ())?;
        sync_directory(&registry.capability, &registry.path).map_err(|_| ())?;
        let lock_identity = identity_from_file(lock).ok_or(())?;
        if lock_identity.capacity() > MAX_IDENTITY_BYTES {
            return Err(());
        }
        try_exclusive_lock(lock, &lock_path).map_err(|_| ())?;
        #[cfg(target_os = "macos")]
        backing
            .binding
            .as_ref()
            .unwrap()
            .validate(&facts.identity)
            .map_err(|_| ())?;
        let previous =
            read_marker(lock.try_clone().map_err(|_| ())?, &lock_path).map_err(|_| ())?;
        #[cfg(target_os = "macos")]
        if previous.as_ref().is_some_and(|m| {
            m.state == MarkerState::Dirty
                && m.owner_profile_identity != profile.profile_root_identity()
        }) {
            return Err(());
        }
        let (marker, persist) =
            next_dirty_marker(previous, profile.profile_root_identity(), true).map_err(|_| ())?;
        backing.marker = Some(marker);
        if persist {
            let mut writer = lock.try_clone().map_err(|_| ())?;
            write_sealed_marker(&mut writer, backing.marker.as_ref().unwrap(), proof)?;
        }
        #[cfg(test)]
        if fault == Some(TestFault::AfterMarker) {
            return Err(());
        }
        let capability = VolumeOwnershipCapability {
            registry: backing.registry.take().unwrap(),
            volume: facts.identity,
            lock_path,
            lock_identity,
            lock_file: backing.lock.take().unwrap(),
            marker: backing.marker.take().unwrap(),
            #[cfg(target_os = "macos")]
            mac_volume: backing.binding.take(),
            handoff_fenced: Rc::new(Cell::new(false)),
            read_fenced: Arc::new(AtomicBool::new(false)),
            _owner_thread: PhantomData,
        };
        #[cfg(test)]
        let injected = fault == Some(TestFault::FinalValidation);
        #[cfg(not(test))]
        let injected = false;
        if injected
            || proof.validate_for_bootstrap().is_err()
            || capability.revalidate().is_err()
            || capability.legacy_metadata_bytes().is_none()
        {
            capability.handoff_fenced.set(true);
            backing.failed_owner = Some(capability);
            return Err(());
        }
        Ok(capability)
    })();
    result.map_err(|()| backing)
}

fn write_sealed_marker(
    file: &mut File,
    marker: &OwnershipMarker,
    proof: &LegacyReserveUseProof<'_>,
) -> Result<(), ()> {
    let contents = format!(
        "version={MARKER_VERSION}\nstate=dirty\nowner_profile_identity={}\ngeneration={}\n",
        marker.owner_profile_identity, marker.generation
    );
    if contents.len() > MAX_MARKER_BYTES {
        return Err(());
    }
    proof.validate_for_bootstrap().map_err(|_| ())?;
    file.set_len(0).map_err(|_| ())?;
    file.seek(SeekFrom::Start(0)).map_err(|_| ())?;
    let mut pending = contents.as_bytes();
    while !pending.is_empty() {
        proof.validate_for_bootstrap().map_err(|_| ())?;
        let n = file.write(pending).map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        pending = &pending[n..];
    }
    proof.validate_for_bootstrap().map_err(|_| ())?;
    file.sync_all().map_err(|_| ())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    mod native {
        use super::super::*;
        use crate::{
            app_dirs::AppRootGuard,
            sample_sources::{DB_FILE_NAME, SourceDatabase},
            test_runtime::TestRuntimeGuard,
        };
        use std::time::{Duration, Instant};

        #[test]
        fn original_sealed_bootstrap_retains_exact_lock_and_registry_at_each_refused_prefix() {
            let _test = TestRuntimeGuard::acquire();
            let profile_dir = tempfile::tempdir().unwrap();
            let media = tempfile::tempdir().unwrap();
            let _root = AppRootGuard::set(profile_dir.path().to_owned()).unwrap();
            let profile = WritableProfileGuard::acquire_current().unwrap();
            let source_path = profile_dir.path().join("legacy");
            std::fs::create_dir(&source_path).unwrap();
            rusqlite::Connection::open(source_path.join(DB_FILE_NAME))
                .unwrap()
                .execute_batch(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/profile-v11/source.sql"
                )))
                .unwrap();
            let mut source =
                SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            source
                .prepare_no_effect_writable_until(&profile, deadline)
                .unwrap();
            let proof = source.reserve_use_proof(&profile, deadline).unwrap();
            let descriptor = File::open(&source_path).unwrap();
            let volume = descriptor_capacity_facts(&descriptor).unwrap().identity;
            let name = lock_file_name(&volume).unwrap();
            for fault in [
                TestFault::AfterLock,
                TestFault::AfterMarker,
                TestFault::FinalValidation,
            ] {
                let app = tempfile::tempdir().unwrap();
                let dir = open_no_follow_directory(app.path()).unwrap();
                let backing = acquire_legacy_inner(
                    &descriptor,
                    &profile,
                    &proof,
                    Some((app.path().to_owned(), dir)),
                    Some(fault),
                )
                .err()
                .unwrap();
                let path = app.path().join(REGISTRY_DIRECTORY_NAME).join(&name);
                let expected = std::fs::read(&path).unwrap();
                let held = backing
                    .lock
                    .as_ref()
                    .or_else(|| backing.failed_owner.as_ref().map(|o| &o.lock_file))
                    .unwrap();
                assert_eq!(
                    identity_from_file(held),
                    identity_from_file(&File::open(&path).unwrap())
                );
                assert_eq!(held.metadata().unwrap().len(), expected.len() as u64);
                assert!(
                    backing.metadata_bytes() > std::mem::size_of::<LegacyVolumeFailureBacking>()
                );
                if fault != TestFault::AfterLock {
                    assert!(String::from_utf8_lossy(&expected).contains("state=dirty"));
                    let contender = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap();
                    assert!(contender.try_lock().is_err());
                    assert_eq!(std::fs::read(&path).unwrap(), expected);
                    if let Some(owner) = &backing.failed_owner {
                        assert!(owner.revalidate().is_err());
                    }
                    drop(backing);
                    contender.try_lock().unwrap();
                } else {
                    assert!(expected.is_empty());
                    drop(backing);
                }
                assert_eq!(std::fs::read(&path).unwrap(), expected);
                proof.validate_for_bootstrap().unwrap();
            }
        }
    }
}
