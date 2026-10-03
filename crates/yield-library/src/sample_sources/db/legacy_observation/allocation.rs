//! Proposed source pager policy and finite logical I/O estimates; no capacity/physical grant.
use super::{
    LegacyObservationError, LegacySourceImage, LegacySourceReadOwner, Progress, Result, refused,
};
use crate::app_dirs::WritableProfileGuard;
use crate::durable_capacity::VolumeIdentity;
use std::time::{Duration, Instant};

const OUTPUT_CAP: u64 = 64 * 1024 * 1024;

/// Genuine retained-owner facts and a proposed future policy; never entered policy or admission.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacySourcePagerObservation {
    image: LegacySourceImage,
    page_size: u64,
    page_count: u64,
    existing_max_pages: u64,
    selected_max_pages: u64,
    sector: u64,
    volume: VolumeIdentity,
    allocation_unit: u64,
    peak: Peak,
}
#[derive(Debug, Eq, PartialEq)]
struct Peak {
    database: u64,
    rollback: u64,
    wal: u64,
    shm: u64,
    total: u64,
}
impl LegacySourcePagerObservation {
    /// Exact original read image; a historical observation, not a lease/audit.
    pub fn image(&self) -> &LegacySourceImage {
        &self.image
    }
    /// Actual current SQLite page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }
    /// Actual current logical page count, distinct from physical main-file bytes.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Current connection page cap; no capture raises or enters it.
    pub fn existing_max_pages(&self) -> u64 {
        self.existing_max_pages
    }
    /// Proposed future page cap, at most the existing cap and 64MiB/page_size.
    pub fn selected_max_pages(&self) -> u64 {
        self.selected_max_pages
    }
    /// Sector value from the actual retained known VFS callback.
    pub fn sector_size(&self) -> u64 {
        self.sector
    }
    /// Actual descriptor volume observation; no registration or free-capacity permission.
    pub fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }
    /// Actual filesystem allocation-unit observation; no physical peak qualification.
    pub fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }
    /// Proposed bounded main-file bytes, not physical allocation or a write ceiling entered now.
    pub fn database_bytes(&self) -> u64 {
        self.peak.database
    }
    /// Conservative DELETE rollback estimate under the selected future policy.
    pub fn rollback_bytes(&self) -> u64 {
        self.peak.rollback
    }
    /// Future WAL estimate; transition remains a separately admitted effect.
    pub fn future_wal_bytes(&self) -> u64 {
        self.peak.wal
    }
    /// Future SHM estimate; no sidecar is opened or created by capture.
    pub fn future_shm_bytes(&self) -> u64 {
        self.peak.shm
    }
    /// Checked source-only logical peak. Journal/profile overhead and physical admission absent.
    pub fn source_logical_peak_bytes(&self) -> u64 {
        self.peak.total
    }
}
impl LegacySourceReadOwner {
    /// Capture source-side policy observations through original retained read custody.
    /// Unknown pager/runtime/volume/policy returns refusal; no zero-byte fallback or grant.
    pub fn capture_pager_observation(
        &self,
        profile: &WritableProfileGuard,
    ) -> Result<LegacySourcePagerObservation> {
        self.capture_pager_until(profile, Instant::now() + Duration::from_secs(2))
    }
    fn capture_pager_until(
        &self,
        profile: &WritableProfileGuard,
        deadline: Instant,
    ) -> Result<LegacySourcePagerObservation> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LegacyObservationError("SQLite extension runtime unknown"))?;
        if Instant::now() >= deadline {
            return Err(LegacyObservationError("pager observation deadline elapsed"));
        }
        // capture() revalidates the frozen original image, profile, descriptor and absent sidecars.
        let image = self.capture_until(profile, deadline)?;
        let _progress = Progress::install(&self.connection, deadline);
        let sector = self
            .connection
            .known_delete_sector_size()
            .ok_or(LegacyObservationError("unknown retained pager/runtime"))?;
        let page_size: u64 = self
            .connection
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .map_err(refused)?;
        let page_count: u64 = self
            .connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .map_err(refused)?;
        let existing_max_pages: u64 = self
            .connection
            .pragma_query_value(None, "max_page_count", |r| r.get(0))
            .map_err(refused)?;
        let selected = select_pages(
            page_size,
            existing_max_pages,
            page_count,
            image.main_bytes(),
        )
        .ok_or(LegacyObservationError("unknown or exceeded page policy"))?;
        let peak = peak(page_size, selected)
            .ok_or(LegacyObservationError("source pager estimate overflow"))?;
        let file = self
            .binding
            .root()
            .open_file_read_nofollow(self.binding.name())
            .map_err(refused)?;
        if crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file).as_deref()
            != Some(self.binding.identity())
        {
            return Err(LegacyObservationError("pager volume descriptor replaced"));
        }
        let facts = crate::durable_capacity::descriptor_capacity_facts(&file).map_err(refused)?;
        if facts.allocation_unit == 0 || !facts.allocation_unit.is_power_of_two() {
            return Err(LegacyObservationError("unknown allocation unit"));
        }
        // No free_bytes is carried into the result or accepted as capacity authorization.
        if self.capture_until(profile, deadline)? != image || Instant::now() >= deadline {
            return Err(LegacyObservationError(
                "pager facts changed or deadline elapsed",
            ));
        }
        Ok(LegacySourcePagerObservation {
            image,
            page_size,
            page_count,
            existing_max_pages,
            selected_max_pages: selected,
            sector,
            volume: facts.identity,
            allocation_unit: facts.allocation_unit,
            peak,
        })
    }
}
fn select_pages(size: u64, existing: u64, count: u64, main: u64) -> Option<u64> {
    if !(512..=65536).contains(&size) || !size.is_power_of_two() || existing == 0 || count == 0 {
        return None;
    }
    let selected = existing.min(OUTPUT_CAP / size);
    let capacity = selected.checked_mul(size)?;
    (count <= selected
        && main >= count.checked_mul(size)?
        && main <= capacity
        && main <= 16 * 1024 * 1024)
        .then_some(selected)
}
fn peak(p: u64, m: u64) -> Option<Peak> {
    if p == 0 || m == 0 {
        return None;
    }
    let database = p.checked_mul(m)?;
    let rollback = m
        .checked_add(1)?
        .checked_mul(65536)?
        .checked_add(m.checked_mul(p.checked_add(8)?)?)?;
    let wal = 32u64.checked_add(m.checked_mul(p.checked_add(24)?)?)?;
    let shm = 32768u64.checked_add(16u64.checked_mul(m)?)?;
    let total = database
        .checked_add(rollback)?
        .checked_add(wal)?
        .checked_add(shm)?;
    Some(Peak {
        database,
        rollback,
        wal,
        shm,
        total,
    })
}

#[cfg(test)]
mod tests;
