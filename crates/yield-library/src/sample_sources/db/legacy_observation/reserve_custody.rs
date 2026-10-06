//! Original pre-lease bootstrap custody, separate from admission and current-lease authority.
use super::*;
use crate::durable_capacity::{VolumeIdentity, descriptor_capacity_facts};

pub(crate) struct LegacyReserveTarget {
    pub(crate) volume: VolumeIdentity,
    pub(crate) unit: u64,
    pub(crate) root: Arc<BoundDatabaseRoot>,
}
/// Borrowed only by the existing worker during reserve bootstrap. Cannot escape original custody.
pub(crate) struct LegacyReserveUseProof<'a> {
    source: &'a LegacySourceReadOwner,
    profile: &'a WritableProfileGuard,
    deadline: Instant,
    frozen: LegacySourcePagerObservation,
    #[cfg(test)]
    before_final_targets: Option<fn(Instant)>,
    #[cfg(test)]
    before_final_root: Option<fn(Instant)>,
}
impl LegacyReserveUseProof<'_> {
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn validate_root_for_bootstrap(&self, root: &BoundDatabaseRoot) -> Result<()> {
        if !self
            .validated_targets()?
            .iter()
            .any(|target| std::ptr::eq(target.root.as_ref(), root))
        {
            return Err(LegacyObservationError(
                "legacy bootstrap root is not original custody",
            ));
        }
        root.validate_profile_guard(self.profile).map_err(refused)?;
        #[cfg(test)]
        if let Some(hold) = self.before_final_root {
            hold(self.deadline);
        }
        self.validate_live_boundary()
    }
    pub(crate) fn validate_for_bootstrap(&self) -> Result<()> {
        self.validated_targets().map(|_| ())
    }
    fn validated_targets(&self) -> Result<Vec<LegacyReserveTarget>> {
        self.validate_live_boundary()?;
        if self
            .source
            .capture_pager_until(self.profile, self.deadline)?
            != self.frozen
        {
            return Err(LegacyObservationError(
                "legacy bootstrap original source changed",
            ));
        }
        let targets = self.source.reserve_targets(self.profile, self.deadline)?;
        #[cfg(test)]
        if let Some(hold) = self.before_final_targets {
            hold(self.deadline);
        }
        // Native observation may consume the remaining budget; refuse cooperatively after it.
        self.validate_live_boundary()?;
        Ok(targets)
    }
    fn validate_live_boundary(&self) -> Result<()> {
        if Instant::now() >= self.deadline || !self.source.connection.has_no_effect_fence() {
            return Err(LegacyObservationError("legacy bootstrap custody closed"));
        }
        Ok(())
    }
}
impl LegacySourceReadOwner {
    /// Retain an explicitly supplied reserve directory for a separately mounted original MAIN.
    /// This only binds live descriptors; it creates no reserve, lease or capacity permission.
    /// Existing source/profile/reservation custody must still be live when the worker uses it.
    pub fn bind_legacy_main_reserve_root(&mut self, root: impl AsRef<Path>) -> Result<()> {
        if root.as_ref().as_os_str().as_encoded_bytes().len() > 4096 {
            return Err(LegacyObservationError("legacy reserve root path bound"));
        }
        let profile = self.profile.try_clone().map_err(refused)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let main = self.capture_pager_until(&profile, deadline)?;
        let root = Arc::new(
            BoundDatabaseRoot::for_external_source_root(&profile, root.as_ref())
                .map_err(refused)?,
        );
        if root.legacy_metadata_bytes().is_none() {
            return Err(LegacyObservationError("legacy reserve root metadata bound"));
        }
        let descriptor = root.try_clone_descriptor().map_err(refused)?;
        let facts = descriptor_capacity_facts(&descriptor).map_err(refused)?;
        if facts.identity != *main.volume() || facts.allocation_unit != main.allocation_unit() {
            return Err(LegacyObservationError(
                "legacy MAIN reserve root volume differs",
            ));
        }
        root.validate_profile_guard(&profile).map_err(refused)?;
        self.capture_until(&profile, deadline)?;
        if self.main_reserve_root.is_some() {
            return Err(LegacyObservationError(
                "legacy MAIN reserve root already bound",
            ));
        }
        self.main_reserve_root = Some(root);
        Ok(())
    }
    pub(crate) fn reserve_use_proof<'a>(
        &'a self,
        profile: &'a WritableProfileGuard,
        deadline: Instant,
    ) -> Result<LegacyReserveUseProof<'a>> {
        let proof = LegacyReserveUseProof {
            frozen: self.capture_pager_until(profile, deadline)?,
            source: self,
            profile,
            deadline,
            #[cfg(test)]
            before_final_targets: None,
            #[cfg(test)]
            before_final_root: None,
        };
        proof.validate_for_bootstrap()?;
        Ok(proof)
    }
    /// At most two distinct original source volumes, rooted only in retained capabilities.
    pub(crate) fn reserve_targets(
        &self,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<Vec<LegacyReserveTarget>> {
        let main = self.capture_pager_until(profile, deadline)?;
        let directory = Arc::clone(self.binding.root());
        let descriptor = directory.try_clone_descriptor().map_err(refused)?;
        let facts = descriptor_capacity_facts(&descriptor).map_err(refused)?;
        if facts.allocation_unit == 0 || !facts.allocation_unit.is_power_of_two() {
            return Err(LegacyObservationError(
                "legacy directory allocation unit unknown",
            ));
        }
        let mut targets = vec![LegacyReserveTarget {
            volume: facts.identity.clone(),
            unit: facts.allocation_unit,
            root: directory,
        }];
        if let Some(root) = &self.main_reserve_root {
            root.validate_profile_guard(profile).map_err(refused)?;
            let descriptor = root.try_clone_descriptor().map_err(refused)?;
            let current = descriptor_capacity_facts(&descriptor).map_err(refused)?;
            if &current.identity != main.volume()
                || current.allocation_unit != main.allocation_unit()
            {
                return Err(LegacyObservationError("explicit MAIN reserve root changed"));
            }
        }
        if main.volume() != &facts.identity {
            let root = self
                .main_reserve_root
                .as_ref()
                .ok_or(LegacyObservationError(
                    "separately mounted MAIN requires an explicit reserve root",
                ))?;
            root.validate_profile_guard(profile).map_err(refused)?;
            let descriptor = root.try_clone_descriptor().map_err(refused)?;
            let facts = descriptor_capacity_facts(&descriptor).map_err(refused)?;
            if &facts.identity != main.volume() || facts.allocation_unit != main.allocation_unit() {
                return Err(LegacyObservationError("original MAIN reserve root changed"));
            }
            targets.push(LegacyReserveTarget {
                volume: facts.identity,
                unit: facts.allocation_unit,
                root: Arc::clone(root),
            });
        } else if main.allocation_unit() != facts.allocation_unit {
            return Err(LegacyObservationError(
                "same legacy volume allocation units differ",
            ));
        }
        self.validate(profile)?;
        if targets.len() > 2 || targets.capacity() > 4 {
            return Err(LegacyObservationError(
                "legacy reserve target backing bound",
            ));
        }
        Ok(targets)
    }
}

#[cfg(test)]
mod tests;
