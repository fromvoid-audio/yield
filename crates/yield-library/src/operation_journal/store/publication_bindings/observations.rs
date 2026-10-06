//! Bounded authority-free command observations; no live roots, objects or grants.
use super::super::staging_checkpoints::{StoredRootRef, StoredStagingSourceFence};
use crate::operation_journal::{OperationId, ParticipantId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// These are explicitly data-only mirrors of the authority-free command wire shape.
// Unlike file-owner roots/objects, none has an API returning a live registration or grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileObservation {
    pub(super) owner: Uuid,
    pub(super) generation: u64,
    pub(super) lease_epoch: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) enum AuthorityObservation {
    Source(StoredStagingSourceFence),
    Profile(ProfileObservation),
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum ExpectedObservation {
    Absent,
    Exact(Uuid),
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RootObservation {
    pub(super) registration: StoredRootRef,
    pub(super) authority: AuthorityObservation,
    pub(super) locator: String,
    pub(super) expected: ExpectedObservation,
}
impl RootObservation {
    pub(super) fn validate(&self) -> Result<(), String> {
        self.registration.validate()?;
        if self.locator.is_empty()
            || self.locator.len() > 1024
            || self.locator.contains('\0')
            || self.locator.contains('/')
            || self.locator.contains('\\')
            || matches!(self.locator.as_str(), "." | "..")
        {
            return Err("publication root locator is invalid".into());
        }
        match &self.authority {
            AuthorityObservation::Source(source) => source.validate()?,
            AuthorityObservation::Profile(profile) => {
                if profile.owner.is_nil() || profile.generation == 0 || profile.lease_epoch == 0 {
                    return Err("publication profile observation is invalid".into());
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RootRoleObservation {
    pub(super) binding: RootObservation,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CopyObservation {
    pub(super) object: Uuid,
    pub(super) bytes: u64,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AbsenceObservation {
    pub(super) marker: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) enum PublicationObservation {
    PublishNoReplace {
        staging: RootRoleObservation,
        target: RootRoleObservation,
        staged: CopyObservation,
        expected_absent: AbsenceObservation,
    },
}

fn one_source<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> Result<Vec<StoredStagingSourceFence>, D::Error> {
    struct One;
    impl<'de> serde::de::Visitor<'de> for One {
        type Value = Vec<StoredStagingSourceFence>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most one original source fence")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let first = seq.next_element::<StoredStagingSourceFence>()?;
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "multiple publication source fences",
                ));
            }
            Ok(first.into_iter().collect())
        }
    }
    decoder.deserialize_seq(One)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandObservation {
    pub(super) operation_id: OperationId,
    pub(super) participant_id: ParticipantId,
    pub(super) effect_id: Uuid,
    #[serde(deserialize_with = "one_source")]
    pub(super) source_fences: Vec<StoredStagingSourceFence>,
    pub(super) target_registration: StoredRootRef,
    pub(super) expected: ExpectedObservation,
    pub(super) effect: PublicationObservation,
}
