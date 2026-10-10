//! Coherent read-only SQL snapshot with finite complete-row/incident-edge query work.
use super::*;
use rusqlite::{Connection, params};
impl LibraryDatabase {
    pub(in super::super) fn harvest_facts(
        &mut self,
        owner: &str,
        request: &HarvestFactsRequest,
        generation: &Arc<Generation>,
    ) -> Result<HarvestFactsBatch, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::HarvestFactsStaleOwner);
        }
        request.validate_sources()?;
        let tx = self.connection.transaction()?;
        let entries = budget::run(&tx, &request.canceled.flag, || {
            let mut entries = Vec::with_capacity(request.files.len());
            for f in &request.files {
                if request.canceled.is_canceled() {
                    return Err(LibraryError::HarvestFactsCanceled);
                }
                // The complete privately issued cohort was fenced immediately before this
                // transaction and will be fenced again after all rows are consumed. Per-row
                // endpoint revalidation here added no atomicity to this separate SQLite read.
                entries.push(read(&tx, f)?);
            }
            Ok(entries)
        })?;
        request.validate_sources()?;
        // Bound actual serialized output through the same capped writer used by owner receipts.
        // Source epoch is separately already accounted in the input budget.
        let wire: Vec<_> = entries
            .iter()
            .map(|e| (e.path.as_str(), e.state.as_str(), e.tracked, e.derivatives))
            .collect();
        let r = request.files[0].source_revision();
        let mut cap = crate::operation_journal::codec::CappedWriter::new(HARVEST_FACTS_MAX_BYTES);
        (
            wire,
            r.source_id().as_str(),
            r.root_identity().as_bytes(),
            r.database_identity(),
            r.root_binding_identity(),
            r.lifecycle_generation(),
            r.lease_epoch(),
            r.revision(),
            request.owner,
            generation.identity,
        )
            .serialize(&mut serde_json::Serializer::new(&mut cap))
            .map_err(|_| LibraryError::HarvestFactsInput)?;
        tx.commit()?;
        Ok(HarvestFactsBatch {
            entries,
            source: r.clone(),
            owner: request.owner,
            generation_id: generation.identity,
            generation: Arc::downgrade(generation),
            canceled: request.canceled.clone(),
        })
    }
}
fn read(c: &Connection, f: &SourceFileObservation) -> Result<HarvestFactsEntry, LibraryError> {
    let source = f.source_revision().source_id();
    let key = ReferenceFileKey::new(source.clone(), f.path().clone())?;
    // Existing complete row/edge shape preflight bounds field types/lengths before allocation,
    // then streams <=64 incident rows into <=64KiB. No whole-source vector or unbounded count.
    super::super::reference_snapshot::read(c, &key, &key)
        .map_err(|_| LibraryError::HarvestFactsMalformed)?;
    let args = params![source.as_str(), f.path().as_str()];
    let row=c.query_row("SELECT harvest_state, COALESCE(file_size=?3 AND modified_ns=?4 AND content_hash=?5,0) FROM harvest_files WHERE source_id=?1 AND relative_path=?2 LIMIT 1",params![source.as_str(),f.path().as_str(),f.file_size() as i64,f.modified_ns(),f.content_hash()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?)));
    let (state, tracked) = match row {
        Err(rusqlite::Error::QueryReturnedNoRows) => (HarvestState::New, false),
        Ok((state, true)) => {
            if !matches!(
                state.as_str(),
                "new" | "seen" | "touched" | "done" | "ignored"
            ) {
                return Err(LibraryError::HarvestFactsMalformed);
            }
            (HarvestState::from_stored(&state), true)
        }
        Ok(_) => return Err(LibraryError::HarvestFactsIdentityUncertain),
        Err(e) => return Err(e.into()),
    };
    // Validate semantic identity only for the endpoint naming this requested live file.
    // Other endpoints remain bounded historical metadata, not a live/physical source proof.
    let valid:bool=c.query_row("WITH incident AS (SELECT * FROM harvest_derivations WHERE (parent_source_id=?1 AND parent_relative_path=?2) OR (child_source_id=?1 AND child_relative_path=?2) LIMIT 65) SELECT NOT EXISTS(SELECT 1 FROM incident WHERE id<=0 OR created_at<0 OR operation NOT IN ('copy','extract','crop_copy','trim_copy','reverse_copy','edit_copy','slide_copy','normalize_copy','duplicate_double_copy','export','copy_to_primary') OR NOT COALESCE((parent_source_id!=?1 OR parent_relative_path!=?2 OR (parent_file_size=?3 AND parent_modified_ns=?4 AND parent_content_hash=?5)) AND (child_source_id!=?1 OR child_relative_path!=?2 OR (child_file_size=?3 AND child_modified_ns=?4 AND child_content_hash=?5)),0)) AND (SELECT COUNT(*) FROM incident)=(SELECT COUNT(DISTINCT id) FROM incident)",params![source.as_str(),f.path().as_str(),f.file_size() as i64,f.modified_ns(),f.content_hash()],|r|r.get(0))?;
    if !valid {
        return Err(LibraryError::HarvestFactsIdentityUncertain);
    }
    let derivatives:i64=c.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM harvest_derivations WHERE parent_source_id=?1 AND parent_relative_path=?2 LIMIT 65)",args,|r|r.get(0))?;
    if !(0..=HARVEST_FACTS_MAX_INCIDENT_EDGES as i64).contains(&derivatives) {
        return Err(LibraryError::HarvestFactsMalformed);
    }
    Ok(HarvestFactsEntry {
        path: f.path().clone(),
        state,
        tracked,
        derivatives: derivatives as u64,
    })
}
