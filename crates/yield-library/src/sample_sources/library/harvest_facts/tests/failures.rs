use super::*;
fn edges(f: &Fixture, count: usize) {
    f.db().execute("WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<?1) INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,parent_file_size,parent_modified_ns,parent_content_hash,child_source_id,child_relative_path,child_file_size,child_modified_ns,child_content_hash,operation,tool_version,created_at) SELECT 'origin','one.wav',10,1,?2,'other','child-'||v||'.wav',10,1,?2,'copy','metadata-fixture',v FROM n",rusqlite::params![count as i64,"a".repeat(64)]).unwrap();
}
#[test]
fn harvest_facts_owner_exact_derivative_counts_and_finite_incident_edge_budget() {
    let f = Fixture::new();
    edges(&f, 2);
    assert_eq!(
        f.facts().unwrap().entries()[0].immediate_derivative_count(),
        2
    );
    f.db()
        .execute("DELETE FROM harvest_derivations", [])
        .unwrap();
    edges(&f, 64);
    assert_eq!(
        f.facts().unwrap().entries()[0].immediate_derivative_count(),
        64
    );
    edges(&f, 1);
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsMalformed)
    ));
}
#[test]
fn harvest_facts_owner_malformed_state_identity_and_oversized_fields_fail_closed() {
    let f = Fixture::new();
    f.apply(f.authored_request(HarvestState::Done)).unwrap();
    f.db()
        .execute("UPDATE harvest_files SET harvest_state='unknown'", [])
        .unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsMalformed)
    ));
    f.db()
        .execute(
            "UPDATE harvest_files SET harvest_state='done',content_hash=NULL",
            [],
        )
        .unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsIdentityUncertain)
    ));
    f.db()
        .execute("UPDATE harvest_files SET note=?1", ["x".repeat(4097)])
        .unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsMalformed)
    ));
}
#[test]
fn harvest_facts_owner_uncertain_and_ambiguous_graph_identities_fail_closed() {
    let f = Fixture::new();
    edges(&f, 1);
    f.db()
        .execute(
            "UPDATE harvest_derivations SET parent_content_hash=NULL",
            [],
        )
        .unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsIdentityUncertain)
    ));
    f.db()
        .execute(
            "UPDATE harvest_derivations SET parent_content_hash=?1,operation='unknown'",
            ["a".repeat(64)],
        )
        .unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsIdentityUncertain)
    ));
    f.db().execute_batch("UPDATE harvest_derivations SET operation='copy'; ALTER TABLE harvest_derivations RENAME TO saved_edges; CREATE TABLE harvest_derivations AS SELECT * FROM saved_edges; INSERT INTO harvest_derivations SELECT * FROM saved_edges;").unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsIdentityUncertain)
    ));
}
#[test]
fn harvest_facts_owner_stale_file_ended_source_and_foreign_profile_rejected() {
    let mut f = Fixture::new();
    let request = f
        .facts_request(&["one.wav"], HarvestFactsCancellation::new())
        .unwrap();
    f.source_db()
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    assert!(matches!(
        f.read(request),
        Err(LibraryError::HarvestFactsSourceChanged)
    ));
    let observation = f.observations(&["one.wav"]);
    let _foreign = PersistenceProfileGuard::named("foreign-harvest-facts");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert!(matches!(
        HarvestFactsRequest::new(&guard, observation, HarvestFactsCancellation::new()),
        Err(LibraryError::HarvestFactsStaleOwner)
    ));
    drop(guard);
    drop(_foreign);
    let request = f
        .facts_request(&["one.wav"], HarvestFactsCancellation::new())
        .unwrap();
    f.source
        .take()
        .unwrap()
        .into_catalog_database()
        .close()
        .unwrap();
    assert!(matches!(
        f.read(request),
        Err(LibraryError::HarvestFactsSourceChanged)
    ));
}
#[test]
fn harvest_facts_owner_missing_indexes_cannot_escape_actual_sqlite_work_budget() {
    let f = Fixture::new();
    f.db().execute_batch("DROP INDEX idx_harvest_derivations_parent; DROP INDEX idx_harvest_derivations_child; WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<200000) INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,child_source_id,child_relative_path,operation,tool_version,created_at) SELECT 'unrelated','p'||v,'other','c'||v,'copy','budget-fixture',v FROM n;").unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsQueryBudget)
    ));
    // Progress handlers are removed even on error, so subsequent legitimate reads can recover.
    f.db()
        .execute("DELETE FROM harvest_derivations", [])
        .unwrap();
    assert!(f.facts().is_ok());
}
#[test]
fn harvest_facts_owner_ambiguous_file_rows_and_stale_audited_revision_fail_closed() {
    let f = Fixture::new();
    f.apply(f.authored_request(HarvestState::Done)).unwrap();
    f.db().execute_batch("ALTER TABLE harvest_files RENAME TO saved_files; CREATE TABLE harvest_files AS SELECT * FROM saved_files; INSERT INTO harvest_files SELECT * FROM saved_files;").unwrap();
    assert!(matches!(
        f.facts(),
        Err(LibraryError::HarvestFactsMalformed)
    ));
    f.db().execute("DELETE FROM harvest_files", []).unwrap();
    let request = f
        .facts_request(&["one.wav"], HarvestFactsCancellation::new())
        .unwrap();
    f.source_db()
        .execute(
            "UPDATE metadata SET value=CAST(value AS INTEGER)+1 WHERE key='revision'",
            [],
        )
        .unwrap();
    assert!(matches!(
        f.read(request),
        Err(LibraryError::HarvestFactsSourceChanged)
    ));
}
