//! Yield's GPUI launcher.

mod logging;

const MAINTENANCE_ADMISSION_MARKER: &str = "YIELD_ADMISSION_PROTOCOL_V1";

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--build-info")) {
        println!(
            "{{\"namespace\":\"{}\",\"protocol\":{},\"package_version\":\"{}\",\"admission_marker\":\"{}\"}}",
            yield_library::app_dirs::APP_DIR_NAME,
            yield_library::app_dirs::MAINTENANCE_ADMISSION_PROTOCOL_VERSION,
            env!("CARGO_PKG_VERSION"),
            MAINTENANCE_ADMISSION_MARKER,
        );
        return;
    }
    let _maintenance_admission =
        match yield_library::app_dirs::acquire_process_maintenance_admission() {
            Ok(admission) => admission,
            Err(error) => {
                eprintln!("Yield could not enter its persistence namespace: {error}");
                std::process::exit(75);
            }
        };
    // The inherited descriptors have been validated, retained as an Arc, and closed by the
    // library. Remove their numeric names before any subprocess can inherit stale FD values.
    // SAFETY: this runs at the start of main, before logging or worker threads are created.
    unsafe {
        std::env::remove_var("YIELD_MAINTENANCE_ADMISSION_FD");
        std::env::remove_var("YIELD_MAINTENANCE_TRANSITION_FD");
    }
    let _log_guard = logging::init();
    logging::install_panic_hook();
    let initial_root = std::env::args_os()
        .skip(1)
        .find(|arg| arg != "--log" && arg != "-log")
        .map(std::path::PathBuf::from);
    yield_ui::run(initial_root);
}
