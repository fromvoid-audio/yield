//! Bounded canonical source facts; decoding never creates live authority.
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
pub(super) struct Payload {
    pub version: u8,
    pub operation: OperationId,
    pub profile: String,
    pub file: File,
    pub snapshot: [u8; 32],
    pub state: String,
}
impl Payload {
    pub fn validate(&self) -> Result<(), LibraryError> {
        if self.version != 1
            || self.operation.as_uuid().is_nil()
            || !text(&self.profile, 256)
            || !self.file.valid()
            || !matches!(
                self.state.as_str(),
                "new" | "seen" | "touched" | "done" | "ignored"
            )
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
        let p: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        if p.encode()? != bytes {
            return Err(invalid());
        }
        Ok(p)
    }
    pub fn key(&self) -> Result<String, LibraryError> {
        Ok(self.operation.as_uuid().to_string())
    }
}
fn text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.contains('\0')
}
