use super::*;
use crate::operation_journal::{
    ParticipantId,
    source_layout_commands::{
        MAX_SOURCE_LAYOUT_INTENT_BYTES, SourceLayoutIntentArgs, SourceLayoutIntentError,
        StoredSourceLayoutIntent, decode_source_layout_intent,
    },
};
use crate::sample_sources::db::SourceCommandId;
use uuid::Uuid;

macro_rules! case {
    ($name:ident, $case:literal) => {
        #[test]
        fn $name() {
            super::super::super::tag_metadata_budget::run_case($case);
        }
    };
}
case!(
    source_layout_intent_owner_readonly_roundtrip,
    "layout-intent-owner"
);
case!(
    source_layout_durable_family_restart_owner,
    "layout-intent-durable"
);
case!(
    source_layout_durable_maximum_manifest,
    "layout-intent-durable-max"
);
case!(source_layout_intent_wire_rejections, "layout-intent-wire");
case!(source_layout_intent_maximum_bounds, "layout-intent-max");
case!(
    source_layout_intent_cancel_and_original_deadline,
    "layout-intent-deadline"
);
case!(
    source_layout_intent_changed_source_and_budget,
    "layout-intent-stale"
);
case!(
    source_layout_intent_handoff_and_foreign_owner,
    "layout-intent-handoff"
);
case!(
    source_layout_intent_replaced_owner_namespaces,
    "layout-intent-replaced"
);
case!(
    source_layout_intent_changed_page_policy,
    "layout-intent-pages"
);
case!(
    source_layout_intent_restart_is_historical,
    "layout-intent-restart"
);

fn intent(
    writer: &SourceWriter,
    p: &SourcePreparedLayoutGeneration,
    b: &crate::sample_sources::db::SourceLayoutMetadataBudget,
) -> Result<SourceLayoutIntentArgs, SourceLayoutIntentError> {
    SourceLayoutIntentArgs::from_prepared(
        writer,
        p,
        b,
        ParticipantId::from_uuid(Uuid::from_u128(100)),
        SourceCommandId::from_uuid(Uuid::from_u128(101)),
        &AtomicBool::new(false),
    )
}
pub(super) fn dispatch(case: &str) {
    if matches!(case, "layout-intent-max" | "layout-intent-durable-max") {
        let (fixture, writer) = setup_max_manifest();
        let p = prepare(&writer, &capture(&writer).unwrap()).unwrap();
        let b = writer.capture_layout_metadata_budget(p.revision()).unwrap();
        let args = intent(&writer, &p, &b).unwrap();
        assert_eq!(
            SourceLayoutGeneration::decode(args.generation_bytes())
                .unwrap()
                .rows()
                .len(),
            256
        );
        assert_eq!(
            decode_source_layout_intent(&args.canonical_bytes().unwrap()).unwrap(),
            StoredSourceLayoutIntent::Supported(args.clone())
        );
        if case == "layout-intent-durable-max" {
            crate::operation_journal::test_source_layout_durable(
                args.clone(),
                &fixture.profile_guard,
            );
        }
        let mut opaque = vec![0; MAX_SOURCE_LAYOUT_INTENT_BYTES];
        opaque[..4].copy_from_slice(b"YSLI");
        opaque[4..12].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(
            decode_source_layout_intent(&opaque).unwrap(),
            StoredSourceLayoutIntent::Unsupported {
                version: 2,
                bytes: opaque.clone()
            }
        );
        opaque.push(0);
        assert_eq!(
            decode_source_layout_intent(&opaque),
            Err(SourceLayoutIntentError::Bytes)
        );
        assert!(4 * MAX_SOURCE_LAYOUT_INTENT_BYTES.div_ceil(3) + 65536 < 1024 * 1024);
        return;
    }
    let (fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let p = prepare(&writer, &capture(&writer).unwrap()).unwrap();
    // Exact prior canonical bytes are fixture data, not production publication.
    writer
        .database_for_tests()
        .connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('source_starmap_generation_v1',?1)",
            [p.bytes()],
        )
        .unwrap();
    let b = writer.capture_layout_metadata_budget(p.revision()).unwrap();
    let before = persisted(&writer);
    let changes = writer.database_for_tests().connection.total_changes();
    let control = writer
        .database_for_tests()
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap();
    let args = intent(&writer, &p, &b).unwrap();
    let bytes = args.canonical_bytes().unwrap();
    match case {
        "layout-intent-durable" => crate::operation_journal::test_source_layout_durable(
            args.clone(),
            &fixture.profile_guard,
        ),
        "layout-intent-owner" => {
            writer
                .database_for_tests()
                .connection
                .pragma_update(None, "query_only", true)
                .unwrap();
            assert_eq!(intent(&writer, &p, &b).unwrap(), args);
            assert_eq!(
                decode_source_layout_intent(&bytes).unwrap(),
                StoredSourceLayoutIntent::Supported(args.clone())
            );
            assert_eq!(args.generation_bytes(), p.bytes());
            assert_eq!(args.scope().source_id(), p.revision().source_id().as_str());
            assert_eq!(args.scope().lease_epoch(), p.revision().lease_epoch());
            assert_eq!(
                args.allocation().prior_generation_digest(),
                Some(*p.bytes_digest())
            );
        }
        "layout-intent-wire" => wire(&bytes),
        "layout-intent-pages" => {
            writer
                .database_for_tests()
                .connection
                .pragma_update(None, "max_page_count", b.maximum_pages() - 1)
                .unwrap();
            let after = persisted(&writer);
            assert_eq!(
                intent(&writer, &p, &b),
                Err(SourceLayoutIntentError::Observation)
            );
            assert_eq!(persisted(&writer), after);
            return;
        }
        "layout-intent-replaced" => {
            for path in [
                fixture._source.path().to_path_buf(),
                writer.database_for_tests().db_path.clone(),
                fixture.profile_guard.profile_root().to_path_buf(),
            ] {
                let saved = path.with_extension("intent-saved");
                let file = path.is_file();
                std::fs::rename(&path, &saved).unwrap();
                if file {
                    std::fs::write(&path, b"replacement").unwrap();
                } else {
                    std::fs::create_dir(&path).unwrap();
                }
                assert_eq!(
                    intent(&writer, &p, &b),
                    Err(SourceLayoutIntentError::Observation)
                );
                if file {
                    std::fs::remove_file(&path).unwrap();
                } else {
                    std::fs::remove_dir(&path).unwrap();
                }
                std::fs::rename(saved, &path).unwrap();
            }
        }
        "layout-intent-deadline" => {
            assert_eq!(
                SourceLayoutIntentArgs::from_prepared(
                    &writer,
                    &p,
                    &b,
                    args.participant_id(),
                    args.source_command_id(),
                    &AtomicBool::new(true)
                ),
                Err(SourceLayoutIntentError::Cancelled)
            );
            std::thread::sleep(Duration::from_millis(2050));
            assert_eq!(
                intent(&writer, &p, &b),
                Err(SourceLayoutIntentError::Deadline)
            );
        }
        "layout-intent-stale" => {
            writer
                .database_for_tests()
                .connection
                .execute(
                    "UPDATE metadata SET value=?1 WHERE key='source_starmap_generation_v1'",
                    [b"changed".as_slice()],
                )
                .unwrap();
            assert_eq!(
                intent(&writer, &p, &b),
                Err(SourceLayoutIntentError::Observation)
            );
            writer
                .database_for_tests()
                .connection
                .execute(
                    "UPDATE metadata SET value=?1 WHERE key='source_starmap_generation_v1'",
                    [p.bytes()],
                )
                .unwrap();
            writer
                .commit_catalog_curation_command(
                    p.revision(),
                    &fixture.profile_guard,
                    &[SourceCurationUpdate::SetRating {
                        path: CurationFixture::path("one.wav"),
                        rating: Rating::KEEP_3,
                    }],
                )
                .unwrap();
            let after = persisted(&writer);
            assert_eq!(
                intent(&writer, &p, &b),
                Err(SourceLayoutIntentError::Observation)
            );
            assert_eq!(persisted(&writer), after);
            return;
        }
        "layout-intent-handoff" => {
            let (_other_fixture, mut other) = setup();
            publish(&mut other, &["one.wav", "two.wav"]);
            let other_before = persisted(&other);
            assert_eq!(
                intent(&other, &p, &b),
                Err(SourceLayoutIntentError::Observation)
            );
            assert_eq!(persisted(&other), other_before);
            let next = SourceWriter::new(
                writer.into_catalog_database(),
                SourceId::from_string("layout-features"),
                7,
            )
            .unwrap();
            let after = persisted(&next);
            assert_eq!(
                intent(&next, &p, &b),
                Err(SourceLayoutIntentError::Observation)
            );
            assert_eq!(persisted(&next), after);
            return;
        }
        "layout-intent-restart" => {
            let path = fixture._source.path().join("intent-fixture.bin");
            std::fs::write(&path, &bytes).unwrap();
            writer.close().unwrap();
            let database = SourceDatabase::open_for_catalog_with_profile_guard(
                fixture._source.path(),
                &fixture.profile_guard,
                Path::new("curation-profile-bound"),
            )
            .unwrap();
            let next =
                SourceWriter::new(database, SourceId::from_string("layout-features"), 8).unwrap();
            assert_eq!(
                decode_source_layout_intent(&std::fs::read(path).unwrap()).unwrap(),
                StoredSourceLayoutIntent::Supported(args.clone())
            );
            assert_eq!(
                intent(&next, &p, &b),
                Err(SourceLayoutIntentError::Observation)
            );
            let fresh = prepare(&next, &capture(&next).unwrap()).unwrap();
            let fresh_budget = next
                .capture_layout_metadata_budget(fresh.revision())
                .unwrap();
            let new = intent(&next, &fresh, &fresh_budget).unwrap();
            assert_ne!(new.scope().lease_epoch(), args.scope().lease_epoch());
            return;
        }
        _ => panic!("unknown intent fixture"),
    }
    assert_eq!(persisted(&writer), before);
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(
        writer
            .database_for_tests()
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap(),
        control
    );
}
fn seal(bytes: &mut [u8]) {
    let end = bytes.len() - 32;
    let mut h = blake3::Hasher::new();
    h.update(b"yield.source-layout-intent.v1\0");
    h.update(&bytes[..end]);
    bytes[end..].copy_from_slice(h.finalize().as_bytes());
}
fn wire(bytes: &[u8]) {
    let mut bad_header = bytes.to_vec();
    bad_header[0] ^= 1;
    assert!(decode_source_layout_intent(&bad_header).is_err());
    bad_header = bytes.to_vec();
    bad_header[4..12].fill(0);
    assert!(decode_source_layout_intent(&bad_header).is_err());
    for n in 0..bytes.len() {
        assert!(
            decode_source_layout_intent(&bytes[..n]).is_err(),
            "truncation {n}"
        );
    }
    let mut corrupt = bytes.to_vec();
    corrupt[13] ^= 1;
    assert!(decode_source_layout_intent(&corrupt).is_err());
    let mut scalar = 60;
    for _ in 0..4 {
        let n = u16::from_le_bytes(bytes[scalar..scalar + 2].try_into().unwrap()) as usize;
        scalar += 2 + n;
    }
    for offset in [12, 28, 44, scalar, scalar + 16, scalar + 24, scalar + 80] {
        let mut bad = bytes.to_vec();
        let len = if offset < 60 { 16 } else { 8 };
        bad[offset..offset + len].fill(0);
        seal(&mut bad);
        assert!(
            decode_source_layout_intent(&bad).is_err(),
            "invalid identity/arithmetic {offset}"
        );
    }
    let mut ambiguous = bytes.to_vec();
    ambiguous[scalar + 88] = 2;
    seal(&mut ambiguous);
    assert!(decode_source_layout_intent(&ambiguous).is_err());
    let mut trailing = bytes.to_vec();
    trailing.insert(trailing.len() - 32, 0);
    seal(&mut trailing);
    assert!(decode_source_layout_intent(&trailing).is_err());
    let mut oversize = bytes.to_vec();
    oversize[60..62].copy_from_slice(&257u16.to_le_bytes());
    seal(&mut oversize);
    assert_eq!(
        decode_source_layout_intent(&oversize),
        Err(SourceLayoutIntentError::Bytes)
    );
    let mut future = bytes.to_vec();
    future[4..12].copy_from_slice(&99u64.to_le_bytes());
    assert_eq!(
        decode_source_layout_intent(&future).unwrap(),
        StoredSourceLayoutIntent::Unsupported {
            version: 99,
            bytes: future
        }
    );
}
