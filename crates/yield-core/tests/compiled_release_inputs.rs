//! Exercise the real compiled API in a child process with conflicting runtime metadata.

use std::process::Command;
use yield_core::ReleaseBuildIdentity;

#[test]
fn runtime_variables_cannot_supply_or_replace_identity() {
    let compiled = format!("{:?}", ReleaseBuildIdentity::compiled());
    if let Ok(expected) = std::env::var("YIELD_IDENTITY_TEST_EXPECTED") {
        assert_eq!(compiled, expected);
        return;
    }
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "runtime_variables_cannot_supply_or_replace_identity",
            "--nocapture",
        ])
        .env("YIELD_IDENTITY_TEST_EXPECTED", compiled)
        .env("YIELD_COMPILED_TARGET", "aarch64-apple-darwin")
        .env("YIELD_RELEASE_CHANNEL", "stable")
        .env("YIELD_RELEASE_VERSION", "99.0.0")
        .env("YIELD_RELEASE_TARGET_VERSION", "99.0.0")
        .env(
            "YIELD_GIT_SHA",
            "abcdef0123456789abcdef0123456789abcdef0123",
        )
        .env("YIELD_BUILD_NUMBER", "999")
        .env("YIELD_RELEASE_BUILD_DATE", "2026-10-02")
        .output()
        .expect("spawn conflicting-runtime fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
