#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI browser UI for Wavecrate.

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowOptions,
    point, px, size,
};

mod browser;
pub(crate) mod external_clipboard;
mod search_input;
mod theme;

const WINDOW_WIDTH: f32 = 1_120.0;
const WINDOW_HEIGHT: f32 = 720.0;

/// Start the browser window.
pub fn run(initial_root: Option<PathBuf>) {
    Application::new().run(move |cx: &mut App| {
        cx.set_global(theme::ActiveTheme::default());
        if let Err(error) = cx
            .text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!(
                "../../../assets/fonts/IoskeleyMono-Regular.ttf",
            ))])
        {
            eprintln!("could not register Ioskeley Mono: {error}");
        }
        search_input::install_keybindings(cx);
        let initial_root = initial_root.clone();
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.0), px(12.0))),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.0), px(560.0))),
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
