//! Yield's GPUI launcher.

fn main() {
    let initial_root = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    yield_ui::run(initial_root);
}
