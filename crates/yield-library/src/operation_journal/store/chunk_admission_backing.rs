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
    request: Option<chunk_claims::ChunkClaimRequest>,
    chunk: chunk_claims::ChunkId,
    input: &'a InputPermit,
    working: Option<InputPermit>,
    output: Option<InputPermit>,
    duplicate: Option<InputPermit>,
    transition: Option<InputPermit>,
    selected: bool,
}
impl<'a> ChunkWriteScope<'a> {
    fn new(
        store: &OperationJournalStore,
        request: Option<&chunk_claims::ChunkClaimRequest>,
        operation: OperationId,
        chunk: chunk_claims::ChunkId,
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
            .checked_add(
                request.map_or(chunk_claims::MAX_OUTPUT_KEY_BYTES, |r| r.output_key().len()),
            )
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        let working = input.reserve_historical_scratch(bytes)?;
        let output = input.reserve_historical_scratch(chunk_claims::point_read_bytes())?;
        let duplicate = input.reserve_historical_scratch(chunk_claims::point_read_bytes())?;
        let transition = request
            .is_none()
            .then(|| input.reserve_historical_scratch(chunk_claims::point_read_bytes()))
            .transpose()?;
        Ok(Self {
            image,
            operation,
            request: request.cloned(),
            chunk,
            input,
            working: Some(working),
            output: Some(output),
            duplicate: Some(duplicate),
            transition,
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
        self.request.as_ref() == Some(request)
    }
    pub(super) fn matches_issue(
        &self,
        chunk: chunk_claims::ChunkId,
        operation: OperationId,
    ) -> bool {
        self.request.is_none() && self.chunk == chunk && self.operation == operation
    }
    pub(super) fn take_transition(&mut self) -> InputPermit {
        self.transition.take().unwrap()
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
                self.transition.take(),
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
        self.prepay_chunk_observation(
            Some(request),
            request.operation_id(),
            request.chunk_id(),
            input,
        )
    }
    pub(in crate::operation_journal) fn prepay_chunk_issuance<'a>(
        &self,
        operation: OperationId,
        chunk: chunk_claims::ChunkId,
        input: &'a InputPermit,
    ) -> Result<ChunkWriteScope<'a>, OperationJournalStoreError> {
        self.prepay_chunk_observation(None, operation, chunk, input)
    }
    fn prepay_chunk_observation<'a>(
        &self,
        request: Option<&chunk_claims::ChunkClaimRequest>,
        operation: OperationId,
        chunk: chunk_claims::ChunkId,
        input: &'a InputPermit,
    ) -> Result<ChunkWriteScope<'a>, OperationJournalStoreError> {
        self.validate_namespace_with_input(Some(input), false)?;
        let mut scope = None;
        let present = self.inspect_bound_complete_claim_operation(
            operation,
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
                scope = Some(ChunkWriteScope::new(
                    self, request, operation, chunk, image, input,
                )?);
                Ok(())
            },
        )?;
        if !present {
            return Err(OperationJournalStoreError::OperationNotFound {
                operation_id: operation,
            });
        }
        Ok(scope.expect("present complete original operation produced its write budget"))
    }
}
