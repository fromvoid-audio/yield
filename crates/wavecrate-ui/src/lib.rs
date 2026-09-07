#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI browser UI for Wavecrate.

use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};

mod browser;
mod search_input;

const WINDOW_WIDTH: f32 = 1_120.0;
const WINDOW_HEIGHT: f32 = 720.0;

/// Start the browser window.
pub fn run(initial_root: Option<PathBuf>) {
    Application::new().run(move |cx: &mut App| {
        search_input::install_keybindings(cx);
        let initial_root = initial_root.clone();
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Wavecrate GPUI".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(browser::Browser::new);
                window.focus(&view.read(cx).focus_handle(cx));
                if let Some(root) = initial_root {
                    view.update(cx, |view, cx| view.start_scan(root, cx));
                }
                view
            },
        )
        .expect("open GPUI window");
        cx.activate(true);
    });
}
