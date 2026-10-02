use super::*;
fn fill(c: &Connection, bytes: bool) {
    if bytes {
        c.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<256) INSERT INTO harvest_copy_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||x,zeroblob(65536),x'7b7d',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    } else {
        c.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO harvest_copy_receipts(command_key,payload,result,identity,owner) SELECT 'capacity-'||x,x'7b7d',x'7b7d',zeroblob(32),'00000000-0000-0000-0000-000000000001' FROM n;").unwrap();
    }
}
#[test]
fn harvest_copy_owner_row_and_byte_capacity_preserve_exact_replay() {
    for bytes in [false, true] {
        let f = Fixture::new();
        let r = f.request();
        fill(&f.db(), bytes);
        assert!(matches!(
            f.apply(r.clone()),
            Err(LibraryError::HarvestCopyReceiptCapacity)
        ));
        assert_eq!(f.count("harvest_derivations"), 0);
        f.db()
            .execute("DELETE FROM harvest_copy_receipts", [])
            .unwrap();
        let original = f.apply(r.clone()).unwrap();
        fill(&f.db(), bytes);
        let replay = f.apply(r).unwrap();
        assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
        assert_eq!(replay.receipt().sequence(), original.receipt().sequence());
        assert_eq!(replay.receipt().edge_id(), original.receipt().edge_id());
        assert_eq!(f.count("harvest_derivations"), 1);
    }
}
