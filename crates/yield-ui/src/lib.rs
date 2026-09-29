#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI browser UI for Yield.

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
    yield_ui,
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
            name: "Yield".into(),
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
        // Keep isolated debug QA windows off the primary work display when requested.
        // Release builds and ordinary launches retain the normal primary placement.
        #[cfg(debug_assertions)]
        let qa_display = (std::env::var("YIELD_DEV_DISPLAY").ok().as_deref() == Some("secondary"))
            .then(|| {
                let primary = cx.primary_display()?.id();
                cx.displays()
                    .into_iter()
                    .find(|display| display.id() != primary)
                    .map(|display| display.id())
            })
            .flatten();
        #[cfg(not(debug_assertions))]
        let qa_display = None;
        let bounds = Bounds::centered(qa_display, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.0), px(12.0))),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id: qa_display,
                window_min_size: Some(size(px(960.0), px(560.0))),
                ..Default::default()
            },
            move |window, cx| {
                // The renderer experiment is opt-in until the effect controls and
                // persistence contract are ready for users.
                #[cfg(debug_assertions)]
                if std::env::var("YIELD_DEV_POST_EFFECT").ok().as_deref() == Some("1") {
                    if let Err(error) = window.set_post_shader(Some(DEBUG_POST_EFFECT)) {
                        eprintln!("visual post effect unavailable: {error}");
                    }
                }
                let view = cx.new(|cx| browser::Browser::new_window(window, cx));
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

#[cfg(debug_assertions)]
const DEBUG_POST_EFFECT: &str = r#"
@group(0) @binding(0) var screen: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;

@fragment
fn post_fragment(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let color = textureSample(screen, screen_sampler, uv);
    return vec4<f32>(color.rgb * vec3<f32>(0.94, 0.98, 1.08), color.a);
}
"#;
