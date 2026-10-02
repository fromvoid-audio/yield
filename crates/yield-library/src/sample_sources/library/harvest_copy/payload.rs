//! Canonical bounded historical facts; decoding them never mints live observations.
use super::*;
use crate::sample_sources::db::SourceFileObservation;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    pub source: String,
    pub root: Vec<u8>,
    pub database: String,
    pub binding: String,
    pub lifecycle: u64,
    pub lease: u64,
    pub revision: u64,
}
impl Scope {
    pub fn from_file(file: &SourceFileObservation) -> Self {
        let r = file.source_revision();
        Self {
            source: r.source_id().as_str().into(),
            root: r.root_identity().as_bytes().to_vec(),
            database: r.database_identity().into(),
            binding: r.root_binding_identity().into(),
            lifecycle: r.lifecycle_generation(),
            lease: r.lease_epoch(),
            revision: r.revision(),
        }
    }
    pub fn same_storage(&self, other: &Self) -> bool {
        self.source == other.source
            && self.root == other.root
            && self.database == other.database
            && self.binding == other.binding
    }
    fn valid(&self) -> bool {
        text(&self.source, 256)
            && text(&self.database, 256)
            && text(&self.binding, 256)
            && !self.root.is_empty()
            && self.root.len() <= 4096
            && self.lease > 0
            && [self.lease, self.lifecycle, self.revision]
                .iter()
                .all(|v| *v <= i64::MAX as u64)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct File {
    pub scope: Scope,
    pub path: StoredRelativePath,
    pub size: u64,
    pub modified: i64,
    pub object: String,
    pub hash: String,
}
impl File {
    pub fn from_observation(o: &SourceContentIdentityObservation) -> Result<Self, LibraryError> {
        let f = o.file();
        let data = Self {
            scope: Scope::from_file(f),
            path: f.path().clone(),
            size: f.file_size(),
            modified: f.modified_ns(),
            object: f.file_identity().ok_or_else(invalid)?.into(),
            hash: o.content_hash().into(),
        };
        if !data.valid() {
            return Err(invalid());
        }
        Ok(data)
    }
    fn valid(&self) -> bool {
        self.scope.valid()
            && self.path.validate().is_ok()
            && self.path.as_str().len() <= 4096
            && self.size <= i64::MAX as u64
            && self.modified >= 0
            && text(&self.object, 512)
            && self.hash.len() == 64
            && self
                .hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
    pub fn key(&self) -> Result<super::super::ReferenceFileKey, LibraryError> {
        super::super::ReferenceFileKey::new(
            crate::sample_sources::SourceId::from_string(&self.scope.source),
            self.path.clone(),
        )
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub rating: Option<i64>,
    pub tags: Vec<String>,
    pub playback: Option<String>,
}
impl Metadata {
    fn valid(&self) -> bool {
        self.rating.is_none_or(|r| (-3..=3).contains(&r))
            && self.tags.len() <= 64
            && self.tags.iter().all(|t| text(t, 256))
            && self.tags.windows(2).all(|p| p[0] < p[1])
            && self.tags.iter().map(String::len).sum::<usize>() <= 16384
            && self
                .playback
                .as_ref()
                .is_none_or(|p| matches!(p.as_str(), "one-shot" | "loop"))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registry {
    pub operation: OperationId,
    pub work: WorkUnitKey,
    pub sequence: u64,
    pub owner: Uuid,
    pub identity: [u8; 32],
}
impl Registry {
    pub fn from_receipt(r: &SourceRegistryParticipantReceipt) -> Result<Self, LibraryError> {
        if r.disposition() != super::super::SourceRegistryParticipantDisposition::Applied {
            return Err(invalid());
        }
        Ok(Self {
            operation: r.operation_id(),
            work: r.key(),
            sequence: r.sequence(),
            owner: r.owner_identity(),
            identity: *r.payload_identity(),
        })
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Payload {
    pub version: u8,
    pub operation: OperationId,
    pub work: WorkUnitKey,
    pub profile: String,
    pub family: Vec<u8>,
    pub parent: File,
    pub child: File,
    pub enrollment: Vec<u8>,
    pub enrolled_revision: u64,
    pub registry: Registry,
    pub snapshot: [u8; 32],
    pub metadata: Metadata,
    pub tool: String,
}
impl Payload {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner: &WritableProfileGuard,
        family: &FrozenCopyObservation,
        parent: &SourceContentIdentityObservation,
        child: &SourceContentIdentityObservation,
        enrollment: &ManifestEnrollmentReceipt,
        registry: &SourceRegistryParticipantReceipt,
        snapshot: ReferenceSnapshot,
        metadata: super::super::HarvestMetadataSnapshot,
        tool: String,
    ) -> Result<Self, LibraryError> {
        let parent = File::from_observation(parent)?;
        let child = File::from_observation(child)?;
        if snapshot.old != parent.key()? || snapshot.new != child.key()? {
            return Err(invalid());
        }
        let metadata = Metadata {
            rating: metadata.rating,
            tags: metadata.tags,
            playback: metadata.playback_type,
        };
        if !metadata.valid() || !text(&tool, 256) {
            return Err(invalid());
        }
        let data = Self {
            version: 1,
            operation: family.operation(),
            work: family.harvest_key(),
            profile: owner.profile_root_identity().into(),
            family: family.canonical_bytes()?,
            parent,
            child,
            enrollment: enrollment.proposal_bytes().to_vec(),
            enrolled_revision: enrollment.committed_revision(),
            registry: Registry::from_receipt(registry)?,
            snapshot: snapshot.identity,
            metadata,
            tool,
        };
        data.validate()?;
        Ok(data)
    }
    pub fn validate(&self) -> Result<(), LibraryError> {
        use crate::operation_journal::work_plan::WorkUnitRole;
        if self.version != 1
            || self.operation.as_uuid().is_nil()
            || self.work.participant_id().as_uuid().is_nil()
            || self.work.role() != WorkUnitRole::Harvest
            || !text(&self.profile, 256)
            || !self.parent.valid()
            || !self.child.valid()
            || self.parent.scope.source == self.child.scope.source
                && self.parent.path == self.child.path
            || self.parent.size != self.child.size
            || self.parent.hash != self.child.hash
            || self.family.is_empty()
            || self.family.len() > 65536
            || self.enrollment.is_empty()
            || self.enrollment.len() > 65536
            || self.enrolled_revision > i64::MAX as u64
            || self.registry.operation != self.operation
            || self.registry.sequence == 0
            || self.registry.sequence > i64::MAX as u64
            || self.registry.owner.is_nil()
            || self.registry.work.role() != WorkUnitRole::GlobalLibrary
            || self.registry.work.participant_id().as_uuid().is_nil()
            || !self.metadata.valid()
            || !text(&self.tool, 256)
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, LibraryError> {
        self.validate()?;
        bounded(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, LibraryError> {
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err(invalid());
        }
        let p: Self = serde_json::from_slice(bytes)?;
        p.validate()?;
        if p.encode()? != bytes {
            return Err(invalid());
        }
        Ok(p)
    }
    pub fn key(&self) -> Result<String, LibraryError> {
        Ok(serde_json::to_string(&(self.operation, self.work))?)
    }
}
pub(super) fn text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.contains('\0')
}
