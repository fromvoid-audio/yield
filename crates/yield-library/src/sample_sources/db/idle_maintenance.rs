//! Idle maintenance borrows original ownership without renewing the control writer.
use super::{SourceAuthorityError, SourceDatabase, SourceDbError, SourceLeaseError};

impl SourceDatabase {
    pub(super) fn maintain_idle_wal(&self) -> Result<(), SourceAuthorityError> {
        // Idle work never enters an active read/write transaction or a prepared metadata policy.
        if !self.connection.is_autocommit() {
            return Ok(());
        }
        if self.tag_metadata_policy_state.get() != super::TAG_METADATA_POLICY_IDLE {
            return Err(SourceDbError::MetadataPolicyUnconfirmed.into());
        }
        // Require real ownership even in test-support builds; no path-only fixture can qualify.
        let profile = self
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        let lease = self
            .source_lease
            .as_ref()
            .ok_or(SourceLeaseError::Required)?;
        self.database_file_binding
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.database_root_binding
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        // Existing in-flight custody prevents release while this admitted maintenance pass runs.
        // It is not a journal-issued mutation grant. Native validation is readonly: never call
        // prepare_for_mutation, which can renew through the separate CONTROL writer.
        // Count this original use under the short gate, then validate while its RAII pin is
        // retained. Native/SQLite validation must not hold the gate mutex needed by closing.
        let _custody = lease.effect_gate().begin(|| Ok(()))?;
        self.validate_profile_owned_open(profile)?;
        self.checkpoint_policy.maybe_checkpoint(
            &self.connection,
            "source_db",
            self.telemetry_label,
        );
        self.validate_profile_owned_open(profile)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/idle_maintenance.rs"]
mod tests;
