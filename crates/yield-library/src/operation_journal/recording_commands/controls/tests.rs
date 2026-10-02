use super::*;

#[test]
fn reserved_controls_bind_every_role_and_exact_proposal_without_authority() {
    let proposal = recording_proposal_for_test(2, 10_000);
    let mut identities = std::collections::HashSet::new();
    for role in ControlRole::ALL {
        let value = RecordingControlReservation::new(&proposal, role).unwrap();
        assert!(identities.insert(value.id()));
        let bytes = value.encode(&proposal).unwrap();
        assert!(bytes.len() < CONTROL_BYTES);
        assert_eq!(
            RecordingControlReservation::decode(&bytes, &proposal).unwrap(),
            value
        );
        assert!(
            RecordingControlReservation::decode(&bytes, &recording_proposal_for_test(2, 10_000))
                .is_err()
        );
        for (field, replacement) in [
            ("version", serde_json::json!(2)),
            ("role", serde_json::json!("Unknown")),
            ("control_id", serde_json::json!(Uuid::new_v4())),
            ("output_id", serde_json::json!(Uuid::new_v4())),
            ("operation_id", serde_json::json!(OperationId::new())),
            ("state", serde_json::json!("Frozen")),
        ] {
            let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            json[field] = replacement;
            assert!(
                RecordingControlReservation::decode(&serde_json::to_vec(&json).unwrap(), &proposal)
                    .is_err(),
                "{field}"
            );
        }
        let duplicate = String::from_utf8(bytes)
            .unwrap()
            .replacen("{", "{\"version\":1,", 1);
        assert!(RecordingControlReservation::decode(duplicate.as_bytes(), &proposal).is_err());
    }
    assert!(
        RecordingControlReservation::decode(&vec![b' '; CONTROL_BYTES + 1], &proposal).is_err()
    );
}

#[test]
fn reservation_rejects_historical_shape_and_full_metadata_budget_without_rewriting_codec() {
    let at_old_limit = recording_proposal_for_test(2, 1008 * 2048);
    assert!(at_old_limit.encode().is_ok());
    assert_eq!(
        at_old_limit.retained_metadata_bound().unwrap(),
        MAX_RECORDING_RETAINED_ENCODED_BYTES
    );
    assert!(reservation_metadata_bound(&at_old_limit).is_err());
    let bounded = recording_proposal_for_test(2, 1007 * 2048);
    assert!(reservation_metadata_bound(&bounded).is_ok());
    let mut wire: serde_json::Value = serde_json::from_slice(&bounded.encode().unwrap()).unwrap();
    wire["format"]["header_bytes"] = 44.into();
    let historical = RecordingProposal::decode(&serde_json::to_vec(&wire).unwrap()).unwrap();
    assert!(reservation_metadata_bound(&historical).is_err());
}
