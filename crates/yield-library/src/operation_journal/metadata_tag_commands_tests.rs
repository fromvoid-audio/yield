use super::*;
fn fixture() -> MetadataTagSourceIntentArgs {
    MetadataTagSourceIntentArgs {
        version: 1,
        participant_id: ParticipantId::new(),
        source_command_id: SourceCommandId::new(),
        scope: MetadataTagHistoricalScope {
            profile_acquisition_identity: "profile".into(),
            source_id: "source".into(),
            root_identity: b"root".to_vec(),
            root_binding_identity: "binding".into(),
            database_identity: "database".into(),
            lifecycle_generation: 1,
            lease_epoch: 1,
            revision: 1,
        },
        directory_generation: 1,
        old_tag_id: 1,
        old_identity: "old".into(),
        mutation: MetadataTagMutation::MetadataTagDelete,
        paths: vec!["one.wav".into()],
    }
}
#[test]
fn canonical_roundtrip_and_ambiguity() {
    let args = fixture();
    let bytes = args.canonical_bytes().unwrap();
    assert_eq!(
        decode_metadata_tag_source_intent(&bytes).unwrap(),
        StoredMetadataTagSourceIntent::Supported(args)
    );
    let text = String::from_utf8(bytes).unwrap();
    for malformed in [
        format!(" {text}"),
        text.replace("\"version\":1", "\"version\":1,\"version\":1"),
        text.replace("\"version\":1", "\"version\":1,\"extra\":0"),
        text.replace("\"MetadataTagDelete\"", "\"Delete\""),
        text.replace("\"one.wav\"", "\"../one.wav\""),
        text.replace("\"one.wav\"", "\"C:one.wav\""),
        text.replace("\"one.wav\"", "\"one.wav\",\"one.wav\""),
    ] {
        assert!(
            decode_metadata_tag_source_intent(malformed.as_bytes()).is_err(),
            "{malformed}"
        );
    }
}
#[test]
fn unsupported_is_retained_and_oversized_rejected_before_parse() {
    let bytes = b"{\"version\":2,\"future_scope\":null}";
    assert_eq!(
        decode_metadata_tag_source_intent(bytes).unwrap(),
        StoredMetadataTagSourceIntent::Unsupported {
            version: 2,
            bytes: bytes.to_vec()
        }
    );
    assert_eq!(
        decode_metadata_tag_source_intent(&vec![b'x'; MAX_METADATA_TAG_SOURCE_ARGS_BYTES + 1]),
        Err(MetadataTagArgsError::Bytes)
    );
    for bytes in [
        b"{}".as_slice(),
        b"{\"version\":-1}",
        b"{\"version\":2,\"version\":3}",
        b"{\"version\":2}junk",
    ] {
        assert!(decode_metadata_tag_source_intent(bytes).is_err());
    }
}
#[test]
fn complete_path_label_identity_and_integer_bounds() {
    let mut a = fixture();
    for n in [64, 65, 128, 129] {
        a.paths = (0..n).map(|i| format!("{i:03}.wav")).collect();
        a.mutation = MetadataTagMutation::MetadataTagDelete;
        assert_eq!(a.validate().is_ok(), n <= 128);
        a.mutation = MetadataTagMutation::MetadataTagRename {
            target_identity: "new".into(),
        };
        assert_eq!(a.validate().is_ok(), n <= 64);
    }
    a = fixture();
    for path in [
        "", "/one.wav", "x//y", "x/./y", "x/../y", "x\\y", "x\0y", "c:/x",
    ] {
        a.paths = vec![path.into()];
        assert_eq!(a.validate(), Err(MetadataTagArgsError::Paths));
    }
    a.paths = vec!["é".repeat(512)];
    assert!(a.validate().is_ok());
    a.paths[0].push('x');
    assert_eq!(a.validate(), Err(MetadataTagArgsError::Paths));
    a = fixture();
    for label in [
        "",
        "OLD",
        " old",
        "old  tag",
        "loop",
        "--one _ shot_",
        "oneshot",
        "old\0tag",
    ] {
        a.old_identity = label.into();
        assert_eq!(a.validate(), Err(MetadataTagArgsError::Tag));
    }
    a = fixture();
    a.mutation = MetadataTagMutation::MetadataTagRename {
        target_identity: "old".into(),
    };
    assert_eq!(a.validate(), Err(MetadataTagArgsError::Tag));
    a = fixture();
    a.participant_id = ParticipantId::from_uuid(uuid::Uuid::nil());
    assert_eq!(a.validate(), Err(MetadataTagArgsError::Identity));
    a = fixture();
    a.source_command_id = SourceCommandId::from_uuid(uuid::Uuid::nil());
    assert_eq!(a.validate(), Err(MetadataTagArgsError::Identity));
    a = fixture();
    a.scope.revision = 0;
    assert_eq!(a.validate(), Err(MetadataTagArgsError::Identity));
    a = fixture();
    a.directory_generation = i64::MAX as u64 + 1;
    assert_eq!(a.validate(), Err(MetadataTagArgsError::Identity));
}
#[test]
fn decoded_path_container_and_encoded_escape_bytes_are_bounded() {
    let a = fixture();
    let text = String::from_utf8(a.canonical_bytes().unwrap()).unwrap();
    let huge = text.replace(
        "[\"one.wav\"]",
        &format!("[{}]", vec!["\"\""; 50000].join(",")),
    );
    assert!(huge.len() < MAX_METADATA_TAG_SOURCE_ARGS_BYTES);
    assert_eq!(
        decode_metadata_tag_source_intent(huge.as_bytes()),
        Err(MetadataTagArgsError::Wire)
    );
    let mut a = fixture();
    a.paths = (0..128)
        .map(|i| format!("{i:03}/{}", "\u{1}".repeat(1000)))
        .collect();
    assert_eq!(a.canonical_bytes(), Err(MetadataTagArgsError::Bytes));
}
