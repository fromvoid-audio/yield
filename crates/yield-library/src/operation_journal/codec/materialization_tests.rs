// Existing real wire fixtures exercise the callable production bounded decoder.
fn json_materialization_generous_limits() -> JsonMaterializationLimits {
    JsonMaterializationLimits {
        maximum_nodes: MAX_RECORD_BYTES,
        maximum_array_entries: MAX_RECORD_BYTES,
        maximum_object_entries: MAX_RECORD_BYTES,
        maximum_owned_string_bytes: MAX_RECORD_BYTES,
        maximum_depth: 128,
    }
}
#[test]
fn bounded_materialization_pinned_and_golden_records_agree_with_original_decoder() {
    for bytes in [V1_FIXTURE, V2_FIXTURE, V3_FIXTURE] {
        assert_eq!(
            decode_with_limits(bytes, json_materialization_generous_limits()).unwrap(),
            decode(bytes).unwrap()
        );
    }
    let mut fixtures = v6_v12_codec_golden_fixtures();
    fixtures.extend([
        ("v4".into(), v4_cancel_fixture()),
        ("v5".into(), v5_copy_fixture()),
        ("v13".into(), v13_selection_output_fixture(false)),
        ("v13-compound".into(), v13_selection_output_fixture(true)),
        ("v14".into(), v14_history_fixture("Undo")),
        ("v15".into(), v15_external_reconciliation_fixture()),
        ("v16".into(), v16_full_capacity_fixture()),
    ]);
    for (_, fixture) in fixtures {
        let bytes = serde_json::to_vec(&fixture).unwrap();
        assert_eq!(
            decode_with_limits(&bytes, json_materialization_generous_limits()).unwrap(),
            decode(&bytes).unwrap()
        );
    }
}
#[test]
fn bounded_materialization_opaque_payload_refusal_does_not_change_default_acceptance() {
    let mut record = decode(V1_FIXTURE).unwrap();
    record.payload = serde_json::json!(vec![Value::Null; 10_000]);
    let bytes = encode(&record).unwrap();
    assert_eq!(decode(&bytes).unwrap(), record);
    assert!(matches!(
        decode_with_limits(
            &bytes,
            JsonMaterializationLimits {
                maximum_array_entries: 16,
                ..json_materialization_generous_limits()
            }
        ),
        Err(BoundedDecodeError::LimitExceeded {
            resource: JsonResourceKind::ArrayEntries,
            maximum: 16
        })
    ));
    assert_eq!(
        decode_with_limits(&bytes, json_materialization_generous_limits()).unwrap(),
        record
    );
}
#[test]
fn bounded_materialization_schema_and_wire_failures_keep_original_categories() {
    let mut value: Value = serde_json::from_slice(V1_FIXTURE).unwrap();
    value["schema_version"] = Value::from(999);
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        decode_with_limits(&bytes, json_materialization_generous_limits()),
        Err(BoundedDecodeError::Decode(
            DecodeError::UnsupportedVersion { version: 999 }
        ))
    ));
    assert!(matches!(
        decode_with_limits(
            &vec![b'x'; MAX_RECORD_BYTES + 1],
            json_materialization_generous_limits()
        ),
        Err(BoundedDecodeError::Decode(DecodeError::Oversize { .. }))
    ));
}

#[test]
fn bounded_materialization_actual_record_object_string_and_depth_refusals() {
    let mut record = decode(V1_FIXTURE).unwrap();
    for (payload, limits, resource) in [
        (
            Value::Object(
                (0..1_000)
                    .map(|i| (format!("key-{i}"), Value::Null))
                    .collect(),
            ),
            JsonMaterializationLimits {
                maximum_object_entries: 64,
                ..json_materialization_generous_limits()
            },
            JsonResourceKind::ObjectEntries,
        ),
        (
            serde_json::json!({"escaped-key": "€".repeat(1_000)}),
            JsonMaterializationLimits {
                maximum_owned_string_bytes: 2_048,
                ..json_materialization_generous_limits()
            },
            JsonResourceKind::OwnedStringBytes,
        ),
        (
            (0..20).fold(Value::Null, |child, _| Value::Array(vec![child])),
            JsonMaterializationLimits {
                maximum_depth: 8,
                ..json_materialization_generous_limits()
            },
            JsonResourceKind::Depth,
        ),
    ] {
        record.payload = payload;
        let bytes = encode(&record).unwrap();
        assert_eq!(decode(&bytes).unwrap(), record);
        assert!(
            matches!(decode_with_limits(&bytes, limits), Err(BoundedDecodeError::LimitExceeded { resource: actual, .. }) if actual == resource)
        );
    }
}
