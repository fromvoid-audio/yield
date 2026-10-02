//! One SQLite transaction for the explicit state and immutable result, with source fencing.
use super::*;
use rusqlite::{Connection, params};
impl LibraryDatabase {
    pub(in super::super) fn harvest_authored_snapshot(
        &mut self,
        owner: &str,
        request: &HarvestAuthoredSnapshotRequest,
    ) -> Result<HarvestAuthoredSnapshot, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::HarvestAuthoredStaleOwner);
        }
        request.validate_sources()?;
        let file = File::from_observation(&request.source)?;
        let key = file.key()?;
        let tx = self.connection.transaction()?;
        let snapshot = super::super::reference_snapshot::read(&tx, &key, &key)?;
        request.validate_sources()?;
        tx.commit()?;
        Ok(HarvestAuthoredSnapshot {
            file,
            identity: snapshot.identity,
            owner: request.owner,
        })
    }
    pub(in super::super) fn apply_harvest_authored(
        &mut self,
        owner: &str,
        request: &HarvestAuthoredRequest,
    ) -> Result<HarvestAuthoredResult, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::HarvestAuthoredStaleOwner);
        }
        request.validate_sources(false)?;
        let tx = self.connection.transaction()?;
        if let Some(receipt) = ledger::read(&tx, request)? {
            request.validate_sources(false)?;
            tx.commit()?;
            return Ok(HarvestAuthoredResult::Replayed(receipt));
        }
        request.validate_sources(true)?;
        ledger::capacity(&tx, request)?;
        crate::sqlite_wal::enforce_wal_admission(&tx)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| invalid())?
            .as_secs();
        let now = i64::try_from(now).map_err(|_| invalid())?;
        let disposition = apply(&tx, &request.payload, now)?;
        request.validate_sources(true)?;
        let receipt = ledger::insert(
            &tx,
            request,
            &Outcome {
                version: 1,
                disposition,
                created: now,
            },
        )?;
        request.validate_sources(true)?;
        tx.commit()?;
        if request.validate_sources(true).is_err() {
            return Err(LibraryError::HarvestAuthoredCommitUncertain);
        }
        Ok(HarvestAuthoredResult::Committed(receipt))
    }
}
fn apply(
    c: &Connection,
    p: &Payload,
    now: i64,
) -> Result<HarvestAuthoredDisposition, LibraryError> {
    let f = &p.file;
    let key = f.key()?;
    if super::super::reference_snapshot::read(c, &key, &key)?.identity != p.snapshot {
        return Ok(HarvestAuthoredDisposition::GraphChanged);
    }
    let count: i64 = c.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM harvest_files WHERE source_id=?1 AND relative_path=?2 LIMIT 2)", params![f.scope.source,f.path.as_str()], |r|r.get(0))?;
    if count > 1 {
        return Err(LibraryError::HarvestAuthoredMalformedGraph);
    }
    if count == 1 {
        let valid: bool = c.query_row("SELECT (file_size IS NULL OR (typeof(file_size)='integer' AND file_size>=0)) AND (modified_ns IS NULL OR typeof(modified_ns)='integer') AND harvest_state IN ('new','seen','touched','done','ignored') FROM harvest_files WHERE source_id=?1 AND relative_path=?2", params![f.scope.source,f.path.as_str()], |r|r.get(0))?;
        if !valid {
            return Err(LibraryError::HarvestAuthoredMalformedGraph);
        }
        let matches: bool = c.query_row("SELECT COALESCE(file_size=?3 AND modified_ns=?4 AND content_hash=?5,0) FROM harvest_files WHERE source_id=?1 AND relative_path=?2", params![f.scope.source,f.path.as_str(),f.size as i64,f.modified,f.hash], |r|r.get(0))?;
        if !matches {
            return Ok(HarvestAuthoredDisposition::IdentityConflict);
        }
    } else {
        c.execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at) VALUES(?1,?2,?3,?4,?5,'new',?6)",params![f.scope.source,f.path.as_str(),f.size as i64,f.modified,f.hash,now])?;
    }
    // Capture bounded independent authored fields and calculate the exact expected row.
    let (discovered, mut seen, mut touched, mut done, mut ignored, note):
        (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<String>) =
        c.query_row("SELECT discovered_at,seen_at,touched_at,done_at,ignored_at,note FROM harvest_files WHERE source_id=?1 AND relative_path=?2",params![f.scope.source,f.path.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    match p.state.as_str() {
        "new" => {
            seen = None;
            touched = None;
            done = None;
            ignored = None;
        }
        "seen" => {
            seen = Some(seen.unwrap_or(now));
        }
        "touched" => {
            seen = Some(seen.unwrap_or(now));
            touched = Some(touched.unwrap_or(now));
        }
        "done" => {
            done = Some(done.unwrap_or(now));
        }
        "ignored" => {
            ignored = Some(ignored.unwrap_or(now));
        }
        _ => return Err(invalid()),
    }
    // The existing explicit-user semantics reset lifecycle timestamps only for New.
    c.execute(
        super::super::harvest::harvest_state_update_sql(),
        params![
            p.state.as_str(),
            f.scope.source,
            f.path.as_str(),
            now,
            p.state == "new"
        ],
    )?;
    let matches: bool = c.query_row("SELECT harvest_state=?3 AND file_size=?4 AND modified_ns=?5 AND content_hash=?6 AND discovered_at=?7 AND seen_at IS ?8 AND touched_at IS ?9 AND done_at IS ?10 AND ignored_at IS ?11 AND note IS ?12 FROM harvest_files WHERE source_id=?1 AND relative_path=?2",params![f.scope.source,f.path.as_str(),p.state.as_str(),f.size as i64,f.modified,f.hash,discovered,seen,touched,done,ignored,note],|r|r.get(0))?;
    if !matches {
        return Err(LibraryError::HarvestAuthoredMalformedGraph);
    }
    Ok(HarvestAuthoredDisposition::Applied)
}
