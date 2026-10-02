//! Application directory helpers anchored to a single `.yield` folder.
//!
//! The helpers centralize where config and log files live across platforms,
//! defaulting to the OS config directory (e.g., `%APPDATA%` on Windows) and
//! allowing a `YIELD_CONFIG_HOME` override for tests or portable setups.
//! Test executables default to an isolated `automated-tests` profile unless
//! they explicitly request the live profile or another config root.

mod error;
mod overrides;
mod paths;
mod profile;
mod profile_ownership;
mod resolution;
mod state;
mod storage_usage;

pub use error::AppDirError;
pub use overrides::{AppRootGuard, ConfigBaseGuard};
pub use paths::{
    handoff_staging_dir, logs_dir, operation_journal_dir, rebuildable_cache_root_dir,
    waveform_cache_dir,
};
pub use profile::{PersistenceMode, PersistenceProfileGuard, ResolvedPersistence};
pub(crate) use profile_ownership::{
    ProfileChildLease, open_canonical_global_app_root, open_no_follow_directory,
};
pub use profile_ownership::{ProfileOwnershipError, WritableProfileGuard};
pub use resolution::{
    app_root_dir, config_base_dir_path, ensure_test_config_base, persistence_mode,
    resolve_persistence, set_app_root_override,
};
pub use storage_usage::{
    CacheClearOutcome, CacheMaintenanceError, DisposableCacheNamespace, GlobalStorageUsage,
    MAX_MAINTENANCE_DEPTH, MAX_MAINTENANCE_ENTRIES, MAX_MAINTENANCE_OPEN_HANDLES,
    StorageUsageError, global_storage_usage, global_storage_usage_for_profile,
};

/// Name of the application directory that lives under the OS config root.
pub const APP_DIR_NAME: &str = ".yield";
/// Name of the directory that stores explicit non-live persistence profiles.
pub const PROFILE_DIR_NAME: &str = "profiles";
/// Canonical non-live profile name used for sandbox/manual QA runs.
pub const SANDBOX_PROFILE_NAME: &str = "sandbox";
/// Canonical non-live profile name used for automated validation runs.
pub const AUTOMATED_PROFILE_NAME: &str = "automated-tests";

const CONFIG_PROFILE_ENV: &str = "YIELD_CONFIG_PROFILE";
const TEST_EXECUTABLE_DIR_NAME: &str = "deps";

#[cfg(test)]
mod tests;
