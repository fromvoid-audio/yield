//! Private live evidence and exact frozen prerequisite validation; no pathname reacquisition.
use super::*;
use crate::operation_journal::{coordinator::UniversalCommandArgs, work_plan::WorkUnitRole};
use crate::sample_sources::db::{ManifestEnrollmentKind, SourceFileObservationKind};
use payload::{File, Registry};
impl HarvestCopyRequest {
    pub(in super::super) fn validate_live(&self, new_effect: bool) -> Result<(), LibraryError> {
        self.family
            .validate()
            .map_err(|_| LibraryError::HarvestCopyEvidence)?;
        if self.family.operation() != self.payload.operation
            || self.family.harvest_key() != self.payload.work
            || self.family.profile_identity() != self.payload.profile
            || self.family.canonical_bytes()? != self.payload.family
        {
            return Err(invalid());
        }
        self.validate_sources(new_effect)
    }
    pub(super) fn validate_sources(&self, new_effect: bool) -> Result<(), LibraryError> {
        self.parent
            .validate()
            .map_err(|_| LibraryError::HarvestCopyEvidence)?;
        self.child
            .validate()
            .map_err(|_| LibraryError::HarvestCopyEvidence)?;
        self.enrollment
            .validate()
            .map_err(|_| LibraryError::HarvestCopyEvidence)?;
        let parent = File::from_observation(&self.parent)?;
        let child = File::from_observation(&self.child)?;
        if self.parent.file().kind() != SourceFileObservationKind::SupportedAudio
            || self.child.file().kind() != SourceFileObservationKind::SupportedAudio
            || !self.payload.parent.scope.same_storage(&parent.scope)
            || !self.payload.child.scope.same_storage(&child.scope)
            || parent.path != self.payload.parent.path
            || child.path != self.payload.child.path
            || new_effect && (parent != self.payload.parent || child != self.payload.child)
        {
            return Err(LibraryError::HarvestCopyEvidence);
        }
        let copy = match self.family.command() {
            UniversalCommandArgs::Copy(c) | UniversalCommandArgs::DuplicateSame(c) => c,
            _ => return Err(invalid()),
        };
        if copy.source().source_id().as_str() != parent.scope.source
            || copy.source().relative_path() != &parent.path
            || copy.destination().source_id().as_str() != child.scope.source
            || copy.destination().relative_path() != &child.path
        {
            return Err(invalid());
        }
        for (endpoint, facts) in [
            (copy.source(), &self.payload.parent),
            (copy.destination(), &self.payload.child),
        ] {
            let fence = self
                .family
                .participants()
                .descriptors()
                .iter()
                .find(|p| p.id() == endpoint.participant_id())
                .and_then(|p| p.source_fence())
                .ok_or_else(invalid)?;
            if fence.source_id().as_str() != facts.scope.source
                || fence.root_identity().as_bytes() != facts.scope.root
                || fence.database_identity() != facts.scope.database
            {
                return Err(invalid());
            }
        }
        let expected = copy.expected_source();
        let historical = &self.payload.parent;
        if expected.source_revision() != historical.scope.revision
            || expected.file_identity() != historical.object
            || expected.file_size() != historical.size
            || expected.modified_ns() != historical.modified
        {
            return Err(invalid());
        }
        if self.enrollment.operation() != self.payload.operation
            || self.enrollment.proposal_bytes() != self.payload.enrollment
            || self.enrollment.committed_revision() != self.payload.enrolled_revision
            || self.enrollment.source_id() != self.payload.child.scope.source
            || self.enrollment.database_identity() != self.payload.child.scope.database
            || self.enrollment.root_identity() != self.payload.child.scope.root
            || self.payload.enrolled_revision > self.payload.child.scope.revision
        {
            return Err(invalid());
        }
        let enrolled = self.enrollment.entries();
        if enrolled.len() != 1 {
            return Err(invalid());
        }
        let e = &enrolled[0];
        let c = &self.payload.child;
        if e.kind != ManifestEnrollmentKind::Audio
            || e.path != c.path
            || e.size != c.size
            || e.modified_ns != c.modified
            || e.file_identity != c.object
            || e.content_hash.as_deref() != Some(&c.hash)
        {
            return Err(invalid());
        }
        let plan = self.family.work_plan();
        let destination = self.enrollment.work_key();
        if destination.participant_id() != copy.destination().participant_id()
            || !matches!(
                destination.role(),
                WorkUnitRole::SourceAffect | WorkUnitRole::DestinationSourceReconcile
            )
            || !plan
                .units()
                .iter()
                .any(|u| u.is_applicable() && u.key() == destination)
            || !plan
                .units()
                .iter()
                .any(|u| u.is_applicable() && u.key() == self.payload.registry.work)
            || Registry::from_receipt(&self.registry)? != self.payload.registry
        {
            return Err(invalid());
        }
        Ok(())
    }
}
pub(super) fn registry(
    connection: &rusqlite::Connection,
    expected: &Registry,
) -> Result<(), LibraryError> {
    // Fixed scalar/type/byte predicates precede materialization, including corrupt schemas.
    let mut statement=connection.prepare("SELECT CASE WHEN typeof(sequence)='integer' AND sequence>0 AND typeof(work_key)='text' AND length(CAST(work_key AS BLOB)) BETWEEN 1 AND 256 AND typeof(payload_identity)='blob' AND length(payload_identity)=32 AND typeof(owner_identity)='text' AND length(CAST(owner_identity AS BLOB))=36 AND typeof(disposition)='text' AND length(CAST(disposition AS BLOB))<=24 THEN 1 ELSE 0 END,sequence, CASE WHEN length(CAST(work_key AS BLOB))<=256 THEN work_key END, CASE WHEN length(payload_identity)=32 THEN payload_identity END,CASE WHEN length(CAST(owner_identity AS BLOB))=36 THEN owner_identity END,CASE WHEN length(CAST(disposition AS BLOB))<=24 THEN disposition END FROM source_registry_participant_receipts WHERE operation_id=?1 LIMIT 2")?;
    let mut rows = statement.query([expected.operation.to_string()])?;
    let row = rows.next()?.ok_or(LibraryError::HarvestCopyEvidence)?;
    if row.get::<_, i64>(0)? != 1 {
        return Err(LibraryError::HarvestCopyEvidence);
    }
    let sequence: i64 = row.get(1)?;
    let key: String = row.get(2)?;
    let identity: Vec<u8> = row.get(3)?;
    let owner: String = row.get(4)?;
    let disposition: String = row.get(5)?;
    if rows.next()?.is_some()
        || sequence as u64 != expected.sequence
        || key != serde_json::to_string(&expected.work)?
        || identity != expected.identity
        || owner != expected.owner.to_string()
        || disposition != "applied"
    {
        return Err(LibraryError::HarvestCopyEvidence);
    }
    Ok(())
}
