//! Source/global transactions remain independent; uncertain acknowledgement retains identity.
use super::*;
impl LibraryDatabase {
    pub(in super::super) fn apply_harvest_copy(
        &mut self,
        owner: &str,
        request: &HarvestCopyRequest,
    ) -> Result<HarvestCopyResult, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::HarvestCopyStaleOwner);
        }
        // Journal queue round-trips always occur outside the SQLite transaction.
        request.validate_live(false)?;
        let tx = self.connection.transaction()?;
        if let Some(receipt) = ledger::read(&tx, request)? {
            request.validate_sources(false)?;
            tx.commit()?;
            return Ok(HarvestCopyResult::Replayed(receipt));
        }
        request.validate_sources(true)?;
        validation::registry(&tx, &request.payload.registry)?;
        ledger::capacity(&tx, request)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| invalid())?
            .as_millis();
        let now = i64::try_from(now).map_err(|_| invalid())?;
        let outcome = mutation::apply(&tx, &request.payload, now)?;
        request.validate_sources(true)?;
        let receipt = ledger::insert(&tx, request, &outcome)?;
        request.validate_sources(true)?;
        tx.commit()?;
        if request.validate_live(false).is_err() {
            return Err(LibraryError::HarvestCopyCommitUncertain);
        }
        Ok(HarvestCopyResult::Committed(receipt))
    }
}
