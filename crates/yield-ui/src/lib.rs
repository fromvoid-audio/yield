#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI browser UI for Yield.

use std::borrow::Cow;
use std::path::PathBuf;
#[cfg(debug_assertions)]
use std::{cell::Cell, rc::Rc};

use gpui::{
    App, AppContext, Application, Bounds, Focusable, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBounds, WindowOptions, actions, point, px, size,
};

#[allow(dead_code)]
pub(crate) mod beat_count_input;
mod browser;
pub(crate) mod external_clipboard;
mod multiline_text_input;
mod search_input;
mod theme;

actions!(
    yield_ui,
    [
        #[allow(missing_docs)]
        Quit,
        #[allow(missing_docs)]
        TogglePostEffect
    ]
);

const WINDOW_WIDTH: f32 = 1_120.0;
const WINDOW_HEIGHT: f32 = 720.0;

/// Start the browser window.
pub fn run(initial_root: Option<PathBuf>) {
    Application::new().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        #[cfg(debug_assertions)]
        {
            let enabled = Rc::new(Cell::new(
                std::env::var("YIELD_DEV_POST_EFFECT").ok().as_deref() == Some("1"),
            ));
            cx.on_action(move |_: &TogglePostEffect, cx| {
                let Some(handle) = cx.active_window() else {
                    return;
                };
                let next = !enabled.get();
                let enabled = Rc::clone(&enabled);
                // Native menu actions can run while the active window is already
                // being updated, so return it to App before updating its renderer.
                cx.defer(move |cx| {
                    let source = next.then_some(DEBUG_POST_EFFECT);
                    match handle.update(cx, |_, window, _| window.set_post_shader(source)) {
                        Ok(Ok(())) => enabled.set(next),
                        Ok(Err(error)) => {
                            enabled.set(false);
                            eprintln!("visual post effect unavailable: {error}");
                        }
                        Err(error) => eprintln!("could not update visual post effect: {error}"),
                    }
                });
            });
        }
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
        #[cfg(debug_assertions)]
        let menu_items = vec![
            MenuItem::action("Toggle Debug Post Effect", TogglePostEffect),
            MenuItem::action("Quit", Quit),
        ];
        #[cfg(not(debug_assertions))]
        let menu_items = vec![MenuItem::action("Quit", Quit)];
        cx.set_menus(vec![Menu {
            name: "Yield".into(),
            items: menu_items,
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
        multiline_text_input::install_keybindings(cx);
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
    let luma = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return vec4<f32>(vec3<f32>(luma), color.a);
}
"#;
