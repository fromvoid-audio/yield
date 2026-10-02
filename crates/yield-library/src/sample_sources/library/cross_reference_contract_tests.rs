// Existing compatibility-API evidence only, not the missing production participant.
use super::*;

#[test]
fn cross_reference_compatibility_replay_loses_result_and_can_consume_reused_old_key() {
    let temp = tempdir().unwrap();
    with_config_home(temp.path(), || {
        let old = HarvestFileKey::new(SourceId::from_string("origin"), PathBuf::from("one.wav"));
        let new = HarvestFileKey::new(SourceId::from_string("destination"), PathBuf::from("one.wav"));
        let identity = HarvestFileIdentity { key: old.clone(), file_size: Some(10), modified_ns: Some(1), content_hash: Some("old".into()) };
        mark_harvest_touched(&identity).unwrap();
        set_harvest_state(&old, HarvestState::Done).unwrap();
        let first = remap_harvest_file_key(&old, &new).unwrap();
        assert!(first > 0);
        assert_eq!(harvest_file(&new).unwrap().unwrap().state, HarvestState::Done);
        assert_eq!(remap_harvest_file_key(&old, &new).unwrap(), 0);
        // Later unrelated source content at the old key must not be consumed by a retry
        // of the earlier operation. The legacy helper has no operation receipt to stop it.
        mark_harvest_touched(&HarvestFileIdentity { content_hash: Some("later".into()), ..identity }).unwrap();
        set_harvest_state(&old, HarvestState::Ignored).unwrap();
        assert!(remap_harvest_file_key(&old, &new).unwrap() > 0);
        assert!(harvest_file(&old).unwrap().is_none());
        assert_eq!(harvest_file(&new).unwrap().unwrap().content_hash.as_deref(), Some("later"));
    });
}
