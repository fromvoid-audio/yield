//! Genuine original descriptor observations under counted runtime custody; no effect permission.
use super::super::capacity_reserve_namespace::ReadonlyReserveNamespace;
use super::super::capacity_volume_owner::{ReadonlyVolumeOwner, VolumeOwnershipError};
use super::resource_custody::{ResourceAdmission, ResourceUse, UseError};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
const MAX_READERS: usize = 32;
pub(super) const BACKING: usize = 64 * 1024;
pub(super) const REGISTRY: usize = 32 * 1024;
const SCRATCH: usize = 32 * 1024;
const MAX_FACTS: usize = 32;
#[derive(Default)]
struct FactBudget(AtomicUsize);
impl FactBudget {
    fn reserve(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(1).filter(|next| *next <= MAX_FACTS)
            })
            .is_ok()
    }
    fn release(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_sub(1).filter(|next| *next < MAX_FACTS)
            })
            .is_ok()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeReadError {
    Closed,
    Missing,
    Unsupported,
    Unknown,
    Full,
    Conflict,
}
#[derive(Eq, PartialEq)]
struct Key {
    volume: VolumeIdentity,
    owner_generation: u64,
    profile: String,
    root: String,
    file: String,
    unit: u64,
}
impl Key {
    fn backing(&self) -> usize {
        self.profile.capacity() + self.root.capacity() + self.file.capacity()
    }
}
#[allow(dead_code)] // Full original issuance/slot composition is OPT-2215.
pub(super) struct NativeReadCapsule {
    data: Arc<NativeCapsuleData>,
}
pub(super) struct NativeCapsuleData {
    key: Key,
    owner: ReadonlyVolumeOwner,
    reserve: ReadonlyReserveNamespace,
    admission: Arc<ResourceAdmission>,
    sampling: AtomicBool,
    fact_headers: FactBudget,
    _input: InputPermit,
}
/// Non-Clone, non-serializable actual native sample. IDs/facts alone cannot construct it.
#[allow(dead_code)]
pub(super) struct NativeReadFacts {
    facts: VolumeFacts,
    original: Arc<NativeCapsuleData>,
    use_generation: u64,
}
const _: () = assert!(MAX_FACTS * std::mem::size_of::<NativeReadFacts>() + 24 * 1024 <= SCRATCH);
const _: () = assert!(
    MAX_READERS
        * (std::mem::size_of::<NativeCapsuleData>()
            + std::mem::size_of::<VolumeIdentity>()
            + std::mem::size_of::<std::sync::Weak<NativeCapsuleData>>()
            + 8 * std::mem::size_of::<usize>())
        <= REGISTRY
);
fn custody_error(error: UseError) -> NativeReadError {
    match error {
        UseError::Closed => NativeReadError::Closed,
        UseError::Full => NativeReadError::Full,
        UseError::Unknown => NativeReadError::Unknown,
    }
}
#[allow(dead_code)]
impl NativeReadCapsule {
    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn same_original(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }

    pub(super) fn read(
        &self,
        resource: &mut ResourceUse,
    ) -> Result<NativeReadFacts, NativeReadError> {
        // One native sample per capsule at a time bounds transient native/path scratch.
        if self
            .data
            .sampling
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(NativeReadError::Full);
        }
        struct SampleGuard<'a>(&'a AtomicBool);
        impl Drop for SampleGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _sample = SampleGuard(&self.data.sampling);
        // Facts may outlive their counted read. Bound those retained headers separately.
        if !self.data.fact_headers.reserve() {
            return Err(NativeReadError::Full);
        }
        struct HeaderGuard<'a> {
            data: &'a NativeCapsuleData,
            active: bool,
        }
        impl Drop for HeaderGuard<'_> {
            fn drop(&mut self) {
                if self.active && !self.data.fact_headers.release() {
                    self.data.admission.invalidate();
                }
            }
        }
        let mut header = HeaderGuard {
            data: &self.data,
            active: true,
        };
        let generation = resource
            .begin_observation(&self.data.admission)
            .map_err(custody_error)?;
        // No admission/slot mutex, source validation, queue wait or actor borrow crosses IO.
        let observed = (|| {
            self.data
                .owner
                .validate()
                .map_err(|_| NativeReadError::Unknown)?;
            let facts = self
                .data
                .reserve
                .full_facts()
                .map_err(|_| NativeReadError::Unknown)?;
            if facts.identity != self.data.key.volume || facts.allocation_unit != self.data.key.unit
            {
                return Err(NativeReadError::Unknown);
            }
            self.data
                .owner
                .validate()
                .map_err(|_| NativeReadError::Unknown)?;
            resource
                .verify_read_for(&self.data.admission)
                .map_err(custody_error)?;
            Ok(facts)
        })();
        match observed {
            Ok(facts) => {
                header.active = false;
                Ok(NativeReadFacts {
                    facts,
                    original: Arc::clone(&self.data),
                    use_generation: generation,
                })
            }
            Err(error) => {
                if error == NativeReadError::Unknown {
                    self.data.admission.invalidate();
                }
                Err(error)
            }
        }
    }
}
impl Drop for NativeReadFacts {
    fn drop(&mut self) {
        if !self.original.fact_headers.release() {
            self.original.admission.invalidate();
        }
    }
}
#[allow(dead_code)]
impl NativeReadFacts {
    pub(super) fn facts(&self) -> &VolumeFacts {
        &self.facts
    }
    pub(super) fn matches(&self, capsule: &NativeReadCapsule, resource: &ResourceUse) -> bool {
        Arc::ptr_eq(&self.original, &capsule.data)
            && resource.verify_read_for(&self.original.admission).is_ok()
            && resource.generation() == self.use_generation
    }
}
impl OwnerRuntime {
    pub(super) fn attach_resource_custody(
        &mut self,
        shared: &Arc<OwnerShared>,
    ) -> Result<(), String> {
        if self.resource_input.is_some() {
            return Err("resource custody already attached".into());
        }
        let input = shared
            .reserve_publication_input(resource_custody::HEADER_BYTES)
            .map_err(|error| error.to_string())?;
        self.resource_admission = Arc::clone(&shared.resource_admission);
        self.resource_input = Some(input);
        Ok(())
    }

    #[allow(dead_code)] // Private genuine issuer consumes this factory in full OPT-2215.
    pub(super) fn capture_native_read_capsule(
        &mut self,
        volume: &VolumeIdentity,
    ) -> Result<NativeReadCapsule, NativeReadError> {
        let shared = self
            .resource_input
            .as_ref()
            .ok_or(NativeReadError::Closed)?
            .shared
            .clone();
        self.resource_admission
            .validate_open()
            .map_err(custody_error)?;
        if OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
            != OwnerLifecycle::Available
        {
            return Err(NativeReadError::Closed);
        }
        self.native_readers
            .retain(|_, reader| reader.strong_count() != 0);
        if !self.native_readers.contains_key(volume) && self.native_readers.len() >= MAX_READERS {
            return Err(NativeReadError::Full);
        }
        let input = shared
            .reserve_publication_input(BACKING)
            .map_err(|_| NativeReadError::Full)?;
        let profile = self
            .final_profile_guard
            .as_ref()
            .ok_or(NativeReadError::Closed)?;
        profile
            .validate_current()
            .map_err(|_| NativeReadError::Unknown)?;
        let (owner, reserve) = if self
            .volume_owner
            .as_ref()
            .is_some_and(|owner| owner.volume() == volume)
        {
            (
                self.volume_owner.as_ref().ok_or(NativeReadError::Missing)?,
                self.reserve.as_mut().ok_or(NativeReadError::Missing)?,
            )
        } else {
            let binding = self
                .source_volumes
                .get_mut(volume)
                .ok_or(NativeReadError::Missing)?;
            if !matches!(binding.readiness, SourceVolumeBindingReadiness::Complete) {
                return Err(NativeReadError::Missing);
            }
            (
                binding.owner.as_ref().ok_or(NativeReadError::Missing)?,
                binding.reserve.as_mut().ok_or(NativeReadError::Missing)?,
            )
        };
        if owner.owner_profile_identity() != profile.profile_root_identity()
            || owner.volume() != reserve.observation().volume()
        {
            return Err(NativeReadError::Conflict);
        }
        let observation = reserve.observation();
        if observation.root_identity().len() > 256
            || observation.file_identity().len() > 256
            || profile.profile_root_identity().len() > 256
        {
            return Err(NativeReadError::Full);
        }
        let key = Key {
            volume: volume.clone(),
            owner_generation: owner.owner_generation(),
            profile: profile.profile_root_identity().to_owned(),
            root: observation.root_identity().to_owned(),
            file: observation.file_identity().to_owned(),
            unit: observation.allocation_unit(),
        };
        let readonly_owner = owner.readonly_observation().map_err(|error| match error {
            VolumeOwnershipError::Unsupported { .. } => NativeReadError::Unsupported,
            _ => NativeReadError::Unknown,
        })?;
        let readonly_reserve = reserve
            .readonly_observation(profile)
            .map_err(|_| NativeReadError::Unknown)?;
        owner.revalidate().map_err(|_| NativeReadError::Unknown)?;
        self.resource_admission
            .validate_open()
            .map_err(custody_error)?;
        let bytes = 2 * std::mem::size_of::<usize>()
            + std::mem::size_of::<NativeCapsuleData>()
            + key.backing()
            + readonly_owner.retained_bytes()
            + readonly_reserve.retained_bytes()
            + SCRATCH;
        if bytes > BACKING {
            return Err(NativeReadError::Full);
        }
        if let Some(original) = self
            .native_readers
            .get(volume)
            .and_then(std::sync::Weak::upgrade)
        {
            if original.key != key || !original.owner.same_original(&readonly_owner) {
                return Err(NativeReadError::Conflict);
            }
            return Ok(NativeReadCapsule { data: original });
        }
        if self.native_reader_registry_input.is_none() {
            self.native_reader_registry_input = Some(
                shared
                    .reserve_publication_input(REGISTRY)
                    .map_err(|_| NativeReadError::Full)?,
            );
        }
        let data = Arc::new(NativeCapsuleData {
            key,
            owner: readonly_owner,
            reserve: readonly_reserve,
            admission: Arc::clone(&self.resource_admission),
            _input: input,
            sampling: AtomicBool::new(false),
            fact_headers: FactBudget::default(),
        });
        self.native_readers
            .insert(volume.clone(), Arc::downgrade(&data));
        Ok(NativeReadCapsule { data })
    }
}
#[cfg(test)]
mod tests;
