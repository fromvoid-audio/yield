#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI browser UI for Wavecrate.

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, Bounds, Focusable, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBounds, WindowOptions, actions, point, px, size,
};

#[allow(dead_code)]
pub(crate) mod beat_count_input;
mod browser;
pub(crate) mod external_clipboard;
mod search_input;
mod theme;

actions!(
    wavecrate_ui,
    [
        #[allow(missing_docs)]
        Quit
    ]
);

const WINDOW_WIDTH: f32 = 1_120.0;
const WINDOW_HEIGHT: f32 = 720.0;

/// Start the browser window.
pub fn run(initial_root: Option<PathBuf>) {
    Application::new().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);
        cx.set_menus(vec![Menu {
            name: "Wavecrate".into(),
            items: vec![MenuItem::action("Quit", Quit)],
        }]);
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
        browser::install_source_registry(cx);
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
