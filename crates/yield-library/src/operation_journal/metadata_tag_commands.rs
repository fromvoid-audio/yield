//! Metadata-only source tag intent data. Serialized values never carry live admission authority.
use super::ParticipantId;
use crate::sample_sources::db::{
    SourceCommandId, SourceTagAdmissionObservation, SourceTagAssignmentMutation, SourceWriter,
};
use crate::sample_sources::reconciliation::RootRelativePath;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Version of the first complete one-source metadata tag scope; dictionary finalization excluded.
pub const METADATA_TAG_SOURCE_ARGS_VERSION: u16 = 1;
/// Maximum canonical source request bytes, independent of physical metadata allocation.
pub const MAX_METADATA_TAG_SOURCE_ARGS_BYTES: usize = 256 * 1024;

/// Metadata assignment delta, never a file rename or delete command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum MetadataTagMutation {
    /// Transfer all old assignments to a distinct normal canonical identity.
    MetadataTagRename {
        /// Canonical normal target identity.
        target_identity: String,
    },
    /// Remove all old assignments, preserving the profile definition.
    MetadataTagDelete,
}

/// Bounded immutable intent observation for exactly one complete source assignment set.
/// Fields are private. Only the bounded decoder reads historical data; no live grant is decoded.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetadataTagSourceIntentArgs {
    version: u16,
    participant_id: ParticipantId,
    source_command_id: SourceCommandId,
    scope: MetadataTagHistoricalScope,
    directory_generation: u64,
    old_tag_id: i64,
    old_identity: String,
    mutation: MetadataTagMutation,
    paths: Vec<String>,
}

/// Finite data validation error, containing no caller paths or source data.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MetadataTagArgsError {
    /// Unknown scope format is retained but cannot be interpreted as a supported command.
    #[error("unsupported metadata tag scope version")]
    UnsupportedVersion,
    /// One immutable identity or continuity field is not bounded/current-shape data.
    #[error("invalid metadata tag scope identity")]
    Identity,
    /// A label is malformed, noncanonical, locked or a self-rename.
    #[error("invalid metadata tag normal identity")]
    Tag,
    /// The complete frozen set has invalid/noncanonical/duplicate/unsorted/oversized paths.
    #[error("invalid bounded metadata tag assignment set")]
    Paths,
    /// Canonical data exceeds the request byte bound or cannot be encoded.
    #[error("metadata tag canonical request exceeds its byte bound")]
    Bytes,
    /// Invalid, ambiguous or noncanonical supported wire data.
    #[error("invalid metadata tag wire data")]
    Wire,
    /// The original retained source observation no longer validates.
    #[error("metadata tag source observation is stale or unavailable")]
    Observation,
}

impl MetadataTagSourceIntentArgs {
    /// Build passive intent data from the real source owner observation. This does not admit it.
    pub fn from_observation(
        writer: &SourceWriter,
        observation: &SourceTagAdmissionObservation,
        participant_id: ParticipantId,
        source_command_id: SourceCommandId,
    ) -> Result<Self, MetadataTagArgsError> {
        writer
            .validate_tag_admission_observation(observation)
            .map_err(|_| MetadataTagArgsError::Observation)?;
        let reference = writer
            .reference_read_fence()
            .map_err(|_| MetadataTagArgsError::Observation)?;
        let selection = observation.selection();
        if reference.source_revision() != selection.revision() {
            return Err(MetadataTagArgsError::Observation);
        }
        let profile_acquisition_identity = reference
            .profile_acquisition_identity()
            .ok_or(MetadataTagArgsError::Observation)?;
        let r = selection.revision();
        let mutation = match selection.mutation() {
            SourceTagAssignmentMutation::Rename(target) => MetadataTagMutation::MetadataTagRename {
                target_identity: target.clone(),
            },
            SourceTagAssignmentMutation::Delete => MetadataTagMutation::MetadataTagDelete,
        };
        let value = Self {
            version: METADATA_TAG_SOURCE_ARGS_VERSION,
            participant_id,
            source_command_id,
            scope: MetadataTagHistoricalScope {
                profile_acquisition_identity,
                source_id: r.source_id().as_str().to_owned(),
                root_identity: r.root_identity().as_bytes().to_vec(),
                root_binding_identity: r.root_binding_identity().to_owned(),
                database_identity: r.database_identity().to_owned(),
                lifecycle_generation: r.lifecycle_generation(),
                lease_epoch: r.lease_epoch(),
                revision: r.revision(),
            },
            directory_generation: observation.directory_generation(),
            old_tag_id: selection.old_tag_id(),
            old_identity: selection.old_identity().to_owned(),
            mutation,
            paths: selection
                .paths()
                .iter()
                .map(|path| {
                    path.as_path()
                        .to_str()
                        .map(str::to_owned)
                        .ok_or(MetadataTagArgsError::Paths)
                })
                .collect::<Result<_, _>>()?,
        };
        value.validate()?;
        reference
            .validate()
            .map_err(|_| MetadataTagArgsError::Observation)?;
        writer
            .validate_tag_admission_observation(observation)
            .map_err(|_| MetadataTagArgsError::Observation)?;
        Ok(value)
    }
    /// Source, physical binding and initial live incarnation represented by this historical data.
    pub fn scope(&self) -> &MetadataTagHistoricalScope {
        &self.scope
    }
    /// Stable required source participant, not an owner-issued completion receipt.
    pub fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }
    /// Stable exact source command correlation, not an allocated OperationId.
    pub fn source_command_id(&self) -> SourceCommandId {
        self.source_command_id
    }
    /// Complete active generation represented by this scope.
    pub fn directory_generation(&self) -> u64 {
        self.directory_generation
    }
    /// Source-local old normal tag ID; not a profile dictionary ID.
    pub fn old_tag_id(&self) -> i64 {
        self.old_tag_id
    }
    /// Canonical old normal identity.
    pub fn old_identity(&self) -> &str {
        &self.old_identity
    }
    /// Requested metadata delta.
    pub fn mutation(&self) -> &MetadataTagMutation {
        &self.mutation
    }
    /// Entire bounded sorted assignment set; empty means the existing tag had no assignments.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Validate retained DTO shape only. Live source/claim authority requires the actual owners.
    pub fn validate(&self) -> Result<(), MetadataTagArgsError> {
        if self.version != METADATA_TAG_SOURCE_ARGS_VERSION {
            return Err(MetadataTagArgsError::UnsupportedVersion);
        }
        let r = &self.scope;
        if self.participant_id.as_uuid().is_nil()
            || self.source_command_id.as_uuid().is_nil()
            || self.old_tag_id <= 0
            || self.directory_generation == 0
            || self.directory_generation > i64::MAX as u64
            || r.revision == 0
            || r.revision > i64::MAX as u64
            || r.lease_epoch == 0
            || r.lease_epoch > i64::MAX as u64
            || r.lifecycle_generation == 0
            || r.lifecycle_generation > i64::MAX as u64
            || r.source_id.contains('\0')
            || r.source_id.is_empty()
            || r.source_id.len() > 256
            || r.root_identity.is_empty()
            || r.root_identity.len() > 4096
            || [
                &r.database_identity,
                &r.root_binding_identity,
                &r.profile_acquisition_identity,
            ]
            .iter()
            .any(|value| value.is_empty() || value.len() > 256 || value.contains('\0'))
        {
            return Err(MetadataTagArgsError::Identity);
        }
        if !normal_identity(&self.old_identity) {
            return Err(MetadataTagArgsError::Tag);
        }
        let limit = match &self.mutation {
            MetadataTagMutation::MetadataTagRename { target_identity } => {
                if !normal_identity(target_identity) || target_identity == &self.old_identity {
                    return Err(MetadataTagArgsError::Tag);
                }
                64
            }
            MetadataTagMutation::MetadataTagDelete => 128,
        };
        if self.paths.len() > limit {
            return Err(MetadataTagArgsError::Paths);
        }
        let mut previous: Option<&str> = None;
        for path in &self.paths {
            if path.is_empty()
                || path.len() > 1024
                || (path.as_bytes().get(1) == Some(&b':')
                    && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic))
                || path.contains(['\0', '\\'])
                || path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || RootRelativePath::try_from_path(path.into()).is_err()
                || previous.is_some_and(|old| old >= path.as_str())
            {
                return Err(MetadataTagArgsError::Paths);
            }
            previous = Some(path);
        }
        bounded_encode(self)?;
        Ok(())
    }
    /// Canonical immutable data after validation, not admission or capacity evidence.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, MetadataTagArgsError> {
        self.validate()?;
        bounded_encode(self)
    }
}

// Matches the source SQL classification after its shared whitespace/ASCII-case canonicalization.
fn normal_identity(label: &str) -> bool {
    if label.is_empty()
        || label.len() > 1024
        || label.contains('\0')
        || label
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
            != label
    {
        return false;
    }
    let edge = label.trim_matches([' ', '-', '_']);
    if matches!(edge, "loop" | "oneshot") {
        return false;
    }
    !(edge.len() > 7
        && edge.starts_with("one")
        && edge.ends_with("shot")
        && edge[3..edge.len() - 4]
            .chars()
            .all(|c| matches!(c, ' ' | '-' | '_')))
}

/// Explicit passive historical scope, with no conversion into a fresh SourceRevision.
/// Profile acquisition identity comes from the same retained writer's publication fence.
/// It is correlation data; the actual profile/journal owner must establish current authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataTagHistoricalScope {
    profile_acquisition_identity: String,
    source_id: String,
    root_identity: Vec<u8>,
    root_binding_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    lease_epoch: u64,
    revision: u64,
}
impl MetadataTagHistoricalScope {
    /// Historical profile acquisition identity, not an ownership capability.
    pub fn profile_acquisition_identity(&self) -> &str {
        &self.profile_acquisition_identity
    }
    /// Historical logical source identity.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
    /// Historical audited source-root bytes.
    pub fn root_identity(&self) -> &[u8] {
        &self.root_identity
    }
    /// Historical retained source database-root identity.
    pub fn root_binding_identity(&self) -> &str {
        &self.root_binding_identity
    }
    /// Historical database-file identity.
    pub fn database_identity(&self) -> &str {
        &self.database_identity
    }
    /// Historical source writer lifecycle.
    pub fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }
    /// Historical positive source lease epoch.
    pub fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }
    /// Historical audited source revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// Bounded retained data. Unknown versions remain opaque and cannot authorize an effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredMetadataTagSourceIntent {
    /// Fully validated canonical version-1 passive data.
    Supported(MetadataTagSourceIntentArgs),
    /// Preserve an uninterpreted newer format without issuing any command or grant.
    Unsupported {
        /// Uninterpreted wire format version.
        version: u64,
        /// Original bounded JSON retained without interpretation.
        bytes: Vec<u8>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u16,
    participant_id: ParticipantId,
    source_command_id: SourceCommandId,
    scope: MetadataTagHistoricalScope,
    directory_generation: u64,
    old_tag_id: i64,
    old_identity: String,
    mutation: MetadataTagMutation,
    #[serde(deserialize_with = "bounded_paths")]
    paths: Vec<String>,
}
fn bounded_paths<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<Vec<String>, D::Error> {
    struct Paths;
    impl<'de> serde::de::Visitor<'de> for Paths {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 128 bounded UTF-8 paths")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut paths = Vec::new();
            while let Some(path) = seq.next_element::<String>()? {
                if paths.len() == 128 || path.len() > 1024 {
                    return Err(serde::de::Error::custom("metadata tag path bound"));
                }
                paths.push(path);
            }
            Ok(paths)
        }
    }
    decoder.deserialize_seq(Paths)
}

/// Decode only after the byte bound; supported data must use its unique canonical JSON spelling.
/// This never reconstructs SourceRevision, an owner observation, capacity claim or write grant.
pub fn decode_metadata_tag_source_intent(
    bytes: &[u8],
) -> Result<StoredMetadataTagSourceIntent, MetadataTagArgsError> {
    if bytes.len() > MAX_METADATA_TAG_SOURCE_ARGS_BYTES {
        return Err(MetadataTagArgsError::Bytes);
    }
    #[derive(Deserialize)]
    struct Header {
        version: u64,
    }
    let header: Header = serde_json::from_slice(bytes).map_err(|_| MetadataTagArgsError::Wire)?;
    if header.version != u64::from(METADATA_TAG_SOURCE_ARGS_VERSION) {
        return Ok(StoredMetadataTagSourceIntent::Unsupported {
            version: header.version,
            bytes: bytes.to_vec(),
        });
    }
    let w: Wire = serde_json::from_slice(bytes).map_err(|_| MetadataTagArgsError::Wire)?;
    let args = MetadataTagSourceIntentArgs {
        version: w.version,
        participant_id: w.participant_id,
        source_command_id: w.source_command_id,
        scope: w.scope,
        directory_generation: w.directory_generation,
        old_tag_id: w.old_tag_id,
        old_identity: w.old_identity,
        mutation: w.mutation,
        paths: w.paths,
    };
    if args.canonical_bytes()?.as_slice() != bytes {
        return Err(MetadataTagArgsError::Wire);
    }
    Ok(StoredMetadataTagSourceIntent::Supported(args))
}

fn bounded_encode(value: &MetadataTagSourceIntentArgs) -> Result<Vec<u8>, MetadataTagArgsError> {
    struct Output(Vec<u8>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_METADATA_TAG_SOURCE_ARGS_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("metadata tag byte bound"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Output(Vec::new());
    serde_json::to_writer(&mut out, value).map_err(|_| MetadataTagArgsError::Bytes)?;
    Ok(out.0)
}

#[cfg(test)]
#[path = "metadata_tag_commands_tests.rs"]
mod tests;
