fn main() {
    gpui::run_native_window_selector_probe().expect("native window selector regression");
    println!("native-window-selector-probe: PASS");
}
