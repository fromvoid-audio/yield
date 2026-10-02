//! Bounded exact-row graph effects; existing authored state is never reparsed or merged away.
use super::*;
use payload::File;
use rusqlite::{Connection, params};
fn shape(c: &Connection, f: &File) -> Result<Option<bool>, LibraryError> {
    let count:i64=c.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM harvest_files WHERE source_id=?1 AND relative_path=?2 LIMIT 2)",params![f.scope.source,f.path.as_str()],|r|r.get(0))?;
    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(LibraryError::HarvestCopyMalformedGraph);
    }
    // Every stored authored field remains bounded before touching or allocating the row.
    let valid:bool=c.query_row("SELECT typeof(source_id)='text' AND typeof(relative_path)='text' AND (file_size IS NULL OR (typeof(file_size)='integer' AND file_size>=0)) AND (modified_ns IS NULL OR typeof(modified_ns)='integer') AND (content_hash IS NULL OR (typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB))<=256)) AND typeof(harvest_state)='text' AND harvest_state IN ('new','seen','touched','done','ignored') AND typeof(discovered_at)='integer' AND (seen_at IS NULL OR typeof(seen_at)='integer') AND (touched_at IS NULL OR typeof(touched_at)='integer') AND (done_at IS NULL OR typeof(done_at)='integer') AND (ignored_at IS NULL OR typeof(ignored_at)='integer') AND (note IS NULL OR (typeof(note)='text' AND length(CAST(note AS BLOB))<=4096)) FROM harvest_files WHERE source_id=?1 AND relative_path=?2",params![f.scope.source,f.path.as_str()],|r|r.get(0))?;
    if !valid {
        return Err(LibraryError::HarvestCopyMalformedGraph);
    }
    // NULL/unverified old content cannot silently merge unrelated authored state.
    let matches:bool=c.query_row("SELECT COALESCE(file_size=?3 AND modified_ns=?4 AND content_hash=?5,0) FROM harvest_files WHERE source_id=?1 AND relative_path=?2",params![f.scope.source,f.path.as_str(),f.size as i64,f.modified,f.hash],|r|r.get(0))?;
    Ok(Some(matches))
}
pub(super) fn apply(c: &Connection, p: &Payload, now: i64) -> Result<Outcome, LibraryError> {
    let current = super::super::reference_snapshot::read(c, &p.parent.key()?, &p.child.key()?)?;
    if current.identity != p.snapshot {
        return Ok(Outcome {
            version: 1,
            disposition: HarvestCopyDisposition::GraphChanged,
            edge: None,
            created: now,
        });
    }
    let parent = shape(c, &p.parent)?;
    let child = shape(c, &p.child)?;
    if parent == Some(false) || child == Some(false) {
        return Ok(Outcome {
            version: 1,
            disposition: HarvestCopyDisposition::IdentityConflict,
            edge: None,
            created: now,
        });
    }
    for (file, exists, origin) in [(&p.parent, parent, true), (&p.child, child, false)] {
        if exists.is_none() {
            c.execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at,touched_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![file.scope.source,file.path.as_str(),file.size as i64,file.modified,file.hash,if origin {"touched"}else{"new"},now,if origin {Some(now)}else{None}])?;
        }
    }
    // Updating these two columns preserves Done/Ignored and all authored note/timestamp values.
    if parent.is_some() {
        c.execute("UPDATE harvest_files SET harvest_state='touched',touched_at=COALESCE(touched_at,?3) WHERE source_id=?1 AND relative_path=?2 AND harvest_state IN ('new','seen')",params![p.parent.scope.source,p.parent.path.as_str(),now])?;
    }
    let folder = p
        .child
        .path
        .as_str()
        .rsplit_once('/')
        .map(|(folder, _)| folder);
    c.execute("INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,parent_file_size,parent_modified_ns,parent_content_hash,child_source_id,child_relative_path,child_file_size,child_modified_ns,child_content_hash,operation,source_range_start,source_range_end,output_duration_seconds,destination_folder,inherited_rating,inherited_tags_json,inherited_playback_type,tool_version,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'copy',NULL,NULL,NULL,?11,?12,?13,?14,?15,?16)",params![p.parent.scope.source,p.parent.path.as_str(),p.parent.size as i64,p.parent.modified,p.parent.hash,p.child.scope.source,p.child.path.as_str(),p.child.size as i64,p.child.modified,p.child.hash,folder,p.metadata.rating,serde_json::to_string(&p.metadata.tags)?,p.metadata.playback,p.tool,now])?;
    let edge = c.last_insert_rowid();
    if edge <= 0 {
        return Err(LibraryError::HarvestCopyMalformedGraph);
    }
    let matches:bool=c.query_row("SELECT parent_source_id=?2 AND parent_relative_path=?3 AND parent_file_size=?4 AND parent_modified_ns=?5 AND parent_content_hash=?6 AND child_source_id=?7 AND child_relative_path=?8 AND child_file_size=?9 AND child_modified_ns=?10 AND child_content_hash=?11 AND operation='copy' AND source_range_start IS NULL AND source_range_end IS NULL AND output_duration_seconds IS NULL AND destination_folder IS ?12 AND inherited_rating IS ?13 AND inherited_tags_json=?14 AND inherited_playback_type IS ?15 AND tool_version=?16 AND created_at=?17 FROM harvest_derivations WHERE id=?1",params![edge,p.parent.scope.source,p.parent.path.as_str(),p.parent.size as i64,p.parent.modified,p.parent.hash,p.child.scope.source,p.child.path.as_str(),p.child.size as i64,p.child.modified,p.child.hash,folder,p.metadata.rating,serde_json::to_string(&p.metadata.tags)?,p.metadata.playback,p.tool,now],|r|r.get(0))?;
    if !matches || shape(c, &p.parent)? != Some(true) || shape(c, &p.child)? != Some(true) {
        return Err(LibraryError::HarvestCopyMalformedGraph);
    }
    Ok(Outcome {
        version: 1,
        disposition: HarvestCopyDisposition::Applied,
        edge: Some(edge as u64),
        created: now,
    })
}
