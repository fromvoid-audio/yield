use super::*;
use rusqlite::{Connection, params};

#[test]
fn attempt_scalar_projection_preserves_required_and_nullable_driver_errors() {
    let c = Connection::open_in_memory().unwrap();
    for sql in [
        "SELECT NULL",
        "SELECT 7",
        "SELECT x'6162'",
        "SELECT CAST(x'ff' AS TEXT)",
    ] {
        let required = c
            .query_row(sql, [], |r| copied_required_text(r, 0))
            .unwrap_err();
        let actual = c.query_row(sql, [], |r| required_uuid(r, 0)).err().unwrap();
        assert_eq!(format!("{required:?}"), format!("{actual:?}"));
        let old = c.query_row(sql, [], |r| copied_nullable_text(r, 0));
        let new = c.query_row(sql, [], |r| nullable_uuid(r, 0));
        match (old, new) {
            (Ok(None), Ok(None)) => {}
            (Err(a), Err(b)) => assert_eq!(format!("{a:?}"), format!("{b:?}")),
            _ => panic!("changed original nullable conversion"),
        }
    }
}
#[test]
fn attempt_scalar_projection_preserves_forms_without_owned_payload() {
    let c = Connection::open_in_memory().unwrap();
    let id = Uuid::new_v4();
    for text in [
        id.to_string(),
        id.to_string().to_uppercase(),
        id.simple().to_string(),
        id.urn().to_string(),
        Uuid::nil().to_string(),
        String::new(),
        format!("{id}\0{}", "x".repeat(1024 * 1024)),
    ] {
        super::super::SCALAR_COPIED_BYTES.with(|b| b.set(0));
        let value = c
            .query_row("SELECT ?1", [&text], |r| required_uuid(r, 0))
            .unwrap();
        let original = Uuid::parse_str(&text);
        assert_eq!(format!("{:?}", value.id), format!("{original:?}"));
        assert_eq!(
            value.canonical,
            original.as_ref().is_ok_and(|i| i.to_string() == text)
        );
        assert_eq!(value.matches(id), text == id.to_string());
        assert_eq!(super::super::SCALAR_COPIED_BYTES.with(|b| b.get()), 0);
    }
}
#[test]
fn attempt_scalar_projection_defers_page_uuid_before_later_driver_failure() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE p(n INTEGER, id);").unwrap();
    c.execute(
        "INSERT INTO p VALUES(0,?1)",
        [format!("{}\0{}", "z".repeat(36), "x".repeat(1024 * 1024))],
    )
    .unwrap();
    for bad in [vec![0xff], vec![0x61]] {
        c.execute("DELETE FROM p WHERE n=1", []).unwrap();
        c.execute("INSERT INTO p VALUES(1,?1)", params![bad])
            .unwrap();
        let old = c
            .prepare("SELECT id FROM p ORDER BY n")
            .unwrap()
            .query_map([], |r| copied_nullable_text(r, 0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_err();
        let new = c
            .prepare("SELECT id FROM p ORDER BY n")
            .unwrap()
            .query_map([], |r| nullable_uuid(r, 0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .err()
            .unwrap();
        assert_eq!(format!("{old:?}"), format!("{new:?}"));
    }
}

#[test]
fn attempt_scalar_projection_keeps_image_before_column_semantics() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE operation_journal_database_attempt(attempt_id TEXT,operation_id,binding_id,start_nonce,database_slice,sidecar_slice,database_bytes,wal_bytes,shm_bytes,image);").unwrap();
    let id = Uuid::new_v4();
    let text = id.to_string();
    c.execute(
        "INSERT INTO operation_journal_database_attempt VALUES(?1,?1,?1,?1,?1,?1,1,0,0,x'00')",
        [&text],
    )
    .unwrap();
    let image_error = super::super::read_start(&c, id).unwrap_err();
    c.execute(
        "UPDATE operation_journal_database_attempt SET operation_id=?1",
        [format!("{}\0{}", "z".repeat(36), "x".repeat(1024 * 1024))],
    )
    .unwrap();
    let malformed = super::super::read_start(&c, id).unwrap_err();
    assert_eq!(format!("{image_error:?}"), format!("{malformed:?}"));
    c.execute(
        "UPDATE operation_journal_database_attempt SET binding_id=CAST(?1 AS TEXT)",
        params![[vec![b'z'; 36], vec![0xff]].concat()],
    )
    .unwrap();
    let original=c.query_row("SELECT CASE WHEN length(binding_id)=36 THEN binding_id END FROM operation_journal_database_attempt",[],|r|r.get::<_,String>(0)).unwrap_err();
    let actual = super::super::read_start(&c, id).unwrap_err();
    assert!(
        format!("{actual:?}").contains(&format!("{original:?}")),
        "{actual:?} vs {original:?}"
    );
}
