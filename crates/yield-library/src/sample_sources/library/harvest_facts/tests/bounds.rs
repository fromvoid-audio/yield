use super::*;
#[test]
fn harvest_facts_owner_request_path_count_unique_path_and_total_input_budget() {
    let f = Fixture::new();
    let obs = f.observations(&["one.wav"]);
    let cancel = HarvestFactsCancellation::new();
    assert!(HarvestFactsRequest::new(f.guard(), vec![], cancel.clone()).is_err());
    assert!(
        HarvestFactsRequest::new(f.guard(), vec![obs[0].clone(); 129], cancel.clone()).is_err()
    );
    assert!(HarvestFactsRequest::new(f.guard(), vec![obs[0].clone(); 2], cancel).is_err());
    drop(f);
    let paths: Vec<_> = (0..80)
        .map(|i| format!("{i}-{}.wav", "x".repeat(700)))
        .collect();
    let f = Fixture::with_paths(paths.clone());
    let refs: Vec<_> = paths.iter().map(String::as_str).collect();
    assert!(
        f.facts_request(&refs, HarvestFactsCancellation::new())
            .is_err()
    );
    let one = f
        .facts_request(&refs[..1], HarvestFactsCancellation::new())
        .unwrap();
    assert_eq!(f.read(one).unwrap().entries().len(), 1);
}
#[test]
fn harvest_facts_owner_output_encoding_is_capped_before_delivery() {
    let paths: Vec<_> = (0..32)
        .map(|i| format!("{i}-{}.wav", "\"".repeat(1015)))
        .collect();
    let f = Fixture::with_paths(paths.clone());
    let refs: Vec<_> = paths.iter().map(String::as_str).collect();
    let request = f
        .facts_request(&refs, HarvestFactsCancellation::new())
        .unwrap();
    assert!(matches!(
        f.read(request),
        Err(LibraryError::HarvestFactsInput)
    ));
}

#[test]
fn harvest_facts_owner_complete_source_fact_input_budget_and_index_only_rejection() {
    let paths: Vec<_> = (0..100).map(|i| format!("short-{i}.wav")).collect();
    let f = Fixture::with_paths(paths.clone());
    f.source_db()
        .execute("UPDATE wav_files SET file_identity=?1", ["z".repeat(512)])
        .unwrap();
    let refs: Vec<_> = paths.iter().map(String::as_str).collect();
    assert!(matches!(
        f.facts_request(&refs, HarvestFactsCancellation::new()),
        Err(LibraryError::HarvestFactsInput)
    ));
    f.source_db().execute_batch("INSERT INTO source_index_entries(path,path_encoding,classification,file_size,modified_ns,file_identity,format_policy_version) VALUES('plain.txt',0,'unsupported_non_audio',3,2,'index-object',1);").unwrap();
    assert!(matches!(
        f.facts_request(&["plain.txt"], HarvestFactsCancellation::new()),
        Err(LibraryError::HarvestFactsInput)
    ));
    assert!(
        f.source()
            .observe_file_identity(StoredRelativePath::new("missing.wav").unwrap())
            .is_err()
    );
}
