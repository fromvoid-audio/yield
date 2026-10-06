//! Borrowed lexical views preserve the default codec's map ordering without a Value graph.
use super::*;
use crate::sample_sources::db::{
    SourceRegistryAuditObservation, SourceRegistryRegistrationObservation,
};
use serde::ser::SerializeMap;
#[derive(Serialize)]
pub(super) struct Fence<'a> {
    maximum_pages: u64,
    objects: [OrderedObject<'a>; 3],
    page_size: u64,
    preparation_nonce: Uuid,
    subject: Subject<'a>,
    version: u8,
}
#[derive(Serialize)]
struct OrderedObject<'a> {
    bytes: u64,
    ceiling: u64,
    device: u64,
    identity: &'a str,
    unit: u64,
}
#[derive(Serialize)]
struct Subject<'a> {
    audit: Audit<'a>,
    database_identity: &'a str,
    database_root_identity: &'a str,
    lease_epoch: u64,
    lease_owner: Uuid,
    lifecycle_generation: u64,
    media_root_identity: &'a [u8],
    profile_acquisition: Uuid,
    registration: Option<Registration<'a>>,
    root_binding_identity: &'a str,
    scan_revision: u64,
    source_id: &'a str,
}
#[derive(Serialize)]
struct Registration<'a> {
    phase: &'a SourceRegistryRegistrationPhase,
    registration_id: Uuid,
}
impl<'a> From<&'a SourceRegistryRegistrationObservation> for Registration<'a> {
    fn from(value: &'a SourceRegistryRegistrationObservation) -> Self {
        Self {
            phase: &value.phase,
            registration_id: value.registration_id(),
        }
    }
}
struct Audit<'a>(&'a SourceRegistryAuditObservation);
impl Serialize for Audit<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            SourceRegistryAuditObservation::Unavailable {} => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("status", "unavailable")?;
                map.end()
            }
            SourceRegistryAuditObservation::Captured {
                revision,
                directory_generation,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("directory_generation", directory_generation)?;
                map.serialize_entry("revision", revision)?;
                map.serialize_entry("status", "captured")?;
                map.end()
            }
        }
    }
}
impl<'a> Fence<'a> {
    pub(super) fn new(value: &'a StoredSourceLeaseFence) -> Self {
        let s = value.subject();
        Self {
            maximum_pages: value.maximum_pages,
            objects: value.objects.each_ref().map(|o| OrderedObject {
                bytes: o.bytes,
                ceiling: o.ceiling,
                device: o.device,
                identity: &o.identity,
                unit: o.unit,
            }),
            page_size: value.page_size,
            preparation_nonce: value.preparation_nonce,
            subject: Subject {
                audit: Audit(s.audit()),
                database_identity: s.database_identity(),
                database_root_identity: s.database_root_identity(),
                lease_epoch: s.lease_epoch(),
                lease_owner: s.lease_owner(),
                lifecycle_generation: s.lifecycle_generation(),
                media_root_identity: s.media_root_identity(),
                profile_acquisition: s.profile_acquisition(),
                registration: s.registration().map(Registration::from),
                root_binding_identity: s.root_binding_identity(),
                scan_revision: s.scan_revision(),
                source_id: s.source_id(),
            },
            version: value.version,
        }
    }
}
const _: () = assert!(std::mem::size_of::<Fence<'static>>() <= 1024);
