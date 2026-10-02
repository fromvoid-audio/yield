use super::*;
use crate::operation_journal::{
    MetadataTagSourceIntentArgs as Args, ParticipantId, StoredMetadataTagSourceIntent,
    decode_metadata_tag_source_intent,
};
use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
#[test]
fn metadata_tag_codec_actual_owner_roundtrip_and_sql_oracle() {
    tag_metadata_budget::run_case("metadata-tag-codec-owner");
}
#[test]
fn metadata_tag_codec_rejects_changed_selection_schema_and_owner_without_effects() {
    tag_metadata_budget::run_case("metadata-tag-codec-stale");
}
#[test]
fn metadata_tag_codec_actual_whole_set_boundaries() {
    tag_metadata_budget::run_case("metadata-tag-codec-bounds");
}
fn produce(
    writer: &SourceWriter,
    observation: &crate::sample_sources::db::SourceTagAdmissionObservation,
) -> Result<Args, crate::operation_journal::MetadataTagArgsError> {
    Args::from_observation(
        writer,
        observation,
        ParticipantId::from_uuid(uuid::Uuid::from_u128(100)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(101)),
    )
}
pub(super) fn owner() {
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old tag", "Old Tag");
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("metadata-tag-codec"),
        7,
    )
    .unwrap();
    let rev = writer.current_revision().unwrap();
    let db = writer.database_for_tests();
    let before = source_writer_state_snapshot(db);
    let changes = db.connection.total_changes();
    for label in [
        "loop",
        "oneshot",
        "--one _ shot_",
        "one–shot",
        "é",
        "Élan groove",
        "雪 の 音",
        "one 🥁 shot",
        "loopy",
    ] {
        let canonical = crate::sample_sources::db::tags::normalize_tag_identity(label)
            .unwrap()
            .normalized_text;
        let query = format!(
            "SELECT {} FROM (SELECT ?1 AS normalized_text) st",
            crate::sample_sources::db::tags::PLAYBACK_TYPE_CLASSIFICATION
        );
        let class: Option<i64> = db
            .connection
            .query_row(&query, [&canonical], |r| r.get(0))
            .unwrap();
        let observed = writer.capture_tag_admission_observation(
            &rev,
            "old tag",
            Mutation::Rename(label.into()),
        );
        assert_eq!(observed.is_ok(), class.is_none(), "{label}");
        if let Ok(observed) = observed {
            let args = produce(&writer, &observed).unwrap();
            assert_eq!(
                args.scope().profile_acquisition_identity(),
                fixture.profile_guard.owner_identity()
            );
            assert_eq!(args.scope().revision(), rev.revision());
            assert_eq!(args.scope().root_identity(), rev.root_identity().as_bytes());
            assert_eq!(args.scope().database_identity(), rev.database_identity());
            let bytes = args.canonical_bytes().unwrap();
            assert_eq!(
                decode_metadata_tag_source_intent(&bytes).unwrap(),
                StoredMetadataTagSourceIntent::Supported(args.clone())
            );
            assert_eq!(
                bytes,
                produce(&writer, &observed)
                    .unwrap()
                    .canonical_bytes()
                    .unwrap()
            );
        }
    }
    assert_eq!(source_writer_state_snapshot(db), before);
    assert_eq!(db.connection.total_changes(), changes);
}
pub(super) fn stale() {
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old tag", "Old Tag");
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("metadata-tag-codec"),
        7,
    )
    .unwrap();
    let rev = writer.current_revision().unwrap();
    for change in [
        "INSERT INTO wav_file_tags(path,tag_id) SELECT 'one.wav',id FROM source_tags WHERE normalized_text='old tag'",
        "CREATE TABLE unexpected_tag_codec(value)",
    ] {
        let observation = writer
            .capture_tag_admission_observation(&rev, "old tag", Mutation::Delete)
            .unwrap();
        writer
            .database_for_tests()
            .connection
            .execute_batch(change)
            .unwrap();
        let before = source_writer_state_snapshot(writer.database_for_tests());
        let changes = writer.database_for_tests().connection.total_changes();
        assert!(produce(&writer, &observation).is_err());
        assert_eq!(
            source_writer_state_snapshot(writer.database_for_tests()),
            before
        );
        assert_eq!(
            writer.database_for_tests().connection.total_changes(),
            changes
        );
    }
    writer
        .database_for_tests()
        .connection
        .execute_batch("DROP TABLE unexpected_tag_codec")
        .unwrap();
    let observation = writer
        .capture_tag_admission_observation(&rev, "old tag", Mutation::Delete)
        .unwrap();
    let db = writer.into_catalog_database();
    let replacement =
        SourceWriter::new(db, SourceId::from_string("metadata-tag-codec"), 7).unwrap();
    let before = source_writer_state_snapshot(replacement.database_for_tests());
    assert!(produce(&replacement, &observation).is_err());
    assert_eq!(
        source_writer_state_snapshot(replacement.database_for_tests()),
        before
    );
}
pub(super) fn bounds() {
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old tag", "Old Tag");
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("metadata-tag-codec"),
        7,
    )
    .unwrap();
    let db = &writer.database_for_tests().connection;
    for i in (0..129).rev() {
        let path = format!("{i:03}.wav");
        db.execute(
            "INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES (?1,1,1,'wav')",
            [&path],
        )
        .unwrap();
        db.execute("INSERT INTO wav_file_tags(path,tag_id) SELECT ?1,id FROM source_tags WHERE normalized_text='old tag'",[&path]).unwrap();
    }
    let rev = writer.current_revision().unwrap();
    assert!(
        writer
            .capture_tag_admission_observation(&rev, "old tag", Mutation::Delete)
            .is_err()
    );
    db.execute("DELETE FROM wav_file_tags WHERE path='128.wav'", [])
        .unwrap();
    let observation = writer
        .capture_tag_admission_observation(&rev, "old tag", Mutation::Delete)
        .unwrap();
    let args = produce(&writer, &observation).unwrap();
    assert_eq!(args.paths().len(), 128);
    assert_eq!(
        decode_metadata_tag_source_intent(&args.canonical_bytes().unwrap()).unwrap(),
        StoredMetadataTagSourceIntent::Supported(args.clone())
    );
    assert!(args.paths().windows(2).all(|x| x[0] < x[1]));
    assert!(
        writer
            .capture_tag_admission_observation(&rev, "old tag", Mutation::Rename("new".into()))
            .is_err()
    );
    db.execute("DELETE FROM wav_file_tags WHERE path>='064.wav'", [])
        .unwrap();
    let observation = writer
        .capture_tag_admission_observation(&rev, "old tag", Mutation::Rename("new".into()))
        .unwrap();
    assert_eq!(produce(&writer, &observation).unwrap().paths().len(), 64);
}
