// Actual reference owner mutation invalidates bounded Harvest read publication.

#[test]
fn harvest_facts_owner_actual_reference_rekey_invalidates_weak_read_generation() {
    use super::super::super::{HarvestFactsRequest,HarvestFactsCancellation};
    let f=ReferenceFixture::new();
    let observation=f.origin.as_ref().unwrap().observe_file_identity(StoredRelativePath::new("one.wav").unwrap()).unwrap();
    let before=f.global().read_harvest_facts(HarvestFactsRequest::new(f.guard.as_ref().unwrap(),vec![observation],HarvestFactsCancellation::new()).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    f.seed();let result=f.apply(f.request(9090)).unwrap();assert_eq!(result.receipt().disposition(),ReferenceDisposition::Applied);
    assert!(!before.is_current_global_generation());
}
