//! Actual bounded SQLite/VFS observations for lifecycle planning, never physical qualification.
use crate::{
    sample_sources::db::BoundDatabaseRoot,
    sqlite_budget::{
        LogicalBudget, OwnedConnection,
        wal_objects::{WalObjectFacts, WalObjects},
    },
};
use rusqlite::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Finite database role whose compiled schema and owner must be validated independently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleAllocationRole {
    /// Global immutable lifecycle reference/receipt transitions.
    GlobalReferences,
    /// Exact source-main lease release; no audit or ordinary source mutation is granted.
    SourceLeaseMaintenance,
}
/// Failure to define exact finite storage observations before an effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LifecycleAllocationError {
    /// Schema, runtime, root/native objects or finite pager configuration is unsupported.
    #[error("lifecycle allocation is unknown; explicit qualification or schema audit required")]
    Unknown,
    /// Original cooperative deadline or finite VM work allowance exhausted.
    #[error("lifecycle allocation observation budget exhausted")]
    Budget,
}
/// One actual object and a future restrictive logical I/O ceiling, without a disk reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleObjectAllocation {
    facts: WalObjectFacts,
    ceiling: u64,
}
impl LifecycleObjectAllocation {
    /// Actual retained native/mapped object identity, not a filename or grant.
    pub fn identity(&self) -> &str {
        &self.facts.identity
    }
    /// Descriptor-observed device identifier.
    pub fn device(&self) -> u64 {
        self.facts.device
    }
    /// Descriptor-observed filesystem fragment unit, not physical overhead qualification.
    pub fn allocation_unit(&self) -> u64 {
        self.facts.allocation_unit
    }
    /// Actual object length, distinct from logical SQLite pages.
    pub fn observed_bytes(&self) -> u64 {
        self.facts.bytes
    }
    /// Checked unit-rounded logical forwarding-VFS restriction for this object.
    pub fn write_ceiling(&self) -> u64 {
        self.ceiling
    }
}
/// Copyable storage planning observations. No decoder, constructor or permit conversion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleDatabaseAllocation {
    role: LifecycleAllocationRole,
    page_size: u64,
    page_count: u64,
    original_max_pages: u64,
    selected_max_pages: u64,
    main: LifecycleObjectAllocation,
    wal: LifecycleObjectAllocation,
    shm: LifecycleObjectAllocation,
}
impl LifecycleDatabaseAllocation {
    /// Family scope, requiring the original actual owner to revalidate before admission.
    pub fn role(&self) -> LifecycleAllocationRole {
        self.role
    }
    /// Actual SQLite page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }
    /// Actual current logical page count.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Existing pager maximum observed before any restriction is installed.
    pub fn original_maximum_pages(&self) -> u64 {
        self.original_max_pages
    }
    /// Maximum that the later entered policy must install and revalidate.
    pub fn selected_maximum_pages(&self) -> u64 {
        self.selected_max_pages
    }
    /// Actual MAIN object and its separately accounted ceiling.
    pub fn main(&self) -> &LifecycleObjectAllocation {
        &self.main
    }
    /// Actual WAL object and its separately accounted ceiling.
    pub fn wal(&self) -> &LifecycleObjectAllocation {
        &self.wal
    }
    /// Actual kernel-mapped SHM object and its separately accounted ceiling.
    pub fn shm(&self) -> &LifecycleObjectAllocation {
        &self.shm
    }

    // Exact passive facts after installing only the selected pager restriction.
    pub(crate) fn matches_entered(&self, actual: &Self) -> bool {
        if actual.original_max_pages != self.selected_max_pages {
            return false;
        }
        let mut normalized = actual.clone();
        normalized.original_max_pages = self.original_max_pages;
        self == &normalized
    }

    // I/O restriction only. Genuine journal attempts/capacity/entered policy remain mandatory.
    pub(crate) fn logical_restriction(&self) -> Option<LogicalBudget> {
        let unit = self.main.allocation_unit();
        LogicalBudget::new(
            [self.main.ceiling, self.wal.ceiling, self.shm.ceiling],
            unit,
            unit,
            unit,
        )
    }
}

// Exact existing passive restriction formula, not a physical allocation qualification.
pub(crate) fn logical_ceiling(bytes: u64, window: u64, unit: u64) -> Option<u64> {
    if unit == 0 {
        return None;
    }
    let ceiling = window
        .checked_add(bytes)?
        .checked_mul(4)?
        .checked_add(4 * 1024 * 1024)?;
    let ceiling = ceiling
        .checked_add(unit - 1)?
        .checked_div(unit)?
        .checked_mul(unit)?;
    (ceiling > 0 && ceiling <= i64::MAX as u64).then_some(ceiling)
}

pub(crate) const MAX_OBSERVATION: Duration = Duration::from_secs(2);
const MAX_VM_OPS: u64 = 10_000_000;
const PROGRESS_OPS: i32 = 1000;
pub(crate) fn bounded_read<T>(
    connection: &Connection,
    deadline: Instant,
    read: impl FnOnce() -> Result<T, LifecycleAllocationError>,
) -> Result<T, LifecycleAllocationError> {
    if deadline
        .checked_duration_since(Instant::now())
        .is_none_or(|v| v.is_zero() || v > MAX_OBSERVATION)
    {
        return Err(LifecycleAllocationError::Budget);
    }
    let remaining = Arc::new(AtomicU64::new(MAX_VM_OPS));
    let progress = Arc::clone(&remaining);
    connection.progress_handler(
        PROGRESS_OPS,
        Some(move || {
            let next = progress
                .load(Ordering::Relaxed)
                .saturating_sub(PROGRESS_OPS as u64);
            progress.store(next, Ordering::Relaxed);
            next == 0 || Instant::now() >= deadline
        }),
    );
    let result = read();
    connection.progress_handler(0, Option::<fn() -> bool>::None);
    if Instant::now() >= deadline || remaining.load(Ordering::Relaxed) == 0 {
        Err(LifecycleAllocationError::Budget)
    } else {
        result
    }
}

pub(crate) fn capture(
    connection: &OwnedConnection,
    root: &BoundDatabaseRoot,
    name: &std::path::Path,
    role: LifecycleAllocationRole,
    deadline: Instant,
) -> Result<LifecycleDatabaseAllocation, LifecycleAllocationError> {
    use LifecycleAllocationError::Unknown;
    let read = |name| {
        connection
            .pragma_query_value(None, name, |r| r.get::<_, u64>(0))
            .map_err(|_| Unknown)
    };
    let page_size = read("page_size")?;
    let page_count = read("page_count")?;
    let original_max_pages = read("max_page_count")?;
    let mode: String = connection
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(|_| Unknown)?;
    let locking: String = connection
        .pragma_query_value(None, "locking_mode", |r| r.get(0))
        .map_err(|_| Unknown)?;
    let query_only: bool = connection
        .pragma_query_value(None, "query_only", |r| r.get(0))
        .map_err(|_| Unknown)?;
    if !mode.eq_ignore_ascii_case("wal")
        || !locking.eq_ignore_ascii_case("normal")
        || query_only
        || !(512..=65536).contains(&page_size)
        || !page_size.is_power_of_two()
        || page_count == 0
        || original_max_pages == 0
        || original_max_pages > 0xffff_fffe
    {
        return Err(Unknown);
    }
    let cap = match role {
        LifecycleAllocationRole::GlobalReferences => 512 * 1024 * 1024,
        LifecycleAllocationRole::SourceLeaseMaintenance => 64 * 1024 * 1024,
    };
    let selected_max_pages = original_max_pages.min(cap / page_size);
    if page_count > selected_max_pages {
        return Err(Unknown);
    }
    let objects = connection
        .retained_wal_objects(root, name, deadline)
        .ok_or(Unknown)?;
    let unit = objects.main.allocation_unit;
    if [&objects.wal, &objects.shm]
        .iter()
        .any(|f| f.device != objects.main.device || f.allocation_unit != unit)
    {
        return Err(Unknown);
    }
    let maximum_main = page_size.checked_mul(selected_max_pages).ok_or(Unknown)?;
    if objects.main.bytes > maximum_main {
        return Err(Unknown);
    }
    let maximum_wal = selected_max_pages
        .checked_mul(page_size.checked_add(24).ok_or(Unknown)?)
        .and_then(|v| v.checked_add(32))
        .ok_or(Unknown)?;
    let maximum_shm = selected_max_pages
        .checked_mul(16)
        .and_then(|v| v.checked_add(32768))
        .ok_or(Unknown)?;
    // Restrict logical charged I/O to four complete page windows plus fixed bookkeeping.
    // This is an explicit conservative refusal ceiling, NOT a physical worst-case grant.
    let object = |facts: WalObjectFacts, window: u64| -> Option<LifecycleObjectAllocation> {
        let ceiling = logical_ceiling(facts.bytes, window, unit)?;
        Some(LifecycleObjectAllocation { facts, ceiling })
    };
    let WalObjects { main, wal, shm } = objects;
    let result = LifecycleDatabaseAllocation {
        role,
        page_size,
        page_count,
        original_max_pages,
        selected_max_pages,
        main: object(main, maximum_main).ok_or(Unknown)?,
        wal: object(wal, maximum_wal).ok_or(Unknown)?,
        shm: object(shm, maximum_shm).ok_or(Unknown)?,
    };
    if result.logical_restriction().is_none() || Instant::now() >= deadline {
        return Err(Unknown);
    }
    Ok(result)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
pub(crate) mod workloads;
