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

#[test]
fn historical_claim_projection_keeps_maximum_scalar_wire_boundary() {
    let original = wire("0", 1);
    let mut padded = vec![b' '; super::super::super::model::MAX_RECORD_BYTES - original.len()];
    padded.extend_from_slice(&original);
    assert_eq!(padded.len(), super::super::super::model::MAX_RECORD_BYTES);
    let old = decode(&padded).unwrap();
    let new = projected(&padded).unwrap().unwrap();
    assert_eq!(classify_record(&old), classify_record(&new));
}

#[test]
fn historical_claim_projection_matches_all_legacy_status_and_existing_v2_evidence_combinations() {
    let phases = [
        "IntentDurable",
        "Prepared",
        "FilesystemStaged",
        "FilesystemPublished",
        "SourceReconciled",
        "GlobalReconciled",
        "ProjectionPublished",
        "ReadinessScheduled",
        "Terminal",
    ];
    let dispositions = [
        "None",
        "RetryPending",
        "PartialNeedsRetry",
        "AuditRequired",
        "CancelRequestedBeforePublish",
        "CancelRequestedAfterPublish",
        "Succeeded",
        "SucceededWithDeferredArtifacts",
        "CancelledBeforePublish",
        "CancelledAfterPublish",
        "RolledBack",
        "BlockedByUser",
        "FailedPreservingData",
        "FailedDataLossRisk",
    ];
    let mut complete: Value =
        serde_json::from_str(include_str!("existing_v2_fixture.json")).unwrap();
    if cfg!(windows) {
        for field in ["source_root", "target_root", "backup_root"] {
            complete["prepared"]["ExistingExpectedIdentity"][field]["path"] =
                Value::from(format!(r"C:\yield\{field}"));
        }
    }
    assert_eq!(
        classify_record(&decode(&serde_json::to_vec(&complete).unwrap()).unwrap()),
        RecordConsistency::Consistent
    );
    for phase in phases {
        for disposition in dispositions {
            for mask in 0..8 {
                let mut value = complete.clone();
                value["phase"] = Value::from(phase);
                value["disposition"] = Value::from(disposition);
                for (bit, field) in ["prepared", "staged", "published"].into_iter().enumerate() {
                    if mask & (1 << bit) == 0 {
                        value[field] = Value::Null;
                    }
                }
                let bytes = serde_json::to_vec(&value).unwrap();
                let old = decode(&bytes).unwrap();
                let new = projected(&bytes).unwrap().unwrap();
                assert_eq!(
                    classify_record(&old),
                    classify_record(&new),
                    "{phase}/{disposition}/evidence={mask}"
                );
            }
            let bytes = String::from_utf8(wire("null", 1))
                .unwrap()
                .replace("IntentDurable", phase)
                .replace("\"None\"", &format!("\"{disposition}\""));
            let old = decode(bytes.as_bytes()).unwrap();
            let new = projected(bytes.as_bytes()).unwrap().unwrap();
            assert_eq!(
                classify_record(&old),
                classify_record(&new),
                "v1 {phase}/{disposition}"
            );
        }
    }
}

#[test]
fn historical_claim_projection_matches_all_v2_absent_final_evidence_presence_categories() {
    // Stored evidence fixtures compare codec/classifier behavior only; they do not
    // install a physical capability or prove an absent-final publication backend.
    let mut complete: Value =
        serde_json::from_str(include_str!("existing_v2_fixture.json")).unwrap();
    let existing = complete["prepared"]["ExistingExpectedIdentity"].clone();
    let mut source_root = existing["source_root"].clone();
    let mut parent = existing["target_root"].clone();
    if cfg!(windows) {
        source_root["path"] = Value::from(r"C:\yield\Source");
        parent["path"] = Value::from(r"C:\yield\Target");
    }
    let content = complete["staged"]["participant"]["CopyValidated"]["evidence"].clone();
    complete["prepared"] = serde_json::json!({"AbsentFinalNoReplace": {
        "direction": "Undo", "source_id": "source-1", "source_root": source_root,
        "target_parent": parent, "final_leaf": "final.wav", "staging": existing["staging"],
        "final_observation": "ObservedAbsent", "copy_validated_evidence": content,
    }});
    complete["published"]["final_claim"] = serde_json::json!({"AbsentFinalNoReplace": {
        "primitive": "QualifiedAbsentFinalNoReplace", "result": "AbsentFinalInstalled",
        "capability_scope": {"TargetParentDescriptor": {
            "target_parent_identity": parent["identity"], "root_path_continuity": "NotClaimed",
        }},
    }});
    let evidence = serde_json::json!({
        "target_parent_stable_id": parent["identity"]["stable_id"],
        "final_stable_id": complete["published"]["reopened_final"]["identity"]["stable_id"],
        "final_len": complete["published"]["reopened_final"]["identity"]["len"],
        "final_content": content,
    });
    let base = decode(&serde_json::to_vec(&complete).unwrap()).unwrap();
    assert_eq!(classify_record(&base), RecordConsistency::Consistent);
    for phase in [
        "IntentDurable",
        "Prepared",
        "FilesystemStaged",
        "FilesystemPublished",
        "SourceReconciled",
        "GlobalReconciled",
        "ProjectionPublished",
        "ReadinessScheduled",
        "Terminal",
    ] {
        for disposition in [
            "None",
            "RetryPending",
            "PartialNeedsRetry",
            "AuditRequired",
            "CancelRequestedBeforePublish",
            "CancelRequestedAfterPublish",
            "Succeeded",
            "SucceededWithDeferredArtifacts",
            "CancelledBeforePublish",
            "CancelledAfterPublish",
            "RolledBack",
            "BlockedByUser",
            "FailedPreservingData",
            "FailedDataLossRisk",
        ] {
            for mask in 0..64 {
                let mut value = complete.clone();
                value["phase"] = Value::from(phase);
                value["disposition"] = Value::from(disposition);
                for (bit, field) in [
                    "prepared",
                    "staged",
                    "published",
                    "absent_final_recovery_observation",
                    "absent_final_transaction_owned_proof",
                    "absent_final_adoption_evidence",
                ]
                .into_iter()
                .enumerate()
                {
                    value[field] = if mask & (1 << bit) == 0 {
                        Value::Null
                    } else if bit < 3 {
                        complete[field].clone()
                    } else {
                        evidence.clone()
                    };
                }
                let bytes = serde_json::to_vec(&value).unwrap();
                let old = decode(&bytes).unwrap();
                let new = projected(&bytes).unwrap().unwrap();
                assert_eq!(
                    classify_record(&old),
                    classify_record(&new),
                    "{phase}/{disposition}/six-evidence={mask}"
                );
            }
        }
    }
}
