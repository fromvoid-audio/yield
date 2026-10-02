//! Capture the actual compiler target; runtime environment cannot select an updater platform.

fn main() {
    println!(
        "cargo:rustc-env=YIELD_COMPILED_TARGET={}",
        std::env::var("TARGET").expect("Cargo TARGET")
    );
}
