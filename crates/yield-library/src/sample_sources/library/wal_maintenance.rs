//! Eligible global post-commit maintenance; no read or receipt replay supplies this marker.
use std::cell::Cell;

use super::connection::LibraryDatabase;
use crate::app_dirs::WritableProfileGuard;

#[derive(Default)]
pub(super) struct OrdinaryWalMaintenance {
    committed: Cell<bool>,
    policy: crate::sqlite_wal::CheckpointPolicy,
}

impl LibraryDatabase {
    /// Mark only a confirmed ordinary transaction commit, never a replay/no-op acknowledgement.
    pub(super) fn note_ordinary_commit(&self) {
        self.ordinary_wal_maintenance.committed.set(true);
    }

    /// A new immutable registry receipt commits even when it records RegistryChanged.
    pub(super) fn note_registry_participant_commit(&self) {
        self.ordinary_wal_maintenance.committed.set(true);
    }

    /// Mark only a successful new operation-link receipt commit, never exact replay.
    pub(super) fn note_operation_link_commit(&self) {
        self.ordinary_wal_maintenance.committed.set(true);
    }

    /// A new authored receipt commits even when its disposition rejects the edit.
    pub(super) fn note_harvest_authored_commit(&self) {
        self.ordinary_wal_maintenance.committed.set(true);
    }

    /// A new copy receipt commits even when its disposition retains a rejected edit.
    pub(super) fn note_harvest_copy_commit(&self) {
        self.ordinary_wal_maintenance.committed.set(true);
    }

    /// A new reference receipt commits even when its disposition retains the old references.
    pub(super) fn note_reference_participant_commit(&self) {
        self.ordinary_wal_maintenance.committed.set(true);
    }

    #[cfg(test)]
    pub(in crate::sample_sources::library) fn ordinary_commit_pending_for_test(&self) -> bool {
        self.ordinary_wal_maintenance.committed.get()
    }

    /// Consume the marker on the same worker after its post-command ownership check.
    /// Failure is best effort and cannot turn an already committed write into an SQL failure.
    pub(super) fn maintain_after_ordinary_commit(&self, guard: &WritableProfileGuard) {
        if !self.ordinary_wal_maintenance.committed.replace(false) {
            return;
        }
        if self.validate_profile_guard(guard).is_ok() {
            self.ordinary_wal_maintenance.policy.maybe_checkpoint(
                &self.connection,
                "library_db",
                "global_writer",
            );
        }
    }
}

#[cfg(test)]
mod tests;
