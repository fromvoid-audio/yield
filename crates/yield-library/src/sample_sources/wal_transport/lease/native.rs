//! Closed original-owner custody, without a public backend or outcome setter.
use super::*;
pub(super) enum LeaseNative {
    Held(SourceLeaseMaintenancePreparation),
    Failed(SourceLeaseMaintenancePreparationFailure),
    Unprepared(
        SourceWriter,
        SourceLeaseMaintenanceAllocation,
        SourceWalTransportRejection,
    ),
}
impl LeaseNative {
    pub(super) fn cleanup_staging_bytes(&self) -> usize {
        match self {
            Self::Unprepared(..) => 0,
            _ => 3 * MAX_SOURCE_LEASE_SCOPE_BYTES + 512,
        }
    }
    pub(super) fn close_without_reuse(self) {
        match self {
            Self::Held(held) => held.close_without_reuse(),
            Self::Failed(failed) => failed.close_without_reuse(),
            Self::Unprepared(writer, input, _) => {
                drop(writer);
                drop(input);
            }
        }
    }
    pub(super) fn acquire(
        writer: SourceWriter,
        input: SourceLeaseMaintenanceAllocation,
        deadline: Instant,
        #[cfg(feature = "test-support")] unknown_wal: bool,
    ) -> Self {
        #[cfg(feature = "test-support")]
        if unknown_wal {
            return match writer.prepare_lease_maintenance_unknown_for_tests(input, deadline) {
                Ok(held) => Self::Held(held),
                Err(failed) => Self::Failed(failed),
            };
        }
        match writer.prepare_lease_maintenance(input, deadline) {
            Ok(held) => Self::Held(held),
            Err(failed) => Self::Failed(failed),
        }
    }
    pub(super) fn rejection(&self) -> Option<SourceWalTransportRejection> {
        match self {
            Self::Held(_) => None,
            Self::Failed(f) => Some(classify(f.error())),
            Self::Unprepared(_, _, r) => Some(*r),
        }
    }
    pub(super) fn held(
        &self,
    ) -> Result<&SourceLeaseMaintenancePreparation, SourceWalTransportRejection> {
        match self {
            Self::Held(h) => Ok(h),
            _ => Err(self
                .rejection()
                .unwrap_or(SourceWalTransportRejection::OwnerLost)),
        }
    }
    pub(super) fn database_identity(&self) -> Option<&str> {
        match self {
            Self::Held(h) => Some(h.observation().subject().database_identity()),
            Self::Failed(f) => f.retained_database_identity(),
            Self::Unprepared(_, input, _) => Some(input.subject().database_identity()),
        }
    }
    pub(super) fn into_writer(self) -> Result<SourceWriter, Self> {
        match self {
            Self::Held(h) => h.abort().map_err(Self::Failed),
            Self::Failed(f) => f.into_writer().map_err(Self::Failed),
            Self::Unprepared(writer, input, reason) => {
                if !writer.lease_transport_unprepared_is_reusable() {
                    return Err(Self::Unprepared(writer, input, reason));
                }
                drop(input);
                Ok(writer)
            }
        }
    }
    pub(super) fn close_ready(&self) -> bool {
        match self {
            Self::Failed(f) => f.source_authority_close_ready(),
            _ => false,
        }
    }
    pub(super) fn close(
        self,
        authority: &mut SourceAuthorityRegistration,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        match self {
            Self::Failed(f) => f.close_for_retirement(authority),
            _ => Err(SourceAuthorityError::InvalidLifecycle),
        }
    }
}
