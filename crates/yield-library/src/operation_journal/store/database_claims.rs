//! Actual SQLite persistence of an immutable exact binding image. No row is a live permit.
use super::{OperationJournalStore, OperationJournalStoreError};
use crate::operation_journal::database_claims::{
    DatabaseClaimInventory, MAX_INVENTORY_BYTES, MAX_RELATION_ROWS, READ_PAGE_ROWS,
};
use crate::operation_journal::{AdmissionClaimInventory, OperationId};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub(super) mod command_backing;
pub(super) mod comparison;
mod funded;
pub(super) mod scalar_projection;
pub(super) use funded::{Inventory as RetainedDatabaseInventory, read_with_input};

pub(super) const BINDING_TABLE: &str = "operation_journal_database_binding";
pub(super) const SLICE_TABLE: &str = "operation_journal_database_claim_slice";
#[cfg(test)]
std::thread_local! {
    pub(super) static INVENTORY_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static SCALAR_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(super) const CREATE_BINDING: &str = "CREATE TABLE operation_journal_database_binding (
    binding_id TEXT NOT NULL PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES operation_journal(operation_id),
    ordinal INTEGER NOT NULL CHECK(typeof(ordinal)='integer' AND ordinal BETWEEN 0 AND 63),
    role INTEGER NOT NULL CHECK(typeof(role)='integer' AND role IN(1,2)),
    volume TEXT NOT NULL CHECK(length(volume) BETWEEN 1 AND 20 AND volume NOT GLOB '*[^0-9]*'),
    main_file TEXT NOT NULL CHECK(typeof(main_file)='text' AND length(CAST(main_file AS BLOB)) BETWEEN 1 AND 256),
    decision_nonce TEXT NOT NULL CHECK(length(decision_nonce)=36),
    inventory_digest BLOB NOT NULL CHECK(typeof(inventory_digest)='blob' AND length(inventory_digest)=32),
    inventory BLOB,
    CHECK(length(binding_id)=36 AND length(operation_id)=36),
    CHECK((ordinal=0 AND typeof(inventory)='blob' AND length(inventory) BETWEEN 1 AND 65536)
       OR (ordinal>0 AND inventory IS NULL)),
    UNIQUE(operation_id,ordinal), UNIQUE(operation_id,binding_id), UNIQUE(decision_nonce,ordinal),
    UNIQUE(operation_id,volume,main_file)
)";
pub(super) const CREATE_SLICE: &str = "CREATE TABLE operation_journal_database_claim_slice (
    slice_id TEXT NOT NULL PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES operation_journal(operation_id),
    binding_id TEXT NOT NULL,
    claim_id TEXT NOT NULL REFERENCES operation_journal_ordinary_claim(claim_id),
    participant_id TEXT NOT NULL,
    direction INTEGER NOT NULL CHECK(typeof(direction)='integer' AND direction BETWEEN 0 AND 3),
    class INTEGER NOT NULL CHECK(typeof(class)='integer' AND class BETWEEN 1 AND 4),
    ceiling INTEGER NOT NULL CHECK(typeof(ceiling)='integer' AND ceiling>0),
    wal_ceiling INTEGER NOT NULL CHECK(typeof(wal_ceiling)='integer' AND wal_ceiling>=0),
    shm_ceiling INTEGER NOT NULL CHECK(typeof(shm_ceiling)='integer' AND shm_ceiling>=0),
    CHECK(length(slice_id)=36 AND length(operation_id)=36 AND length(binding_id)=36
       AND length(claim_id)=36 AND length(participant_id)=36),
    CHECK((class IN(1,3) AND wal_ceiling=0 AND shm_ceiling=0)
       OR (class IN(2,4) AND wal_ceiling<=ceiling AND shm_ceiling=ceiling-wal_ceiling)),
    FOREIGN KEY(operation_id,binding_id) REFERENCES operation_journal_database_binding(operation_id,binding_id),
    UNIQUE(operation_id,slice_id)
)";
pub(super) fn create(connection: &Connection) -> Result<(), OperationJournalStoreError> {
    connection.execute_batch(CREATE_BINDING)?;
    connection.execute_batch(CREATE_SLICE)?;
    Ok(())
}
pub(super) fn error(detail: impl ToString) -> OperationJournalStoreError {
    OperationJournalStoreError::InconsistentRecord {
        reason: format!("database claim inventory: {}", detail.to_string()),
    }
}
fn class_tag(class: crate::operation_journal::full_capacity::FullCapacityAllocationClass) -> i64 {
    use crate::operation_journal::full_capacity::FullCapacityAllocationClass::*;
    match class {
        SourceDatabaseCommit => 1,
        SourceWalShm => 2,
        GlobalDatabaseCommit => 3,
        GlobalWalShm => 4,
        _ => 0,
    }
}
fn bounded_count(
    connection: &Connection,
    table: &str,
) -> Result<usize, OperationJournalStoreError> {
    // Hard-coded private table names only. Stop after MAX+1 rather than materializing rows.
    let count: i64 = connection.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} LIMIT ?1)"),
        [MAX_RELATION_ROWS as i64 + 1],
        |r| r.get(0),
    )?;
    if count < 0 || count > MAX_RELATION_ROWS as i64 {
        return Err(error("relation row ceiling"));
    }
    Ok(count as usize)
}
pub(super) fn insert(
    connection: &Connection,
    operation: OperationId,
    inventory: &DatabaseClaimInventory,
) -> Result<(), OperationJournalStoreError> {
    let encoded = inventory.encode().map_err(error)?;
    let digest = inventory.digest().map_err(error)?;
    for (table, additional) in [
        (BINDING_TABLE, inventory.bindings().len()),
        (SLICE_TABLE, inventory.slices().len()),
    ] {
        if bounded_count(connection, table)?
            .checked_add(additional)
            .is_none_or(|n| n > MAX_RELATION_ROWS)
        {
            return Err(error("relation exhausted"));
        }
    }
    let nonce = inventory.header().decision_nonce.to_string();
    for (ordinal, b) in inventory.bindings().iter().enumerate() {
        connection.execute("INSERT INTO operation_journal_database_binding(binding_id,operation_id,ordinal,role,volume,main_file,decision_nonce,inventory_digest,inventory) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![b.id.to_string(),operation.to_string(),ordinal as i64,b.role as u8,b.volume.to_string(),b.main_file,nonce,digest.as_slice(),if ordinal==0 {Some(encoded.as_slice())} else {None}])?;
    }
    for s in inventory.slices() {
        connection.execute("INSERT INTO operation_journal_database_claim_slice(slice_id,operation_id,binding_id,claim_id,participant_id,direction,class,ceiling,wal_ceiling,shm_ceiling) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![s.id.to_string(),operation.to_string(),s.binding.to_string(),s.claim.to_string(),s.participant.to_string(),s.direction as u8,class_tag(s.class),s.ceiling as i64,s.wal_ceiling as i64,s.shm_ceiling as i64])?;
    }
    verify(connection, operation, inventory)
}
pub(super) fn read(
    connection: &Connection,
    operation: OperationId,
) -> Result<Option<DatabaseClaimInventory>, OperationJournalStoreError> {
    let bytes:Option<Option<Vec<u8>>>=connection.query_row(
        "SELECT CASE WHEN typeof(inventory)='blob' AND length(inventory) BETWEEN 1 AND ?2 THEN inventory END FROM operation_journal_database_binding WHERE operation_id=?1 AND ordinal=0",
        params![operation.to_string(),MAX_INVENTORY_BYTES as i64],|r|r.get(0)).optional()?;
    let Some(bytes) = bytes else {
        let present:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM operation_journal_database_binding WHERE operation_id=?1) OR EXISTS(SELECT 1 FROM operation_journal_database_claim_slice WHERE operation_id=?1)",[operation.to_string()],|r|r.get(0))?;
        if present {
            return Err(error("missing root binding retains unsupported evidence"));
        }
        // Old operations stay inspectable, but no binding/required exception is reconstructed.
        return Ok(None);
    };
    let bytes = bytes.ok_or_else(|| error("root encoding type/length"))?;
    #[cfg(test)]
    INVENTORY_COPIED_BYTES.with(|counter| counter.set(counter.get().saturating_add(bytes.len())));
    let inventory = DatabaseClaimInventory::decode(&bytes).map_err(error)?;
    verify(connection, operation, &inventory)?;
    Ok(Some(inventory))
}
pub(super) fn verify(
    connection: &Connection,
    operation: OperationId,
    inventory: &DatabaseClaimInventory,
) -> Result<(), OperationJournalStoreError> {
    inventory.validate().map_err(error)?;
    let expected = inventory.encode().map_err(error)?;
    let digest = inventory.digest().map_err(error)?;
    let mut statement=connection.prepare("SELECT CASE WHEN length(binding_id)=36 THEN binding_id END,ordinal,role,CASE WHEN length(volume)<=20 THEN volume END,CASE WHEN length(CAST(main_file AS BLOB))<=256 THEN main_file END,CASE WHEN length(decision_nonce)=36 THEN decision_nonce END,CASE WHEN typeof(inventory_digest)='blob' AND length(inventory_digest)=32 THEN inventory_digest END,CASE WHEN typeof(inventory)='blob' AND length(inventory)<=65536 THEN inventory END FROM operation_journal_database_binding WHERE operation_id=?1 ORDER BY ordinal LIMIT 65")?;
    let mut rows = statement.query([operation.to_string()])?;
    for (ordinal, b) in inventory.bindings().iter().enumerate() {
        let row = rows.next()?.ok_or_else(|| error("missing binding row"))?;
        if !scalar_projection::matches_text(row, 0, &b.id.to_string())?
            || row.get::<_, i64>(1)? != ordinal as i64
            || row.get::<_, i64>(2)? != b.role as i64
            || !scalar_projection::matches_text(row, 3, &b.volume.to_string())?
            || !scalar_projection::matches_text(row, 4, b.main_file.as_str())?
            || !scalar_projection::matches_text(
                row,
                5,
                &inventory.header().decision_nonce.to_string(),
            )?
            || row.get::<_, Option<Vec<u8>>>(6)?.as_deref() != Some(digest.as_slice())
            || row.get::<_, Option<Vec<u8>>>(7)?.as_deref()
                != if ordinal == 0 {
                    Some(expected.as_slice())
                } else {
                    None
                }
        {
            return Err(error("binding image/readback conflict"));
        }
    }
    if rows.next()?.is_some() {
        return Err(error("extra binding row"));
    }
    let mut statement=connection.prepare("SELECT CASE WHEN length(slice_id)=36 THEN slice_id END,CASE WHEN length(binding_id)=36 THEN binding_id END,CASE WHEN length(claim_id)=36 THEN claim_id END,CASE WHEN length(participant_id)=36 THEN participant_id END,direction,class,ceiling,wal_ceiling,shm_ceiling FROM operation_journal_database_claim_slice WHERE operation_id=?1 ORDER BY claim_id,binding_id,participant_id,direction,slice_id LIMIT 257")?;
    let mut rows = statement.query([operation.to_string()])?;
    for s in inventory.slices() {
        let row = rows.next()?.ok_or_else(|| error("missing slice row"))?;
        for (index, id) in [s.id, s.binding, s.claim, s.participant]
            .into_iter()
            .enumerate()
        {
            if !scalar_projection::matches_text(row, index, &id.to_string())? {
                return Err(error("slice identity conflict"));
            }
        }
        for (index, value) in [
            s.direction as i64,
            class_tag(s.class),
            s.ceiling as i64,
            s.wal_ceiling as i64,
            s.shm_ceiling as i64,
        ]
        .into_iter()
        .enumerate()
        {
            if row.get::<_, i64>(index + 4)? != value {
                return Err(error("slice allocation/direction conflict"));
            }
        }
    }
    if rows.next()?.is_some() {
        return Err(error("extra slice row"));
    }
    Ok(())
}

impl OperationJournalStore {
    /// Recover an operation whose acceptance reply was lost before the caller learned its ID.
    /// Unique nonce/ordinal zero names the original operation. Caller digest is correlation,
    /// never live authority; a conflicting expected image remains unsupported.
    /// None observes no binding with this nonce; it does not prove rollback or authorize refund.
    pub(crate) fn read_database_claim_inventory_for_nonce(
        &self,
        nonce: Uuid,
        expected_digest: [u8; 32],
    ) -> Result<Option<(OperationId, DatabaseClaimInventory)>, OperationJournalStoreError> {
        if nonce.is_nil() || !self.connection.is_autocommit() {
            return Err(error(
                "invalid nonce or unconfirmed transaction for readback",
            ));
        }
        self.validate_namespace()?;
        // Read the first retained row for the nonce, not only ordinal zero. A missing
        // root with later rows is partial durable evidence, never confirmed absence.
        // The unique nonce/ordinal index bounds this to one row in the original query.
        let first: Option<(Option<scalar_projection::UuidProjection>, i64)> = self.connection.query_row(
            "SELECT CASE WHEN typeof(operation_id)='text' AND length(operation_id)=36 THEN operation_id END,ordinal FROM operation_journal_database_binding WHERE decision_nonce=?1 ORDER BY ordinal LIMIT 1",
            [nonce.to_string()], |row| Ok((scalar_projection::uuid(row, 0)?, row.get(1)?)),
        ).optional()?;
        let Some((text, ordinal)) = first else {
            self.validate_namespace()?;
            return Ok(None);
        };
        if ordinal != 0 {
            return Err(error("nonce retains binding evidence without its root"));
        }
        let text = text.ok_or_else(|| error("nonce operation identity bound"))?;
        let id = text.id.map_err(error)?;
        if id.is_nil() || !text.canonical {
            return Err(error("nonce operation identity"));
        }
        let operation = OperationId::from_uuid(id);
        let inventory = self
            .read_database_claim_inventory(operation)?
            .ok_or_else(|| error("nonce root vanished"))?;
        if inventory.header().decision_nonce != nonce
            || inventory.digest().map_err(error)? != expected_digest
        {
            return Err(error("nonce inventory digest conflict"));
        }
        self.validate_namespace()?;
        Ok(Some((operation, inventory)))
    }
    /// Real persistence entrypoint for a future genuine all-owner latch. The lease remains
    /// private owner-thread capacity authority; inventory fields and the nonce are observations.
    /// This method neither allocates an OperationId nor calls the provider a second time.
    pub(crate) fn settle_initial_database_claims_with_lease(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
        claims: &AdmissionClaimInventory,
        inventory: &DatabaseClaimInventory,
        provider: &crate::operation_journal::capacity_provider::CapacityProvider,
        lease: crate::operation_journal::capacity_provider::ProvisionalCapacityLease,
    ) -> Result<super::InitialClaimSettlement, super::InitialClaimSettlementError> {
        let _extensions =
            crate::sqlite_ext::try_extension_free_metadata_scope().ok_or_else(|| {
                super::InitialClaimSettlementError::Store(error(
                    "extension-free journal policy unavailable",
                ))
            })?;
        self.validate_database_claim_metadata_capacity(record)
            .map_err(super::InitialClaimSettlementError::Store)?;
        self.settle_initial_claims_with_inventory(record, claims, provider, lease, Some(inventory))
    }

    /// Include this conservative complete DELETE-journal peak before admission. The same
    /// bounded bundled-pager policy used for captured metadata caps the whole journal at 512MiB,
    /// disables spill/savepoints/temporary disk objects and checks actual descriptor identity.
    /// The logical peak never qualifies a native provider or protected reserve.
    pub(super) fn validate_database_claim_metadata_capacity(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        self.configure_recording_metadata_policy()?;
        self.validate_database_claim_metadata_volume(record)
    }
    /// Read-only restart/readback check. Historical observations never change pager policy.
    pub(super) fn validate_database_claim_metadata_volume(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_database_claim_metadata_volume_with_input(record, None)
    }

    pub(super) fn validate_database_claim_metadata_volume_with_input(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        let working = input
            .map(|input| {
                let namespace = super::namespace_backing::working_bytes(
                    &self.database_root,
                    &self.profile_guard,
                    &self.database_path,
                )
                .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
                const REASONS: &str = concat!(
                    "not a disk handle",
                    "unknown disk object identity",
                    "Windows volume query failed: NTSTATUS(-2147483648)",
                    "Windows disk object identity changed during capacity query",
                    "invalid Windows volume capacity record",
                    "filesystem allocation unit is zero",
                    "free-space arithmetic overflow",
                    "invalid free-space block count",
                    "invalid filesystem fragment size",
                    "invalid filesystem block size",
                    "journal descriptor replaced",
                    "journal peak overflow",
                    "complete journal/rollback peak not covered on actual descriptor volume",
                );
                let bytes = super::database_attempt_metadata::plan_backing(record)?
                    .checked_add(namespace)
                    .and_then(|n| n.checked_add(6 * REASONS.len()))
                    .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
                input.reserve_historical_scratch(bytes)
            })
            .transpose()?;
        let mut native = None;
        let result = (|| {
            let opened = crate::sample_sources::db::sqlite_identity::open_existing_database_file(
                &self.database_root,
                std::path::Path::new(super::JOURNAL_DATABASE_FILE_NAME),
                &self.database_path,
            )
            .map_err(super::map_identity_error)?;
            if opened.identity != self.database_identity {
                return Err(error("journal descriptor replaced"));
            }
            let facts = if let Some(input) = input {
                crate::durable_capacity::descriptor_capacity_facts_with_funding(
                    &opened.file,
                    &mut |bytes| {
                        // Original SDK text, discovery text, its Display scratch
                        // and outer reason coexist: exact copy plus three
                        // twofold String capacities, with fixed framing prepaid.
                        let bytes = bytes
                            .checked_mul(8)
                            .and_then(|n| n.checked_add(128))
                            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
                        native = Some(input.reserve_historical_scratch(bytes)?);
                        Ok::<_, OperationJournalStoreError>(())
                    },
                )
                .map_err(|observed| match observed {
                    crate::durable_capacity::FundedDescriptorError::Observed(observed, _) => {
                        error(observed)
                    }
                    #[cfg(windows)]
                    crate::durable_capacity::FundedDescriptorError::Funding(error) => error,
                })?
            } else {
                crate::durable_capacity::descriptor_capacity_facts(&opened.file).map_err(error)?
            };
            let plan = super::full_capacity_plan_for_record(record)?;
            let charged=plan.allocations().iter().filter(|a|a.class()==crate::operation_journal::full_capacity::FullCapacityAllocationClass::JournalRecord && a.volume()==&facts.identity)
            .try_fold(0u64,|sum,a|sum.checked_add(a.logical_bytes())).ok_or_else(||error("journal peak overflow"))?;
            if charged < crate::operation_journal::recording_commands::RECORDING_JOURNAL_PEAK_BYTES
            {
                return Err(error(
                    "complete journal/rollback peak not covered on actual descriptor volume",
                ));
            }
            self.validate_namespace_with_input(input, true)?;
            Ok(())
        })();
        if result.is_err() {
            if let (Some(input), Some(working)) = (input, working) {
                input.retain_historical_error(working, native, None, None);
            }
        }
        result
    }
    pub(crate) fn read_database_claim_inventory(
        &self,
        operation: OperationId,
    ) -> Result<Option<DatabaseClaimInventory>, OperationJournalStoreError> {
        if !self.connection.is_autocommit() {
            return Err(error(
                "unconfirmed transaction cannot provide durable inventory readback",
            ));
        }
        self.validate_namespace()?;
        let value = read(&self.connection, operation)?;
        if let Some(inventory) = &value {
            self.validate_database_claim_operation(operation, inventory)?;
        }
        self.validate_namespace()?;
        Ok(value)
    }
    /// Compare the immutable intent and original aggregate ledger during restart or lost-reply readback.
    pub(super) fn validate_database_claim_operation(
        &self,
        operation: OperationId,
        inventory: &DatabaseClaimInventory,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_database_claim_operation_with_input(operation, inventory, None)
    }

    pub(super) fn validate_database_claim_operation_with_input(
        &self,
        operation: OperationId,
        inventory: &DatabaseClaimInventory,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        if let Some(input) = input {
            let present =
                self.inspect_complete_claim_operation(operation, input, |record, _| {
                    self.validate_database_claim_record_with_input(record, inventory, Some(input))
                })?;
            if !present {
                return Err(error("orphan inventory"));
            }
            Ok(())
        } else {
            let record = self
                .read_row(operation)?
                .ok_or_else(|| error("orphan inventory"))?
                .record;
            self.validate_database_claim_record(&record, inventory)
        }
    }

    fn validate_database_claim_record(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
        inventory: &DatabaseClaimInventory,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_database_claim_record_with_input(record, inventory, None)
    }

    fn validate_database_claim_record_with_input(
        &self,
        record: &crate::operation_journal::StoredOperationRecord,
        inventory: &DatabaseClaimInventory,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        let operation = record.operation_id;
        self.verify_recording_initial_readback_with_input(record, input)?;
        match input {
            Some(input) => command_backing::validate_record(record, inventory, input)?,
            None => inventory.validate_record(record).map_err(error)?,
        }
        if let Some(input) = input {
            return comparison::validate(self, record, inventory, input);
        }
        let plan = super::full_capacity_plan_for_record(record)?;
        let plan = crate::operation_journal::full_capacity::FullCapacityPlan::new(
            plan.allocations()
                .iter()
                .filter(|a| class_tag(a.class()) != 0)
                .cloned()
                .collect(),
        )
        .map_err(error)?;
        let mut claims = Vec::new();
        // Original aggregate image is bounded, including post-intent claims. Bind only
        // the immutable initial DB classes; unrelated chunk rows remain separately audited.
        let mut query=self.connection.prepare("SELECT CASE WHEN length(claim_id)=36 THEN claim_id END FROM operation_journal_ordinary_claim WHERE operation_id=?1 AND allocation_class IN('SourceDatabaseCommit','SourceWalShm','GlobalDatabaseCommit','GlobalWalShm') ORDER BY claim_id LIMIT 257")?;
        let ids = query
            .query_map([operation.to_string()], |r| scalar_projection::uuid(r, 0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if ids.len() > 256 {
            return Err(error("operation database claim bound"));
        }
        for text in ids {
            let text = text.ok_or_else(|| error("ordinary claim identity bound"))?;
            let id = crate::operation_journal::AdmissionClaimId::from_uuid(text.id.map_err(error)?)
                .map_err(error)?;
            let claim = super::ordinary_claims::read_claim(&self.connection, id)
                .map_err(error)?
                .ok_or_else(|| error("missing ordinary claim"))?;
            claims.push(
                crate::operation_journal::AdmissionClaim::new(
                    claim.claim_id(),
                    claim.volume().clone(),
                    claim.allocation_class(),
                    claim.allocated_bytes(),
                )
                .map_err(error)?,
            );
            if super::ordinary_claims::read_release(&self.connection, id)
                .map_err(error)?
                .is_some()
            {
                return Err(error(
                    "slice-bound ordinary claim was released without attempt settlement",
                ));
            }
        }
        let claims = AdmissionClaimInventory::new(claims).map_err(error)?;
        inventory.validate_against(&plan, &claims).map_err(error)?;
        Ok(())
    }
    /// Bounded restart audit. Stored bytes never recreate a live nonce, reservation or permit.
    #[cfg(test)]
    pub(super) fn validate_database_claims_inside_transaction(
        &self,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_database_claims_inside_transaction_with_input(None)
    }

    pub(super) fn validate_database_claims_inside_transaction_with_input(
        &self,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        bounded_count(&self.connection, BINDING_TABLE)?;
        bounded_count(&self.connection, SLICE_TABLE)?;
        for table in [BINDING_TABLE, SLICE_TABLE] {
            let bad:bool=self.connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE typeof(operation_id)!='text' OR length(operation_id)!=36)"),[],|r|r.get(0))?;
            if bad {
                return Err(error("restart operation identity bound"));
            }
            let mut after = String::new();
            loop {
                // Composite private indexes order these keys; no UNION materializes the relations.
                let mut statement=self.connection.prepare(&format!("SELECT DISTINCT operation_id FROM {table} WHERE operation_id>?1 ORDER BY operation_id LIMIT ?2"))?;
                let page = statement
                    .query_map(params![after, READ_PAGE_ROWS as i64], |r| {
                        scalar_projection::uuid(r, 0)
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                if page.is_empty() {
                    break;
                }
                for text in page {
                    let text = text.ok_or_else(|| error("restart operation identity bound"))?;
                    let id = text.id.map_err(error)?;
                    if id.is_nil() || !text.canonical {
                        return Err(error("restart noncanonical operation identity"));
                    }
                    let operation = OperationId::from_uuid(id);
                    if input.is_some() {
                        let inventory = read_with_input(&self.connection, operation, input)?
                            .ok_or_else(|| error("missing restart inventory"))?;
                        self.validate_database_claim_operation_with_input(
                            operation, &inventory, input,
                        )?;
                    } else if self.recording_prestart_scope.get() {
                        // Finish the two-tree operation decoder before owning the binary
                        // inventory. Subsequent comparisons reuse this exact decoded row;
                        // command digest construction owns only one JSON tree at a time.
                        let record = self
                            .read_row(operation)?
                            .ok_or_else(|| error("orphan inventory"))?
                            .record;
                        let inventory = read_with_input(&self.connection, operation, input)?
                            .ok_or_else(|| error("missing restart inventory"))?;
                        self.validate_database_claim_record(&record, &inventory)?;
                    } else {
                        let inventory = read_with_input(&self.connection, operation, input)?
                            .ok_or_else(|| error("missing restart inventory"))?;
                        self.validate_database_claim_operation(operation, &inventory)?;
                    }
                    after = id.to_string();
                }
            }
        }
        Ok(())
    }
}
