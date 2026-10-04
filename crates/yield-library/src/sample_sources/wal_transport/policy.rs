//! Closed genuine family inputs/native custody; no caller-defined policy or decision issuer.
use super::*;
use crate::sample_sources::db::write::{
    SourceLayoutMetadataBudget, SourceLayoutWalPreparation, SourceLayoutWalPreparationFailure,
    SourceTagAdmissionObservation, SourceTagWalPreparation, SourceTagWalPreparationFailure,
};

/// Historical family of an actually entered original source policy, never admission authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceWalPolicyFamily {
    /// Original whole-source tag allocation and assignment scope.
    Tag,
    /// Original bounded source-derived layout allocation scope.
    Layout,
}

pub(super) enum RequestPolicy {
    Tag(Box<SourceTagAdmissionObservation>),
    Layout(Box<SourceLayoutMetadataBudget>),
}
impl RequestPolicy {
    pub(super) fn input_bytes(&self) -> Option<usize> {
        match self {
            Self::Tag(observation) => observation.retained_input_bytes(),
            Self::Layout(budget) => budget.retained_input_bytes(),
        }
    }
}

pub(super) enum Native {
    // Credits unavailable before any preparation: retain the unchanged original owner/input.
    // This variant has no empty-reservation retirement authority.
    Unprepared(SourceWriter, Option<RequestPolicy>),
    Held(SourceWalPreparation),
    Failed(SourceWalPreparationFailure),
    HeldTag(SourceTagWalPreparation),
    FailedTag(SourceTagWalPreparationFailure),
    HeldLayout(SourceLayoutWalPreparation),
    FailedLayout(SourceLayoutWalPreparationFailure),
}
impl Native {
    pub(super) fn acquire(
        writer: SourceWriter,
        expected: &SourceRevision,
        deadline: Instant,
        policy: Option<RequestPolicy>,
    ) -> Self {
        match policy {
            None => match writer.prepare_wal_reservation(expected, deadline) {
                Ok(held) => Self::Held(held),
                Err(failed) => Self::Failed(failed),
            },
            Some(RequestPolicy::Tag(observation)) => {
                match writer.prepare_tag_wal_reservation(*observation, deadline) {
                    Ok(held) => Self::HeldTag(held),
                    Err(failed) => Self::FailedTag(failed),
                }
            }
            Some(RequestPolicy::Layout(budget)) => {
                match writer.prepare_layout_wal_reservation(*budget, deadline) {
                    Ok(held) => Self::HeldLayout(held),
                    Err(failed) => Self::FailedLayout(failed),
                }
            }
        }
    }
    pub(super) fn is_prepared(&self) -> bool {
        matches!(self, Self::Held(_) | Self::HeldTag(_) | Self::HeldLayout(_))
    }
    pub(super) fn error(&self) -> Option<&SourceWalPreparationError> {
        match self {
            Self::Failed(f) => Some(f.error()),
            Self::FailedTag(f) => Some(f.error()),
            Self::FailedLayout(f) => Some(f.error()),
            _ => None,
        }
    }
    // Policy validation reads only entered knobs and current CONTROL/native ownership.
    // Full family rows were validated inside the original exclusive empty transaction.
    pub(super) fn held(&self) -> Result<&SourceWalPreparation, SourceWalPreparationError> {
        match self {
            Self::Held(held) => Ok(held),
            Self::HeldTag(held) => held.original_wal_preparation(),
            Self::HeldLayout(held) => held.original_wal_preparation(),
            _ => Err(SourceWalPreparationError::RollbackUnconfirmed),
        }
    }
    pub(super) fn family(&self) -> Option<SourceWalPolicyFamily> {
        match self {
            Self::HeldTag(_) => Some(SourceWalPolicyFamily::Tag),
            Self::HeldLayout(_) => Some(SourceWalPolicyFamily::Layout),
            _ => None,
        }
    }
    pub(super) fn database_identity(&self) -> Option<&str> {
        match self {
            Self::Unprepared(_, _) => None,
            Self::Held(held) => Some(held.revision().database_identity()),
            Self::HeldTag(held) => Some(held.revision().database_identity()),
            Self::HeldLayout(held) => Some(held.revision().database_identity()),
            Self::Failed(f) => f.retained_database_identity(),
            Self::FailedTag(f) => f.retained_database_identity(),
            Self::FailedLayout(f) => f.retained_database_identity(),
        }
    }
    pub(super) fn into_writer(self) -> Result<SourceWriter, Self> {
        match self {
            Self::Unprepared(writer, policy) => {
                if !writer.lease_transport_unprepared_is_reusable() {
                    return Err(Self::Unprepared(writer, policy));
                }
                // Actual policy/selection backing drops before owner slot credit is refunded.
                drop(policy);
                Ok(writer)
            }
            Self::Held(held) => held.abort().map_err(Self::Failed),
            Self::Failed(failed) => failed.into_writer().map_err(Self::Failed),
            Self::HeldTag(held) => held.abort().map_err(Self::FailedTag),
            Self::FailedTag(failed) => failed.into_writer().map_err(Self::FailedTag),
            Self::HeldLayout(held) => held.abort().map_err(Self::FailedLayout),
            Self::FailedLayout(failed) => failed.into_writer().map_err(Self::FailedLayout),
        }
    }
    pub(super) fn close_ready(&self) -> bool {
        match self {
            Self::Failed(f) => f.source_authority_close_ready(),
            Self::FailedTag(f) => f.source_authority_close_ready(),
            Self::FailedLayout(f) => f.source_authority_close_ready(),
            _ => false,
        }
    }
    pub(super) fn close(
        self,
        authority: &mut SourceAuthorityRegistration,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        match self {
            Self::Failed(f) => f.close_for_retirement(authority),
            Self::FailedTag(f) => f.close_for_retirement(authority),
            Self::FailedLayout(f) => f.close_for_retirement(authority),
            _ => Err(SourceAuthorityError::InvalidLifecycle),
        }
    }
}
