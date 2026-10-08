//! Prepaid metadata for the original post-intent claim write, not admission authority.
use super::*;
use crate::operation_journal::{AdmissionClaim, owner::InputPermit};
use std::mem::size_of;

struct OperationImage {
    record: StoredOperationRecord,
    sequence: i64,
    digest: [u8; 32],
    _credit: super::super::owner::StartupCredit,
}

pub(in crate::operation_journal) struct ChunkWriteScope<'a> {
    image: OperationImage,
    operation: OperationId,
    request: chunk_claims::ChunkClaimRequest,
    input: &'a InputPermit,
    working: Option<InputPermit>,
    output: Option<InputPermit>,
    duplicate: Option<InputPermit>,
    selected: bool,
}
impl<'a> ChunkWriteScope<'a> {
    fn new(
        store: &OperationJournalStore,
        request: &chunk_claims::ChunkClaimRequest,
        image: OperationImage,
        input: &'a InputPermit,
    ) -> Result<Self, OperationJournalStoreError> {
        // Exact one-row claim/plan construction and validators. Count original
        // operation graphs while borrowed; no record or decoded authority escapes.
        let node = 12 * size_of::<((VolumeIdentity, FullCapacityAllocationClass), u64)>()
            + 16 * size_of::<usize>();
        let bytes = store
            .database_path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_mul(17)
            .and_then(|p| {
                p.checked_add(
                    4096 + 8 * node
                        + 4 * size_of::<AdmissionClaim>()
                        + 2 * size_of::<FullCapacityPlan>()
                        + 2 * size_of::<AdmissionClaimInventory>()
                        + 2 * size_of::<super::super::owner::OperationJournalOwnerError>()
                        + 2 * size_of::<chunk_claims::FundedChunkClaimRecord>(),
                )
            })
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let bytes = bytes
            .checked_add(store.namespace_prepaid_bytes()?)
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let bytes = bytes
            .checked_add(request.output_key().len())
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let working = input.reserve_historical_scratch(bytes)?;
        let output = input.reserve_historical_scratch(chunk_claims::point_read_bytes())?;
        let duplicate = input.reserve_historical_scratch(chunk_claims::point_read_bytes())?;
        Ok(Self {
            image,
            operation: request.operation_id(),
            request: request.clone(),
            input,
            working: Some(working),
            output: Some(output),
            duplicate: Some(duplicate),
            selected: true,
        })
    }
    pub(in crate::operation_journal) fn retain_observation_credit(&mut self, credit: InputPermit) {
        self.working
            .as_mut()
            .unwrap()
            .absorb_historical_scratch(credit);
    }
    pub(super) fn record(&self) -> &StoredOperationRecord {
        &self.image.record
    }
    pub(super) fn input(&self) -> &'a InputPermit {
        self.input
    }
    pub(super) fn verify_image(
        &self,
        store: &OperationJournalStore,
    ) -> Result<(), OperationJournalStoreError> {
        initial_settlement_backing::statement(
            &store.connection,
            self.input,
            c"SELECT sequence, record FROM operation_journal WHERE operation_id = ?1",
            |row| {
                row.bind_text(1, &self.operation.to_string())?;
                if !row.next()? {
                    return Err(OperationJournalStoreError::OperationNotFound {
                        operation_id: self.operation,
                    });
                }
                let ValueRef::Blob(bytes) = row.get_ref(1)? else {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: "operation record changed SQLite type".into(),
                    });
                };
                if row.get_ref(0)? != ValueRef::Integer(self.image.sequence)
                    || blake3::hash(bytes).as_bytes() != &self.image.digest
                {
                    return Err(OperationJournalStoreError::ChunkClaim {
                        reason: "operation changed after preparing chunk write".into(),
                    });
                }
                Ok(())
            },
        )
    }
    pub(super) fn matches(&self, request: &chunk_claims::ChunkClaimRequest) -> bool {
        &self.request == request
    }
    pub(super) fn validate_namespace(
        &self,
        store: &OperationJournalStore,
        precommit: bool,
    ) -> Result<(), OperationJournalStoreError> {
        store.validate_namespace_prepaid(self.input, self.working.as_ref().unwrap(), precommit)
    }
    pub(super) fn take_duplicate(&mut self) -> InputPermit {
        self.duplicate.take().unwrap()
    }
    pub(super) fn take_output(&mut self) -> InputPermit {
        self.output.take().unwrap()
    }
    pub(super) fn finish(mut self, selected: bool) {
        self.selected = selected;
    }
}
impl Drop for ChunkWriteScope<'_> {
    fn drop(&mut self) {
        if self.selected {
            self.input.retain_historical_error(
                self.working.take().unwrap(),
                self.output.take(),
                self.duplicate.take(),
                None,
            );
        }
    }
}
impl OperationJournalStore {
    pub(in crate::operation_journal) fn prepay_chunk_write<'a>(
        &self,
        request: &chunk_claims::ChunkClaimRequest,
        input: &'a InputPermit,
    ) -> Result<ChunkWriteScope<'a>, OperationJournalStoreError> {
        self.validate_namespace_with_input(Some(input), false)?;
        let mut scope = None;
        let present = self.inspect_bound_complete_claim_operation(
            request.operation_id(),
            input,
            |record, sequence, digest| {
                // Only a prepaid private owned observation escapes. Neither its
                // decoded record nor its loan can be extracted or cloned by callers.
                let mut credit = input.reserve_historical_scratch(
                    database_claims::command_backing::shape_working_bytes(),
                )?;
                let cloned = database_claims::command_backing::settlement_shape(record)?.cloned;
                credit.absorb_historical_scratch(input.reserve_historical_scratch(cloned)?);
                let image = OperationImage {
                    record: record.clone(),
                    sequence,
                    digest,
                    _credit: credit.into_startup_credit(),
                };
                scope = Some(ChunkWriteScope::new(self, request, image, input)?);
                Ok(())
            },
        )?;
        if !present {
            return Err(OperationJournalStoreError::OperationNotFound {
                operation_id: request.operation_id(),
            });
        }
        Ok(scope.expect("present complete original operation produced its write budget"))
    }
}
