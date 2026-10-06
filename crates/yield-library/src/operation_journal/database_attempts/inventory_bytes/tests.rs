use super::*;

#[derive(serde::Deserialize)]
struct Envelope {
    #[serde(deserialize_with = "deserialize")]
    inventory: Vec<u8>,
}

#[test]
fn attempt_inventory_bytes_preserve_boundary_and_bound_capacity() {
    for length in [
        0,
        1,
        MAX_INVENTORY_BYTES - 1,
        MAX_INVENTORY_BYTES,
        MAX_INVENTORY_BYTES + 1,
        4 * MAX_INVENTORY_BYTES,
    ] {
        let original: Vec<u8> = (0..length).map(|n| (n % 256) as u8).collect();
        let json = serde_json::json!({"inventory": original});
        OWNED_BYTES.with(|n| n.set(0));
        MAX_CAPACITY.with(|n| n.set(0));
        let value: Envelope = serde_json::from_slice(&serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(
            value.inventory,
            original[..length.min(MAX_INVENTORY_BYTES + 1)]
        );
        assert_eq!(
            OWNED_BYTES.with(|n| n.get()),
            length.min(MAX_INVENTORY_BYTES + 1)
        );
        assert!(MAX_CAPACITY.with(|n| n.get()) <= MAX_INVENTORY_BYTES + 1);
    }
}

#[test]
fn attempt_inventory_bytes_validate_late_scalar_after_overflow() {
    let prefix = "0,".repeat(MAX_INVENTORY_BYTES + 10);
    for invalid in ["256", "-1", "null", "true", "\"0\"", "{}", "[]", "0.5"] {
        let json = format!("{{\"inventory\":[{prefix}{invalid}]}}");
        let expected = serde_json::from_str::<Vec<u8>>(&format!("[{prefix}{invalid}]"))
            .err()
            .unwrap()
            .to_string();
        let actual = serde_json::from_str::<Envelope>(&json)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            actual.split(" at line").next(),
            expected.split(" at line").next()
        );
    }
}

#[test]
fn attempt_inventory_bytes_canonical_overflow_checks_complete_wire() {
    let array = "0,".repeat(MAX_INVENTORY_BYTES);
    let input = format!("{{\"inventory\":[{array}0],\"tail\":1}}");
    let canonical = format!("{{\"inventory\":[{array}0],\"tail\":1}}");
    assert!(canonical_overflow(input.as_bytes(), canonical.as_bytes(), usize::MAX).unwrap());
    assert_eq!(
        canonical_overflow(input.as_bytes(), canonical.as_bytes(), canonical.len() - 1)
            .unwrap_err()
            .to_string(),
        "attempt encoding bound"
    );
    let whitespace = input.replace("[0", "[ 0");
    assert_eq!(
        canonical_overflow(
            whitespace.as_bytes(),
            canonical.as_bytes(),
            canonical.len() - 1
        )
        .unwrap_err()
        .to_string(),
        "attempt encoding bound"
    );
    for input in [
        input.replace("[0", "[ 0"),
        input.replace(",0]", ",0 ]"),
        input.replace("\"tail\":1", "\"tail\":2"),
        input.replace("{\"inventory\"", "{ \"inventory\""),
    ] {
        assert!(!canonical_overflow(input.as_bytes(), canonical.as_bytes(), usize::MAX).unwrap());
    }
}
