//! Private metadata precondition for future attempt persistence, never a commit permit.
use super::{OperationJournalStore, OperationJournalStoreError, database_claims, ordinary_claims};
use crate::operation_journal::full_capacity::FullCapacityAllocationClass;
use crate::operation_journal::{AdmissionClaimId, OperationId};
use rusqlite::params;
use uuid::Uuid;

impl OperationJournalStore {
    /// Hold the process extension exclusion across the future attempt transaction. A successful
    /// check proves only bounded logical journal metadata and original durable claim retention;
    /// it does not prove physical free space, owner registration, legal commands or live capacity.
    /// The caller must keep this guard until its SQLite transaction has quiesced, and revalidate
    /// the namespace immediately before commit. No serialized observation can recreate it.
    #[allow(dead_code)] // Actual attempt insertion is the remaining scope of OPT-2206.
    pub(super) fn prepare_database_attempt_metadata(
        &self,
        operation: OperationId,
    ) -> Result<std::sync::RwLockReadGuard<'static, ()>, OperationJournalStoreError> {
        let extensions =
            crate::sqlite_ext::try_extension_free_metadata_scope().ok_or_else(|| {
                database_claims::error("attempt metadata requires extension exclusion")
            })?;
        if !self.connection.is_autocommit() {
            return Err(database_claims::error(
                "attempt metadata requires confirmed transaction state",
            ));
        }
        self.validate_namespace()?;
        self.read_database_claim_inventory(operation)?
            .ok_or_else(|| {
                database_claims::error("attempt metadata requires original database bindings")
            })?;
        let record = self
            .read_row(operation)?
            .ok_or_else(|| database_claims::error("attempt operation missing"))?
            .record;
        // Includes the entire bounded database, all indexes/rows/control images, and the maximum
        // DELETE rollback copy. Existing database bytes are counted again, never credited back.
        self.validate_database_claim_metadata_capacity(&record)?;
        self.validate_database_attempt_journal_claims(&record)?;
        self.validate_namespace()?;
        Ok(extensions)
    }
    pub(super) fn validate_database_attempt_journal_claims(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_database_attempt_journal_claims_with_input(record, None)
    }

    pub(super) fn validate_database_attempt_journal_claims_with_input(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_database_claim_metadata_volume_with_input(record, input)?;
        let operation = record.operation_id;
        let plan = super::full_capacity_plan_for_record(&record)?;
        let expected: Vec<_> = plan
            .allocations()
            .iter()
            .filter(|a| a.class() == FullCapacityAllocationClass::JournalRecord)
            .collect();
        // Bounded SQL projection checks type/length before materializing caller-corrupted keys.
        let mut statement = self.connection.prepare(
            "SELECT CASE WHEN typeof(claim_id)='text' AND length(claim_id)=36 THEN claim_id END
             FROM operation_journal_ordinary_claim
             WHERE operation_id=?1 AND allocation_class='JournalRecord'
             ORDER BY claim_id LIMIT 257",
        )?;
        let ids = statement
            .query_map(params![operation.to_string()], |r| {
                r.get::<_, Option<String>>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if ids.len() > 256 || ids.len() != expected.len() || ids.is_empty() {
            return Err(database_claims::error("attempt journal claim cardinality"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for text in ids {
            let text =
                text.ok_or_else(|| database_claims::error("attempt journal claim key shape"))?;
            let uuid = Uuid::parse_str(&text).map_err(database_claims::error)?;
            if uuid.is_nil() || uuid.to_string() != text {
                return Err(database_claims::error("attempt journal claim key identity"));
            }
            let id = AdmissionClaimId::from_uuid(uuid).map_err(database_claims::error)?;
            let claim = ordinary_claims::read_claim(&self.connection, id)
                .map_err(database_claims::error)?
                .ok_or_else(|| database_claims::error("attempt journal claim missing"))?;
            if claim.operation_id() != operation
                || claim.allocation_class() != FullCapacityAllocationClass::JournalRecord
                || !seen.insert(claim.volume().device)
                || !expected.iter().any(|a| {
                    a.volume() == claim.volume() && a.allocated_bytes() == claim.allocated_bytes()
                })
                || ordinary_claims::read_release(&self.connection, id)
                    .map_err(database_claims::error)?
                    .is_some()
            {
                return Err(database_claims::error(
                    "attempt journal claim differs from retained original plan",
                ));
            }
        }
        self.validate_namespace_with_input(input, true)?;
        Ok(())
    }
}
