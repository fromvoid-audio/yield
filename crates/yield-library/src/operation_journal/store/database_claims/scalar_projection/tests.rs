use super::*;
use rusqlite::Connection;

#[test]
fn scalar_projection_original_driver_types_utf8_and_nullable_errors_are_unchanged() {
    let c = Connection::open_in_memory().unwrap();
    for expression in ["NULL", "1", "1.5", "zeroblob(36)", "CAST(X'FF' AS TEXT)"] {
        c.query_row(&format!("SELECT {expression} AS original"), [], |row| {
            let original = copied_text(row, 0);
            let projected = uuid(row, 0);
            let compared = matches_text(row, 0, "x");
            match original {
                Ok(None) => {
                    assert!(projected.unwrap().is_none());
                    assert!(!compared.unwrap());
                }
                Err(error) => {
                    assert_eq!(projected.err().unwrap().to_string(), error.to_string());
                    assert_eq!(compared.err().unwrap().to_string(), error.to_string());
                }
                _ => panic!("unexpected original conversion"),
            }
            Ok(())
        })
        .unwrap();
    }
}

#[test]
fn scalar_projection_borrows_same_original_large_nul_and_uuid_forms() {
    let c = Connection::open_in_memory().unwrap();
    let id = Uuid::new_v4();
    for text in [
        String::new(),
        id.simple().to_string(),
        id.urn().to_string(),
        id.to_string(),
        id.to_string().to_uppercase(),
        Uuid::nil().to_string(),
        format!("{}\0{}", id, "x".repeat(1024 * 1024)),
    ] {
        c.query_row("SELECT ?1", [&text], |row| {
            super::super::SCALAR_COPIED_BYTES.with(|n| n.set(0));
            let projection = uuid(row, 0)?.unwrap();
            assert_eq!(projection.id, Uuid::parse_str(&text));
            assert_eq!(
                projection.canonical,
                Uuid::parse_str(&text).is_ok_and(|id| id.to_string() == text)
            );
            assert_eq!(
                matches_text(row, 0, &id.to_string())?,
                text == id.to_string()
            );
            super::super::SCALAR_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
            let original = copied_text(row, 0)?.unwrap();
            assert_eq!(original, text);
            super::super::SCALAR_COPIED_BYTES.with(|n| assert_eq!(n.get(), text.len()));
            Ok(())
        })
        .unwrap();
    }
}

#[test]
fn scalar_projection_keeps_later_page_driver_error_before_earlier_uuid_error() {
    let c = Connection::open_in_memory().unwrap();
    let large = format!("{}\0{}", Uuid::new_v4(), "x".repeat(1024 * 1024));
    for second in ["zeroblob(36)", "CAST(X'FF' AS TEXT)"] {
        let sql = format!("SELECT ?1 AS id UNION ALL SELECT {second}");
        let mut query = c.prepare(&sql).unwrap();
        let original = query
            .query_map([&large], |row| copied_text(row, 0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .err()
            .unwrap();
        super::super::SCALAR_COPIED_BYTES.with(|n| n.set(0));
        let mut query = c.prepare(&sql).unwrap();
        let projected = query
            .query_map([&large], |row| uuid(row, 0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .err()
            .unwrap();
        assert_eq!(projected.to_string(), original.to_string());
        super::super::SCALAR_COPIED_BYTES.with(|n| assert_eq!(n.get(), 0));
    }
}
