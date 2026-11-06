//! Private child executable for checked macOS live watcher observation.

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    use std::{env, io, path::PathBuf};
    use yield_library::sample_sources::SourceId;
    use yield_source_watcher::{SourceWatchSpec, macos_live::serve_live_watcher};

    let mut args = env::args_os().skip(1);
    let (Some(version), Some(root), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("Expected a private watcher protocol version and source root");
        return ExitCode::from(64);
    };
    let root = PathBuf::from(root);
    if version != "--protocol-v1" || !root.is_absolute() {
        eprintln!("Invalid private watcher protocol version or source root");
        return ExitCode::from(64);
    }
    // The parent owns the real source binding. The child supplies only scheduling evidence.
    let spec = SourceWatchSpec::new(SourceId::from_string("live-watch-helper"), 1, root, None);
    match serve_live_watcher(spec, io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Live watcher helper failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("The live watcher helper requires macOS");
    ExitCode::from(69)
}
