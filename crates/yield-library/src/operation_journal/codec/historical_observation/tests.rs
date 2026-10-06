use super::super::super::validation::{RecordConsistency, classify_record};
use super::super::decode;
use super::*;

fn wire(payload: &str, version: u32) -> Vec<u8> {
    format!(r#"{{"schema_version":{version},"operation_id":"01234567-89ab-cdef-0123-456789abcdef","intent":{{"actor":"User","kind":"FileHistory","label":"v1"}},"phase":"IntentDurable","disposition":"None","payload":{payload},"created_unix_ms":1,"updated_unix_ms":1}}"#).into_bytes()
}
fn projected(bytes: &[u8]) -> Result<Option<StoredOperationRecord>, OperationJournalStoreError> {
    let input = InputPermit::historical_test_input(1);
    let mut scratch = Scratch::new(&input, bytes.len())?;
    observe(bytes, &mut scratch)
}

#[test]
fn historical_claim_projection_matches_original_opaque_json_rules() {
    for version in [1, 2] {
        for payload in [
            "null",
            "true",
            "false",
            "0",
            "-9223372036854775809",
            "18446744073709551616",
            "1e300",
            r#""\u0000é\ud83d\ude00\"\\""#,
            r#"{"":{"nested":[null,0,"x"]},"b":2}"#,
        ] {
            let bytes = wire(payload, version);
            let original = decode(&bytes).unwrap();
            let observation = projected(&bytes).unwrap().unwrap();
            assert_eq!(
                classify_record(&original),
                classify_record(&observation),
                "{payload}"
            );
            assert_eq!(observation.payload, Value::Null);
        }
        for payload in [
            r#"{"a":1,"\u0061":2}"#,
            r#"{"é":1,"\u00e9":2}"#,
            r#"{"😀":1,"\ud83d\ude00":2}"#,
            r#"{"x":{"a":1,"a":2}}"#,
            r#""\ud800""#,
            r#""\udc00""#,
            "1e400",
            "[1,]",
            "[",
            "NaN",
        ] {
            let bytes = wire(payload, version);
            assert!(decode(&bytes).is_err(), "old accepted {payload}");
            assert!(projected(&bytes).is_err(), "observer accepted {payload}");
        }
    }
}

#[test]
fn historical_claim_projection_preserves_scalar_nested_and_consistency_refusals() {
    let original = String::from_utf8(wire("null", 1)).unwrap();
    for altered in [
        original.replace("\"label\":\"v1\"", "\"label\":\"v1\",\"unknown\":[]"),
        original.replace("\"phase\":\"IntentDurable\"", "\"phase\":\"Unknown\""),
        original.replace("\"created_unix_ms\":1", "\"created_unix_ms\":1.5"),
        original.replace("\"payload\":null", "\"payload\":null,\"payload\":null"),
    ] {
        assert!(decode(altered.as_bytes()).is_err());
        assert!(projected(altered.as_bytes()).is_err());
    }
    for phase in [
        "IntentDurable",
        "Prepared",
        "FilesystemPublished",
        "Terminal",
    ] {
        let altered = original.replace("IntentDurable", phase);
        let old = decode(altered.as_bytes()).unwrap();
        let new = projected(altered.as_bytes()).unwrap().unwrap();
        assert_eq!(classify_record(&old), classify_record(&new));
    }
}

#[test]
fn historical_claim_projection_accepts_exact_maximum_payload_without_graph_ownership() {
    let mut bytes = wire("\"\"", 1);
    let position = bytes.windows(2).position(|w| w == b"\"\"").unwrap() + 1;
    bytes.splice(
        position..position,
        std::iter::repeat_n(
            b'x',
            super::super::super::model::MAX_RECORD_BYTES - bytes.len(),
        ),
    );
    assert_eq!(bytes.len(), super::super::super::model::MAX_RECORD_BYTES);
    let old = decode(&bytes).unwrap();
    let new = projected(&bytes).unwrap().unwrap();
    assert_eq!(classify_record(&old), RecordConsistency::Consistent);
    assert_eq!(classify_record(&old), classify_record(&new));
    assert_eq!(new.payload, Value::Null);
}

#[test]
fn historical_claim_projection_refuses_original_pool_pressure_without_fallback() {
    let input =
        InputPermit::historical_test_input(3 * super::super::super::model::MAX_RECORD_BYTES);
    assert!(matches!(
        Scratch::new(&input, super::super::super::model::MAX_RECORD_BYTES),
        Err(OperationJournalStoreError::HistoricalClaimResourceBound)
    ));
}

#[test]
fn historical_claim_projection_preserves_original_depth_boundary_and_object_identity() {
    for levels in [1, 64, 124, 125, 126, 127, 128, 129] {
        let payload = format!("{}0{}", "[".repeat(levels), "]".repeat(levels));
        let bytes = wire(&payload, 1);
        assert_eq!(
            decode(&bytes).is_ok(),
            projected(&bytes).is_ok(),
            "depth={levels}"
        );
    }
    let bytes = wire(r#"[{"a":1},{"\u0061":2}]"#, 1);
    assert!(
        projected(&bytes).is_ok(),
        "separate objects may reuse decoded keys"
    );
}
