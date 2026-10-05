use super::*;
#[path = "historical.rs"]
mod historical;
fn capture(c: &Connection) -> Result<Rows> {
    super::capture(c, Instant::now() + std::time::Duration::from_secs(2))
}

fn fixture() -> Connection {
    fixture_encoding("UTF-8")
}
fn fixture_encoding(encoding: &str) -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "encoding", encoding).unwrap();
    c.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE wav_files (path TEXT PRIMARY KEY, file_size INTEGER NOT NULL, modified_ns INTEGER NOT NULL); PRAGMA user_version=11; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; INSERT INTO metadata VALUES ('revision','7');").unwrap();
    c
}
#[test]
fn individual_text_boundaries_and_canonical_revision_extremes_match_historical_semantics() {
    for revision in ["0", "9223372036854775807"] {
        let c = fixture();
        c.execute(
            "UPDATE metadata SET value=?1 WHERE key='revision'",
            [revision],
        )
        .unwrap();
        c.execute(
            "INSERT INTO metadata VALUES (?1,'bounded key')",
            ["k".repeat(4096)],
        )
        .unwrap();
        c.execute("INSERT INTO wav_files VALUES (?1,0,-1)", ["p".repeat(4096)])
            .unwrap();
        assert!(compare(&c).is_ok());
        c.execute("UPDATE wav_files SET path=path||'p'", [])
            .unwrap();
        assert!(compare(&c).is_err());
    }
    for sql in [
        "INSERT INTO metadata VALUES (printf('%.*c',4097,'k'),'oversized key')",
        "INSERT INTO metadata VALUES ('value',printf('%.*c',65536,'v'))",
    ] {
        let c = fixture();
        c.execute_batch(sql).unwrap();
        assert!(compare(&c).is_err());
    }
}
fn compare(c: &Connection) -> Result<Rows> {
    let new = capture(c);
    let old = historical::capture(c);
    match (&new, &old) {
        (Ok(n), Ok(o)) => {
            assert_eq!(
                (n.revision, n.metadata_rows, n.audio_rows, n.digest),
                (o.revision, o.metadata_rows, o.audio_rows, o.digest)
            );
        }
        (Err(_), Err(_)) => (),
        _ => panic!("streaming and historical refusal differ: {new:?} / {old:?}"),
    }
    new
}
#[test]
fn canonical_stream_matches_historical_utf8_tuple_order_and_exact_non_sorting_plans() {
    let c = fixture();
    for (key, value) in [
        ("é", "非"),
        ("z", "last"),
        ("a\0b", "nul metadata is historical text"),
        ("a", "first"),
    ] {
        c.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    for (path, size, modified) in [
        ("é/x", 4, -8),
        ("z", 0, i64::MAX),
        ("a/b", 10, 0),
        ("a", 2, i64::MIN),
    ] {
        c.execute(
            "INSERT INTO wav_files VALUES (?1,?2,?3)",
            rusqlite::params![path, size, modified],
        )
        .unwrap();
    }
    c.pragma_update(None, "query_only", true).unwrap();
    for index in [
        "sqlite_autoindex_metadata_1",
        "sqlite_autoindex_wav_files_1",
    ] {
        let collation: String = c
            .query_row(
                &format!("SELECT coll FROM pragma_index_xinfo('{index}') WHERE key=1"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(collation, "BINARY");
    }
    assert_eq!(compare(&c).unwrap().audio_rows, 4);
    verify_plan(
        &c,
        METADATA_PLAN,
        "SCAN metadata USING INDEX sqlite_autoindex_metadata_1",
    )
    .unwrap();
    verify_plan(
        &c,
        AUDIO_PLAN,
        "SCAN wav_files USING INDEX sqlite_autoindex_wav_files_1",
    )
    .unwrap();
    assert!(
        verify_plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT key FROM metadata ORDER BY value",
            "SCAN metadata USING INDEX sqlite_autoindex_metadata_1"
        )
        .is_err()
    );
}
#[test]
fn original_maximum_row_and_byte_inputs_keep_identical_digest_without_owned_audio_paths() {
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        let c = fixture_encoding(encoding);
        let header = std::mem::size_of::<(String, String)>();
        let revision = "revision".len() + 1 + header;
        let remaining_values = 65536 - revision - 127 * (4 + header);
        for n in 0..127 {
            let value_len =
                remaining_values / 127 + if n == 0 { remaining_values % 127 } else { 0 };
            c.execute(
                "INSERT INTO metadata VALUES (?1,?2)",
                rusqlite::params![format!("m{n:03}"), "v".repeat(value_len)],
            )
            .unwrap();
        }
        let path_len = 4 * 1024 * 1024 / 1024 - std::mem::size_of::<(String, i64, i64)>();
        for n in (0..1024).rev() {
            let length = path_len + if n == 0 { 40 } else { 0 } - if n == 1 { 40 } else { 0 };
            let path = format!(
                "{n:04}/{}{}",
                "\u{e000}".repeat((length - 5) / 3),
                "x".repeat((length - 5) % 3)
            );
            assert_eq!(path.len(), length);
            c.execute(
                "INSERT INTO wav_files VALUES (?1,?2,?3)",
                rusqlite::params![path, n as i64, -(n as i64)],
            )
            .unwrap();
        }
        let result = compare(&c).unwrap();
        assert_eq!((result.metadata_rows, result.audio_rows), (128, 1024));
        c.execute(
            "UPDATE metadata SET value=value || 'x' WHERE key='m000'",
            [],
        )
        .unwrap();
        assert!(compare(&c).is_err());
        c.execute(
            "UPDATE metadata SET value=substr(value,1,length(value)-1) WHERE key='m000'",
            [],
        )
        .unwrap();
        c.execute(
            "UPDATE wav_files SET path=path || 'x' WHERE file_size=2",
            [],
        )
        .unwrap();
        assert!(compare(&c).is_err());
    }
}
#[test]
fn borrowed_stream_preserves_historical_null_storage_utf8_nul_path_and_revision_refusals() {
    for sql in [
        "INSERT INTO metadata VALUES (NULL,'null key')",
        "INSERT INTO metadata VALUES (x'61','blob key')",
        "INSERT INTO metadata VALUES ('blob',x'ff')",
        "INSERT INTO metadata VALUES ('utf8',CAST(x'ff' AS TEXT))",
        "INSERT INTO metadata VALUES ('','empty')",
        "INSERT INTO metadata VALUES ('source.legacy_adoption.v1','reserved')",
        "UPDATE metadata SET value='07' WHERE key='revision'",
        "UPDATE metadata SET value='9223372036854775808' WHERE key='revision'",
        "INSERT INTO wav_files VALUES (NULL,1,0)",
        "INSERT INTO wav_files VALUES (x'61',1,0)",
        "INSERT INTO wav_files VALUES (CAST(x'ff' AS TEXT),1,0)",
        "INSERT INTO wav_files VALUES ('../escape',1,0)",
        "INSERT INTO wav_files VALUES ('bad' || char(0) || 'path',1,0)",
        "INSERT INTO wav_files VALUES ('negative',-1,0)",
        "INSERT INTO wav_files VALUES ('real',1.5,0)",
        "INSERT INTO wav_files VALUES ('mtime',1,0.5)",
    ] {
        let c = fixture();
        c.execute_batch(sql).unwrap();
        assert!(compare(&c).is_err(), "{sql}");
    }
}
#[test]
fn scalar_duplicate_audio_groups_keep_historical_tuple_and_duplicate_order() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE wav_files (path TEXT, file_size INTEGER, modified_ns INTEGER)")
        .unwrap();
    let values = [(4, -9), (0, 1), (4, -10), (0, 1), (i64::MAX, i64::MIN)];
    let mut refs = Vec::new();
    for (s, m) in values {
        c.execute(
            "INSERT INTO wav_files VALUES ('é/path',?1,?2)",
            rusqlite::params![s, m],
        )
        .unwrap();
        refs.push((c.last_insert_rowid(), s, m));
    }
    let mut old = values
        .iter()
        .map(|(s, m)| ("é/path".to_owned(), *s, *m))
        .collect::<Vec<_>>();
    old.sort();
    let mut expected = blake3::Hasher::new();
    for (p, s, m) in old {
        hash_text(&mut expected, &p);
        expected.update(&s.to_le_bytes());
        expected.update(&m.to_le_bytes());
    }
    let mut left = c.prepare(AUDIO_LOOKUP).unwrap();
    let mut right = c.prepare(AUDIO_LOOKUP).unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    sort_references(&mut refs, |a, b| {
        compare_audio(&mut left, &mut right, a, b, deadline)
    })
    .unwrap();
    let mut actual = blake3::Hasher::new();
    for (_, s, m) in refs {
        hash_text(&mut actual, "é/path");
        actual.update(&s.to_le_bytes());
        actual.update(&m.to_le_bytes());
    }
    assert_eq!(actual.finalize(), expected.finalize());
    assert!(compare_text(&mut left, &mut right, -42, 1, deadline).is_err());
    assert!(super::capture(&fixture(), Instant::now()).is_err());
    let mut input = [3, 1, 2];
    let mut calls = 0;
    assert!(
        sort_references(&mut input, |_, _| {
            calls += 1;
            Err(LegacyObservationError("comparison refusal"))
        })
        .is_err()
    );
    assert_eq!(
        calls, 1,
        "fallible ordering must return at the first refusal"
    );
}
#[test]
fn complete_counts_detect_lookahead_and_do_not_accept_an_unqualified_index_or_schema() {
    for (table, rows) in [("metadata", 129), ("wav_files", 1025)] {
        let c = fixture();
        for n in 0..rows {
            if table == "metadata" {
                c.execute("INSERT INTO metadata VALUES (?1,'x')", [format!("key{n}")])
                    .unwrap();
            } else {
                c.execute(
                    "INSERT INTO wav_files VALUES (?1,1,0)",
                    [format!("path{n}")],
                )
                .unwrap();
            }
        }
        assert!(compare(&c).is_err());
    }
    let c = fixture();
    c.execute_batch("DROP TABLE wav_files; CREATE TABLE wav_files (path TEXT, file_size INTEGER NOT NULL, modified_ns INTEGER NOT NULL)").unwrap();
    assert!(capture(&c).is_err());
    c.execute_batch("CREATE INDEX fake_index ON wav_files(path COLLATE NOCASE)")
        .unwrap();
    assert!(capture(&c).is_err());
}

#[test]
fn every_original_encoding_keeps_rust_utf8_order_and_encoded_key_limits() {
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        let c = fixture_encoding(encoding);
        for key in ["a", "é", "Ā", "\u{e000}", "𐀀", "a\0b"] {
            c.execute("INSERT INTO metadata VALUES (?1,'preserved')", [key])
                .unwrap();
        }
        for path in ["a", "é", "Ā", "\u{e000}", "𐀀"] {
            c.execute("INSERT INTO wav_files VALUES (?1,1,-1)", [path])
                .unwrap();
        }
        compare(&c).unwrap();
        if encoding != "UTF-8" {
            let key = "\u{e000}".repeat(2048);
            assert_eq!(key.len(), 6144);
            c.execute(
                "INSERT INTO metadata VALUES (?1,'accepted encoded boundary')",
                [key.clone()],
            )
            .unwrap();
            compare(&c).unwrap();
            c.execute("UPDATE metadata SET key=key||'x' WHERE key=?1", [key])
                .unwrap();
            assert!(compare(&c).is_err());
        }
    }
}
