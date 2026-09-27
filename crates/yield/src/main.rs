//! Yield's GPUI launcher.

mod logging;

fn main() {
    let _log_guard = logging::init();
    logging::install_panic_hook();
    let initial_root = std::env::args_os()
        .skip(1)
        .find(|arg| arg != "--log" && arg != "-log")
        .map(std::path::PathBuf::from);
    yield_ui::run(initial_root);
}
