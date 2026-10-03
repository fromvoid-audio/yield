//! Controlled packer inputs validate identity; synthetic targets do not qualify native binaries.

use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn stable() -> [Option<&'static str>; 7] {
    [
        Some("stable"),
        Some("0.19.1"),
        Some("0.19.1"),
        Some("aarch64-apple-darwin"),
        Some(COMMIT),
        Some("42"),
        Some("2026-10-02"),
    ]
}

#[test]
fn exact_channel_build_identity_is_retained() {
    for (channel, version, expected_channel) in [
        ("stable", "0.19.1", ReleaseChannel::Stable),
        ("rc", "0.19.1-rc.2", ReleaseChannel::Rc),
        (
            "nightly",
            "0.19.1-nightly.20261002+01234567",
            ReleaseChannel::Nightly,
        ),
        (
            "nightly",
            "0.19.1-nightly.2026-10-02+01234567",
            ReleaseChannel::Nightly,
        ),
    ] {
        let mut input = stable();
        input[0] = Some(channel);
        input[1] = Some(version);
        let identity = validate(input).expect("exact packer identity");
        assert_eq!(identity.channel(), expected_channel);
        assert_eq!(identity.version(), version);
        assert_eq!(identity.target_version(), [0, 19, 1]);
        assert_eq!(identity.target(), "aarch64-apple-darwin");
        assert_eq!(identity.build_number(), 42);
        assert_eq!(identity.build_date(), "2026-10-02");
        assert_eq!(
            identity.commit(),
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            ]
        );
    }
}

#[test]
fn missing_inputs_never_fill_from_cargo_or_another_field() {
    for index in 0..7 {
        for absent in [None, Some("")] {
            let mut input = stable();
            input[index] = absent;
            assert_eq!(validate(input), Err(ReleaseIdentityError::Unavailable));
        }
    }
}

#[test]
fn mismatched_or_unknown_identity_is_rejected() {
    for (index, values) in [
        (0, vec!["beta", "stable ", "STABLE"]),
        (1, vec!["0.19.2", "0.19.1-rc.1", "0.19.1+metadata"]),
        (
            2,
            vec!["0.19", "0.19.1.2", "00.19.1", "0.4294967296.1", "0.-19.1"],
        ),
        (
            3,
            vec![
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-gnu",
                "../target",
            ],
        ),
        (
            4,
            vec![
                "01234567",
                "0000000000000000000000000000000000000000",
                "g123456789abcdef0123456789abcdef01234567",
            ],
        ),
        (5, vec!["0", "+42", "-1", "42 ", "18446744073709551616"]),
        (
            6,
            vec![
                "2026-02-29",
                "2026-13-01",
                "2026-04-31",
                "0000-01-01",
                "2026-10-2",
            ],
        ),
    ] {
        for value in values {
            let mut input = stable();
            input[index] = Some(value);
            assert_eq!(
                validate(input),
                Err(ReleaseIdentityError::InvalidMetadata),
                "{index}: {value}"
            );
        }
    }
}

#[test]
fn nightly_suffix_is_bound_to_exact_commit_and_build_date() {
    for version in [
        "0.19.1-nightly.2026-10-03+01234567",
        "0.19.1-nightly.2026-10-02+deadbeef",
        "0.19.1-nightly.20261003+01234567",
        "0.19.1-nightly.20261002+deadbeef",
        "0.19.1-nightly.20261002+012345",
        "0.19.1-nightly.20261002+01234567+metadata",
        "0.19.2-nightly.20261002+01234567",
        "0.19.1-nightly.20261002",
    ] {
        let mut input = stable();
        input[0] = Some("nightly");
        input[1] = Some(version);
        assert_eq!(validate(input), Err(ReleaseIdentityError::InvalidMetadata));
    }
}

#[test]
fn rc_ordinal_is_finite_and_preserved() {
    for version in [
        "0.19.1-rc.0",
        "0.19.1-rc.01",
        "0.19.1-rc.4294967296",
        "0.19.1-rc.1+sha",
    ] {
        let mut input = stable();
        input[0] = Some("rc");
        input[1] = Some(version);
        assert_eq!(validate(input), Err(ReleaseIdentityError::InvalidMetadata));
    }
}

#[test]
fn calendar_dates_and_contract_targets_remain_authoritative() {
    for date in ["2024-02-29", "2000-02-29"] {
        let mut input = stable();
        input[6] = Some(date);
        assert!(validate(input).is_ok());
    }
    let targets: Vec<_> = include_str!("../../../../release_contract.toml")
        .lines()
        .skip_while(|line| line.trim() != "targets = [")
        .skip(1)
        .take_while(|line| line.trim() != "]")
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .collect();
    for target in targets {
        let mut input = stable();
        input[3] = Some(target);
        assert_eq!(validate(input).unwrap().target(), target);
    }
}

#[test]
fn each_field_is_bounded_before_parsing() {
    let oversized = "a".repeat(129);
    // Test-only static storage supplies the same lifetime as compiler-captured inputs.
    let oversized = Box::leak(oversized.into_boxed_str());
    for index in 0..7 {
        for value in [oversized as &str, "日本語"] {
            let mut input = stable();
            input[index] = Some(value);
            assert_eq!(validate(input), Err(ReleaseIdentityError::InvalidMetadata));
        }
    }
}

#[test]
fn compiled_inputs_cannot_claim_an_uncontracted_host() {
    if !contract_target(env!("YIELD_COMPILED_TARGET")) {
        assert!(ReleaseBuildIdentity::compiled().is_err());
    }
}
