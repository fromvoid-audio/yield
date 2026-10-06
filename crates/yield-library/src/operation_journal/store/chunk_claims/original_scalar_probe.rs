// Exact pre-OPT2399 decoder, diagnostic-only; no effect/claim API.
use super::*;
pub(super) fn original_decode_row(row: &Row<'_>) -> Result<ChunkClaimRecord, ChunkClaimReadError> {
    let chunk_id = ChunkId::parse(row.get::<_, String>(0)?.as_str())
        .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let operation_id = OperationId::parse(row.get::<_, String>(1)?.as_str())
        .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let claim_id = AdmissionClaimId::from_uuid(
        Uuid::parse_str(row.get::<_, String>(2)?.as_str())
            .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?,
    )
    .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let participant_id = ParticipantId::parse(row.get::<_, String>(3)?.as_str())
        .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))?;
    let output_key: String = row.get(4)?;
    if output_key.is_empty() || output_key.len() > MAX_OUTPUT_KEY_BYTES {
        return Err(ChunkClaimReadError::Malformed(String::from(
            "output key bounds",
        )));
    }
    let logical_offset = original_positive_u64(row.get::<_, i64>(5)?, "logical offset")?;
    let logical_length = original_positive_u64(row.get::<_, i64>(6)?, "logical length")?;
    let volume_device = row
        .get::<_, String>(7)?
        .parse::<u64>()
        .map_err(|_| ChunkClaimReadError::Malformed(String::from("volume device bounds")))?;
    let volume = VolumeIdentity {
        device: volume_device as u64,
    };
    let allocation_class = original_allocation_class(row.get::<_, String>(8)?.as_str())?;
    if !supports_chunk_output_class(allocation_class) {
        return Err(ChunkClaimReadError::Malformed(String::from(
            "chunk output allocation class is not incrementally chargeable",
        )));
    }
    let allocated_bytes = original_positive_u64(row.get::<_, i64>(9)?, "allocated bytes")?;
    let allocation_unit = original_positive_u64(row.get::<_, i64>(10)?, "allocation unit")?;
    let protected_free_bytes = original_positive_u64(row.get::<_, i64>(11)?, "protected floor")?;
    let state = match row.get::<_, String>(12)?.as_str() {
        "Claimed" => ChunkClaimState::Claimed,
        "Issued" => ChunkClaimState::Issued,
        "Released" => ChunkClaimState::Released,
        value => {
            return Err(ChunkClaimReadError::Malformed(format!(
                "unknown state {value}"
            )));
        }
    };
    let issue_id = original_optional_uuid(row.get(13)?)?;
    let release_id = original_optional_uuid(row.get(14)?)?;
    if (state == ChunkClaimState::Claimed && (issue_id.is_some() || release_id.is_some()))
        || (state == ChunkClaimState::Issued && (issue_id.is_none() || release_id.is_some()))
        || (state == ChunkClaimState::Released && release_id.is_none())
    {
        return Err(ChunkClaimReadError::Malformed(String::from(
            "state transition identities",
        )));
    }
    Ok(ChunkClaimRecord {
        chunk_id,
        operation_id,
        claim_id,
        participant_id,
        output_key,
        logical_offset,
        logical_length,
        volume,
        allocation_class,
        allocated_bytes,
        allocation_unit,
        protected_free_bytes,
        state,
        issue_id,
        release_id,
    })
}

fn original_positive_u64(value: i64, field: &str) -> Result<u64, ChunkClaimReadError> {
    if value < 0 || (field != "logical offset" && value == 0) {
        return Err(ChunkClaimReadError::Malformed(format!("{field} bounds")));
    }
    Ok(value as u64)
}

fn original_optional_uuid(value: Option<String>) -> Result<Option<Uuid>, ChunkClaimReadError> {
    value
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|error| ChunkClaimReadError::Malformed(error.to_string()))
        })
        .transpose()
}

fn original_allocation_class(
    value: &str,
) -> Result<FullCapacityAllocationClass, ChunkClaimReadError> {
    match value {
        "DestinationStaging" => Ok(FullCapacityAllocationClass::DestinationStaging),
        "JournalRecord" => Ok(FullCapacityAllocationClass::JournalRecord),
        "SourceDatabaseCommit" => Ok(FullCapacityAllocationClass::SourceDatabaseCommit),
        "SourceWalShm" => Ok(FullCapacityAllocationClass::SourceWalShm),
        "GlobalDatabaseCommit" => Ok(FullCapacityAllocationClass::GlobalDatabaseCommit),
        "GlobalWalShm" => Ok(FullCapacityAllocationClass::GlobalWalShm),
        "ExistingDestination" => Ok(FullCapacityAllocationClass::ExistingDestination),
        "ExistingRecoveryPayload" => Ok(FullCapacityAllocationClass::ExistingRecoveryPayload),
        "OperationRecoveryPayload" => Ok(FullCapacityAllocationClass::OperationRecoveryPayload),
        value => Err(ChunkClaimReadError::Malformed(format!(
            "unknown allocation class {value}"
        ))),
    }
}
