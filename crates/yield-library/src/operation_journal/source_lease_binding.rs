//! Explicit historical pre-audit lifecycle/lease scope. Decoding never restores live custody.
use crate::lifecycle_allocation::{LifecycleAllocationRole, LifecycleDatabaseAllocation};
use crate::sample_sources::db::{SourceRegistryReferenceSubject, SourceRegistryRegistrationPhase};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Whole canonical stored lease scope ceiling, independent of the operation record cap.
pub const MAX_SOURCE_LEASE_SCOPE_BYTES: usize = 16 * 1024;
/// Invalid or out-of-policy historical data; never a permission or cleanup result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid bounded source lifecycle/lease scope")]
pub struct SourceLeaseBindingError;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    identity: String,
    device: u64,
    unit: u64,
    bytes: u64,
    ceiling: u64,
}
/// Canonical finite scope for lease maintenance before or after an audit.
/// Registration phase, scan revision, audit and native facts are retained observations only.
/// This is distinct from StoredSourceFence and has no conversion to SourceRevision or a permit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSourceLeaseFence {
    version: u8,
    subject: SourceRegistryReferenceSubject,
    preparation_nonce: Uuid,
    page_size: u64,
    maximum_pages: u64,
    objects: [Object; 3],
}
impl StoredSourceLeaseFence {
    pub(crate) fn retained_heap_bytes(&self) -> Option<usize> {
        self.objects
            .iter()
            .try_fold(self.subject.retained_heap_bytes()?, |n, o| {
                n.checked_add(o.identity.capacity())
            })
    }
    pub(crate) fn from_capture(
        subject: SourceRegistryReferenceSubject,
        allocation: &LifecycleDatabaseAllocation,
        nonce: Uuid,
    ) -> Result<Self, SourceLeaseBindingError> {
        if allocation.role() != LifecycleAllocationRole::SourceLeaseMaintenance {
            return Err(SourceLeaseBindingError);
        }
        let objects = [allocation.main(), allocation.wal(), allocation.shm()].map(|o| Object {
            identity: o.identity().into(),
            device: o.device(),
            unit: o.allocation_unit(),
            bytes: o.observed_bytes(),
            ceiling: o.write_ceiling(),
        });
        let value = Self {
            version: 1,
            subject,
            preparation_nonce: nonce,
            page_size: allocation.page_size(),
            maximum_pages: allocation.selected_maximum_pages(),
            objects,
        };
        value.validate()?;
        Ok(value)
    }
    /// Validate retained shape and bounds only, without acquiring any owner or registration.
    pub fn validate(&self) -> Result<(), SourceLeaseBindingError> {
        use SourceRegistryRegistrationPhase::*;
        crate::sample_sources::db::validate_registry_reference_subject(&self.subject)
            .map_err(|_| SourceLeaseBindingError)?;
        let text =
            |s: &str| !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control);
        let subject = &self.subject;
        if self.version != 1
            || self.preparation_nonce.is_nil()
            || subject
                .registration()
                .is_none_or(|r| !matches!(r.phase(), Prepared | RegisteredPhaseObserved | Draining))
            || subject.media_root_identity().len() > 176
            || !text(subject.database_root_identity())
            || !text(subject.database_identity())
            || !text(subject.root_binding_identity())
            || !(512..=65536).contains(&self.page_size)
            || !self.page_size.is_power_of_two()
            || self.maximum_pages == 0
            || self
                .page_size
                .checked_mul(self.maximum_pages)
                .is_none_or(|v| v > 64 * 1024 * 1024)
            || subject.database_identity() != self.objects[0].identity
        {
            return Err(SourceLeaseBindingError);
        }
        let windows = [
            self.page_size.checked_mul(self.maximum_pages),
            self.page_size
                .checked_add(24)
                .and_then(|p| p.checked_mul(self.maximum_pages))
                .and_then(|v| v.checked_add(32)),
            self.maximum_pages
                .checked_mul(16)
                .and_then(|v| v.checked_add(32768)),
        ];
        for (i, o) in self.objects.iter().enumerate() {
            if !text(&o.identity)
                || o.unit == 0
                || !o.unit.is_power_of_two()
                || o.unit > 1024 * 1024
                || o.bytes > i64::MAX as u64
                || o.ceiling == 0
                || o.ceiling > i64::MAX as u64
                || o.ceiling < o.bytes
                || o.ceiling % o.unit != 0
                || windows[i]
                    .and_then(|w| crate::lifecycle_allocation::logical_ceiling(o.bytes, w, o.unit))
                    != Some(o.ceiling)
                || o.device != self.objects[0].device
                || o.unit != self.objects[0].unit
                || self.objects[..i].iter().any(|p| p.identity == o.identity)
            {
                return Err(SourceLeaseBindingError);
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| SourceLeaseBindingError)?
            .len()
            > MAX_SOURCE_LEASE_SCOPE_BYTES
        {
            return Err(SourceLeaseBindingError);
        }
        Ok(())
    }
    /// Exact historical source/lifecycle/lease/profile tuple; no audit completeness is required.
    pub fn subject(&self) -> &SourceRegistryReferenceSubject {
        &self.subject
    }
    // The original passive allocation factory captures no registration. A stored
    // phase/nonce is history; compare every original owner/audit scalar without
    // constructing a registered current subject or treating phase as an ACK.
    pub(crate) fn matches_allocation_subject(
        &self,
        current: &SourceRegistryReferenceSubject,
    ) -> bool {
        let s = &self.subject;
        current.registration().is_none()
            && s.source_id() == current.source_id()
            && s.lifecycle_generation() == current.lifecycle_generation()
            && s.profile_acquisition() == current.profile_acquisition()
            && s.lease_owner() == current.lease_owner()
            && s.lease_epoch() == current.lease_epoch()
            && s.media_root_identity() == current.media_root_identity()
            && s.database_root_identity() == current.database_root_identity()
            && s.database_identity() == current.database_identity()
            && s.root_binding_identity() == current.root_binding_identity()
            && s.scan_revision() == current.scan_revision()
            && s.audit() == current.audit()
    }
    pub(crate) fn database_device(&self) -> u64 {
        self.objects[0].device
    }
    /// Original local preparation identity, not the journal decision nonce or live custody.
    pub fn preparation_nonce(&self) -> Uuid {
        self.preparation_nonce
    }
    /// Canonical bounded bytes, never a source reacquisition instruction.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, SourceLeaseBindingError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| SourceLeaseBindingError)
    }
    /// Strict canonical bounded decode. It yields observations only.
    pub fn decode(bytes: &[u8]) -> Result<Self, SourceLeaseBindingError> {
        if bytes.len() > MAX_SOURCE_LEASE_SCOPE_BYTES {
            return Err(SourceLeaseBindingError);
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| SourceLeaseBindingError)?;
        if value.canonical_bytes()? != bytes {
            return Err(SourceLeaseBindingError);
        }
        Ok(value)
    }
    pub(crate) fn claim_media_root(&self) -> String {
        // Explicit lossless bytes in the new domain; never reinterpret an authoritative root.
        format!(
            "lease-root-v1:{}",
            URL_SAFE_NO_PAD.encode(self.subject.media_root_identity())
        )
    }
    pub(crate) fn write_ceilings(&self) -> [u64; 3] {
        self.objects.each_ref().map(|o| o.ceiling)
    }
    pub(crate) fn device(&self) -> u64 {
        self.objects[0].device
    }
    pub(crate) fn allocation_unit(&self) -> u64 {
        self.objects[0].unit
    }
    pub(crate) fn lifecycle_observed_registered(&self) -> bool {
        self.subject.registration().is_some_and(|r| {
            matches!(
                r.phase(),
                SourceRegistryRegistrationPhase::RegisteredPhaseObserved
                    | SourceRegistryRegistrationPhase::Draining,
            )
        })
    }
}
