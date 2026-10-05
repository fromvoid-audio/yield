//! Original-owner legacy preparation. Frozen wire data never reacquires custody or grants writes.
use super::{ParticipantId, ParticipantKind, ParticipantRequirement, ParticipantSet};
use crate::sample_sources::db::{LegacyCommonAllocationObservation, SourceCommandId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

/// Independent passive request version, not a source/journal schema version.
pub const LEGACY_ADOPTION_INPUT_VERSION: u64 = 1;
/// Binary cap ensuring canonical base64 plus its JSON quotes fits in 32KiB. The other half
/// of the combined 64KiB source-local request/receipt ceiling remains available to the receipt.
pub const MAX_LEGACY_ADOPTION_INPUT_BYTES: usize = ((32 * 1024 - 2) / 4) * 3;
const MAGIC: &[u8; 8] = b"YLADOPT\0";

/// Invalid, stale, cancelled or unsupported original-owner input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LegacyAdoptionInputError {
    /// Actual original retained owner observation could not be obtained.
    #[error("legacy adoption original observation unavailable")]
    Observation,
    /// Preparation was cancelled before any durable identity or writes.
    #[error("legacy adoption preparation cancelled")]
    Cancelled,
    /// Original cooperative observation deadline elapsed.
    #[error("legacy adoption preparation deadline elapsed")]
    Deadline,
    /// Identity, policy, object or topology data is inconsistent.
    #[error("invalid legacy adoption historical data")]
    Invalid,
    /// Input is corrupt, noncanonical or exceeds the finite byte cap.
    #[error("invalid legacy adoption bounded wire input")]
    Wire,
}

/// Passive frozen original-owner command, with no current lease, audit, claim or effect capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyAdoptionIntentArgs {
    data: Data,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Data {
    participant: ParticipantId,
    command: SourceCommandId,
    profile_owner: String,
    profile_root: String,
    media_root: String,
    source_directory: String,
    journal_directory: String,
    legacy_version: u64,
    target_version: u64,
    journal_schema: u64,
    sqlite_version: u64,
    revision: u64,
    metadata_rows: u64,
    audio_rows: u64,
    image_digest: [u8; 32],
    source_policy: Policy,
    journal_policy: Policy,
    objects: [Object; 8],
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    page_size: u64,
    page_count: u64,
    existing_max_pages: u64,
    selected_max_pages: u64,
    sector: u64,
    observed_cache_spill: u64,
    observed_temp_store: u64,
}
// Fixed array order is the explicit role encoding: source main/rollback/future WAL/future SHM/
// reservation, journal main/rollback, profile lock. Do not relabel objects as other capacity classes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Object {
    identity: String,
    device: u64,
    unit: u64,
    observed: u64,
    conditional_logical_bytes: u64,
}
impl LegacyAdoptionIntentArgs {
    pub(crate) fn legacy_metadata_bytes(&self) -> Option<usize> {
        let d = &self.data;
        let text = [
            &d.profile_owner,
            &d.profile_root,
            &d.media_root,
            &d.source_directory,
            &d.journal_directory,
        ];
        if text.iter().any(|s| s.capacity() > 1024)
            || d.objects.iter().any(|o| o.identity.capacity() > 1024)
        {
            return None;
        }
        Some(
            std::mem::size_of::<Self>()
                + text.iter().map(|s| s.capacity()).sum::<usize>()
                + d.objects
                    .iter()
                    .map(|o| o.identity.capacity())
                    .sum::<usize>(),
        )
    }
    pub(crate) fn from_original(
        observation: &LegacyCommonAllocationObservation,
        participant: ParticipantId,
        command: SourceCommandId,
    ) -> Result<Self, LegacyAdoptionInputError> {
        let s = observation.source();
        let j = observation.journal();
        let image = s.image();
        let value = Self {
            data: Data {
                participant,
                command,
                profile_owner: j.owner_identity().to_owned(),
                profile_root: j.profile_identity().to_owned(),
                media_root: observation.media_root_identity().to_owned(),
                source_directory: observation.source_directory_identity().to_owned(),
                journal_directory: j.directory_identity().to_owned(),
                legacy_version: 11,
                target_version: 13,
                journal_schema: u64::from(super::store::JOURNAL_SCHEMA_VERSION),
                sqlite_version: rusqlite::version_number() as u64,
                revision: image.revision(),
                metadata_rows: image.metadata_rows() as u64,
                audio_rows: image.audio_rows() as u64,
                image_digest: *image.digest(),
                source_policy: Policy {
                    page_size: s.page_size(),
                    page_count: s.page_count(),
                    existing_max_pages: s.existing_max_pages(),
                    selected_max_pages: s.selected_max_pages(),
                    sector: s.sector_size(),
                    observed_cache_spill: 0,
                    observed_temp_store: 2,
                },
                journal_policy: Policy {
                    page_size: j.page_size(),
                    page_count: j.page_count(),
                    existing_max_pages: j.current_max_pages(),
                    selected_max_pages: j.selected_max_pages(),
                    sector: j.sector_size(),
                    observed_cache_spill: j.current_cache_spill(),
                    observed_temp_store: j.current_temp_store(),
                },
                objects: std::array::from_fn(|i| {
                    let o = &observation.objects()[i];
                    Object {
                        identity: o.object_identity().to_owned(),
                        device: o.volume().device,
                        unit: o.allocation_unit(),
                        observed: o.observed_bytes(),
                        conditional_logical_bytes: o.proposed_logical_bytes(),
                    }
                }),
            },
        };
        value.validate()?;
        value.canonical_bytes()?;
        Ok(value)
    }
    /// Exact historical participant correlation; no live participant is reacquired.
    pub fn participant_id(&self) -> ParticipantId {
        self.data.participant
    }
    /// Exact frozen source-command correlation; no OperationId is allocated here.
    pub fn source_command_id(&self) -> SourceCommandId {
        self.data.command
    }
    /// Original combined schema/row/physical/profile digest, not a current source revision.
    pub fn original_image_digest(&self) -> &[u8; 32] {
        &self.data.image_digest
    }
    // Descriptor-only comparison before SQLite can inspect or recover a changed rollback file.
    pub(crate) fn matches_journal_rollback(&self, bytes: u64, identity: Option<&str>) -> bool {
        let original = &self.data.objects[6];
        original.observed == bytes
            && match identity {
                Some(identity) => original.identity == identity,
                None => {
                    original.identity
                        == format!(
                            "directory:{}/{}-journal",
                            self.data.journal_directory,
                            super::store::JOURNAL_DATABASE_FILE_NAME
                        )
                }
            }
    }
    /// Original legacy semantic revision, never an audited current SourceRevision.
    pub fn original_revision(&self) -> u64 {
        self.data.revision
    }
    /// Actual historical original main-file identity.
    pub fn source_database_identity(&self) -> &str {
        &self.data.objects[0].identity
    }
    /// Actual historical original source reservation descriptor identity.
    pub fn source_reservation_identity(&self) -> &str {
        &self.data.objects[4].identity
    }
    /// Actual historical original profile acquisition.
    pub fn profile_owner_identity(&self) -> &str {
        &self.data.profile_owner
    }
    /// Checked historical logical totals in stable actual device order; never physical charges.
    pub fn proposed_volume_totals(&self) -> Result<Vec<(u64, u64, u64)>, LegacyAdoptionInputError> {
        self.validate()?;
        self.totals()
    }
    fn totals(&self) -> Result<Vec<(u64, u64, u64)>, LegacyAdoptionInputError> {
        let mut totals = BTreeMap::<u64, (u64, u64)>::new();
        for o in &self.data.objects {
            let (unit, n) = totals.entry(o.device).or_insert((o.unit, 0));
            if *unit != o.unit {
                return Err(LegacyAdoptionInputError::Invalid);
            }
            *n = n
                .checked_add(o.conditional_logical_bytes)
                .ok_or(LegacyAdoptionInputError::Invalid)?;
        }
        Ok(totals.into_iter().map(|(d, (u, n))| (d, u, n)).collect())
    }
    /// Validate only bounded historical shape/arithmetic; this cannot establish current ownership.
    pub fn validate(&self) -> Result<(), LegacyAdoptionInputError> {
        use LegacyAdoptionInputError::Invalid as E;
        let d = &self.data;
        let text =
            |s: &str| !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control);
        if d.participant.as_uuid().is_nil()
            || d.command.as_uuid().is_nil()
            || [
                &d.profile_owner,
                &d.profile_root,
                &d.media_root,
                &d.source_directory,
                &d.journal_directory,
            ]
            .iter()
            .any(|s| !text(s))
            || d.target_version != 13
            || d.journal_schema != 12
            || d.sqlite_version != 3_050_002
            || d.legacy_version != 11
            || d.revision > i64::MAX as u64
            || d.metadata_rows == 0
            || d.metadata_rows > 128
            || d.audio_rows > 1024
            || d.image_digest == [0; 32]
        {
            return Err(E);
        }
        let source = policy_bounds(&d.source_policy, 64 * 1024 * 1024)?;
        let journal = policy_bounds(&d.journal_policy, 512 * 1024 * 1024)?;
        if d.source_policy.observed_cache_spill != 0
            || d.source_policy.observed_temp_store != 2
            || d.objects[0].observed == 0
            || d.objects[0].observed > 16 * 1024 * 1024
            || d.objects[5].observed == 0
            || d.objects[0].observed
                < d.source_policy
                    .page_size
                    .checked_mul(d.source_policy.page_count)
                    .ok_or(E)?
            || d.objects[5].observed
                < d.journal_policy
                    .page_size
                    .checked_mul(d.journal_policy.page_count)
                    .ok_or(E)?
        {
            return Err(E);
        }
        let expected = [
            source[0], source[1], source[2], source[3], 4096, journal[0], journal[1], 4096,
        ];
        let mut identities = BTreeSet::new();
        for (i, o) in d.objects.iter().enumerate() {
            if !text(&o.identity)
                || !identities.insert(&o.identity)
                || o.unit == 0
                || !o.unit.is_power_of_two()
                || o.conditional_logical_bytes != expected[i]
                || o.observed > expected[i]
                || ((1..=4).contains(&i) && o.observed != 0)
            {
                return Err(E);
            }
        }
        // Positively absent future sidecars are bound to the original direct-child namespace.
        for (i, suffix) in [(1, "-journal"), (2, "-wal"), (3, "-shm")] {
            if d.objects[i].identity
                != format!(
                    "directory:{}/{}{}",
                    d.source_directory,
                    crate::sample_sources::DB_FILE_NAME,
                    suffix
                )
                || d.objects[i].device != d.objects[1].device
                || d.objects[i].unit != d.objects[1].unit
            {
                return Err(E);
            }
        }
        if d.objects[6].identity.starts_with("directory:")
            && (d.objects[6].observed != 0
                || d.objects[6].identity
                    != format!(
                        "directory:{}/{}-journal",
                        d.journal_directory,
                        super::store::JOURNAL_DATABASE_FILE_NAME
                    ))
        {
            return Err(E);
        }
        self.totals()?;
        Ok(())
    }
    /// Derive the one required pre-lease participant, without fabricating a current source fence.
    pub fn participant_observation(&self) -> Result<ParticipantSet, LegacyAdoptionInputError> {
        self.validate()?;
        let d = super::ParticipantDescriptor::new(
            self.participant_id(),
            ParticipantKind::LegacySource,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .map_err(|_| LegacyAdoptionInputError::Invalid)?;
        let c = super::ParticipantCheckpoint::new(
            self.participant_id(),
            super::ParticipantCheckpointState::Pending,
            None,
            None,
        )
        .map_err(|_| LegacyAdoptionInputError::Invalid)?;
        ParticipantSet::new(vec![d], vec![c]).map_err(|_| LegacyAdoptionInputError::Invalid)
    }
    /// Exact pre-lease topology only; an ordinary leased source participant is never substituted.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), LegacyAdoptionInputError> {
        if participants != &self.participant_observation()? {
            return Err(LegacyAdoptionInputError::Invalid);
        }
        Ok(())
    }
    /// Strict bounded canonical request bytes; serialization does not preserve live ownership.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LegacyAdoptionInputError> {
        self.validate()?;
        let payload = serde_json::to_vec(&self.data).map_err(|_| LegacyAdoptionInputError::Wire)?;
        if payload.len() > MAX_LEGACY_ADOPTION_INPUT_BYTES - 16 {
            return Err(LegacyAdoptionInputError::Wire);
        }
        let mut bytes = Vec::with_capacity(16 + payload.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&LEGACY_ADOPTION_INPUT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }
}
fn policy_bounds(p: &Policy, cap: u64) -> Result<[u64; 4], LegacyAdoptionInputError> {
    use LegacyAdoptionInputError::Invalid as E;
    if !(512..=65536).contains(&p.page_size)
        || !p.page_size.is_power_of_two()
        || !(512..=65536).contains(&p.sector)
        || !p.sector.is_power_of_two()
        || p.existing_max_pages == 0
        || p.existing_max_pages > 0xffff_fffe
        || p.observed_cache_spill > i64::MAX as u64
        || p.page_count == 0
        || p.selected_max_pages == 0
        || p.selected_max_pages != p.existing_max_pages.min(cap / p.page_size)
        || p.page_count > p.selected_max_pages
        || p.observed_temp_store > 2
    {
        return Err(E);
    }
    let m = p.selected_max_pages;
    let database = m.checked_mul(p.page_size).ok_or(E)?;
    let rollback = m
        .checked_add(1)
        .and_then(|n| n.checked_mul(65536))
        .and_then(|n| {
            m.checked_mul(p.page_size + 8)
                .and_then(|pages| n.checked_add(pages))
        })
        .ok_or(E)?;
    let wal = m
        .checked_mul(p.page_size + 24)
        .and_then(|n| n.checked_add(32))
        .ok_or(E)?;
    let shm = m
        .checked_mul(16)
        .and_then(|n| n.checked_add(32768))
        .ok_or(E)?;
    Ok([database, rollback, wal, shm])
}
/// Bounded decoded historical data; neither variant is retained custody or effect permission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredLegacyAdoptionInput {
    /// Supported complete passive input, still requiring original owner admission.
    Supported(LegacyAdoptionIntentArgs),
    /// Unknown future version retained exactly without interpreting its body.
    Unsupported {
        /// Unknown wire version.
        version: u64,
        /// Exact bounded original bytes.
        bytes: Vec<u8>,
    },
}
/// Decode under the byte cap, then require canonical supported data; unknown versions stay passive.
pub fn decode_legacy_adoption_input(
    bytes: &[u8],
) -> Result<StoredLegacyAdoptionInput, LegacyAdoptionInputError> {
    use LegacyAdoptionInputError::Wire as E;
    if bytes.len() < 16 || bytes.len() > MAX_LEGACY_ADOPTION_INPUT_BYTES || &bytes[..8] != MAGIC {
        return Err(E);
    }
    let version = u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| E)?);
    if version != LEGACY_ADOPTION_INPUT_VERSION {
        return Ok(StoredLegacyAdoptionInput::Unsupported {
            version,
            bytes: bytes.to_vec(),
        });
    }
    let data = serde_json::from_slice(&bytes[16..]).map_err(|_| E)?;
    let args = LegacyAdoptionIntentArgs { data };
    if args.canonical_bytes()? != bytes {
        return Err(E);
    }
    Ok(StoredLegacyAdoptionInput::Supported(args))
}
impl Serialize for LegacyAdoptionIntentArgs {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use base64::Engine;
        let b = self.canonical_bytes().map_err(serde::ser::Error::custom)?;
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(b))
    }
}
impl<'de> Deserialize<'de> for LegacyAdoptionIntentArgs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = LegacyAdoptionIntentArgs;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded canonical base64 legacy adoption input")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                use base64::Engine;
                if s.len() > 4 * MAX_LEGACY_ADOPTION_INPUT_BYTES.div_ceil(3) {
                    return Err(E::custom("legacy adoption input exceeds transport bound"));
                }
                let b = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(E::custom)?;
                if base64::engine::general_purpose::STANDARD.encode(&b) != s {
                    return Err(E::custom("noncanonical legacy adoption transport"));
                }
                match decode_legacy_adoption_input(&b).map_err(E::custom)? {
                    StoredLegacyAdoptionInput::Supported(a) => Ok(a),
                    StoredLegacyAdoptionInput::Unsupported { .. } => {
                        Err(E::custom("unsupported legacy adoption version"))
                    }
                }
            }
        }
        d.deserialize_str(Visitor)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(test)]
mod tests;
