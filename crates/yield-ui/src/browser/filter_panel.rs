//! Bounded, independently scrollable presentation of existing filter controls.

use gpui::{
    Context, DragMoveEvent, MouseButton, MouseDownEvent, MouseUpEvent, Render, Window, div,
    prelude::*, px,
};
use yield_design::Theme;

use super::{Browser, curation, panel_resize::bounded_size, tag_filter, view::PaneAxis};
use crate::theme::color;

const RAIL_HEIGHT: f32 = 6.0;

pub(super) fn height(preferred: Option<f32>, viewport_height: f32, theme: &Theme) -> f32 {
    let minimum = theme.spacing.control_height;
    // This panel lives inside the auxiliary scroll area; retain space for the
    // folder tree and siblings rather than consuming the entire workspace.
    let maximum = (viewport_height * 0.3).min(360.0).max(minimum);
    bounded_size(
        preferred.unwrap_or(theme.spacing.row_height * 6.0),
        minimum,
        maximum,
    )
}

#[derive(Clone, Copy)]
struct FilterResizeToken;

struct ResizeGhost;

impl Render for ResizeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub(super) fn render_filter_panel(
    browser: &Browser,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let controls = div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .debug_selector(|| "sidebar-filter-content".into())
        .child(curation::render_playback_type_filters(
            browser,
            theme.clone(),
            cx,
        ))
        .child(curation::render_playback_age_filters(
            browser,
            theme.clone(),
            cx,
        ))
        .child(curation::render_rating_filters(browser, theme.clone(), cx))
        .child(tag_filter::render(browser, theme.clone(), cx))
        .child(super::harvest_filter::render(browser, theme.clone(), cx));
    let rail = div()
        .id("sidebar-filter-resize")
        .debug_selector(|| "sidebar-filter-resize".into())
        .flex_none()
        .w_full()
        .h(px(RAIL_HEIGHT))
        .cursor_row_resize()
        .bg(color(theme.colors.border))
        .hover({
            let hover = color(theme.colors.border_emphasis);
            move |style| style.bg(hover)
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event: &MouseDownEvent, window, cx| {
                window.prevent_default();
                view.begin_pane_resize(PaneAxis::Filter, event.position, window, cx);
                cx.stop_propagation();
            }),
        )
        .on_drag(FilterResizeToken, |_, _, _, cx| cx.new(|_| ResizeGhost))
        .on_drag_move::<FilterResizeToken>(cx.listener(
            |view, event: &DragMoveEvent<FilterResizeToken>, window, cx| {
                view.update_pane_resize(event.event.position, window, cx);
            },
        ))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event: &MouseUpEvent, window, cx| {
                view.finish_pane_resize(event.position, window, cx);
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event: &MouseUpEvent, window, cx| {
                view.finish_pane_resize(event.position, window, cx);
            }),
        );
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .child(rail)
        .child(
            div()
                .id("sidebar-filter-scroll")
                .debug_selector(|| "sidebar-filter-viewport".into())
                .flex_none()
                .h(px(height(
                    browser.preferred_filter_height,
                    f32::from(window.viewport_size().height),
                    theme,
                )))
                .overflow_y_scroll()
                .track_scroll(&browser.filter_panel_scroll)
                .child(controls),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Entity, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        VisualTestContext, point, size,
    };

    fn reveal_rail(cx: &mut VisualTestContext) -> gpui::Bounds<gpui::Pixels> {
        for _ in 0..40 {
            cx.refresh().expect("filter layout");
            let rail = cx
                .debug_bounds("sidebar-filter-resize")
                .expect("filter rail");
            let auxiliary = cx
                .debug_bounds("sidebar-auxiliary")
                .expect("auxiliary viewport");
            if rail.top() >= auxiliary.top() && rail.bottom() <= auxiliary.bottom() {
                return rail;
            }
            cx.simulate_event(ScrollWheelEvent {
                position: point(auxiliary.right() - px(2.0), auxiliary.center().y),
                delta: ScrollDelta::Pixels(point(
                    px(0.0),
                    (auxiliary.center().y - rail.center().y).clamp(px(-40.0), px(40.0)),
                )),
                modifiers: Modifiers::none(),
                touch_phase: TouchPhase::Moved,
            });
        }
        panic!(
            "filter rail cannot be reached: rail={:?} auxiliary={:?} folder={:?}",
            cx.debug_bounds("sidebar-filter-resize"),
            cx.debug_bounds("sidebar-auxiliary"),
            cx.debug_bounds("sidebar-folder-list")
        );
    }

    fn pointer_contract(browser: Entity<Browser>, cx: &mut VisualTestContext) {
        cx.update(|window, app| {
            window.focus(&browser.read(app).search_input.read(app).focus_handle());
        });
        let rail = reveal_rail(cx);
        let start = rail.center();
        let original = cx
            .debug_bounds("sidebar-filter-viewport")
            .expect("filter viewport");
        let end = point(start.x, start.y - px(50.0));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(start.x, start.y - px(4.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        cx.refresh().expect("resized filter viewport");
        let resized = cx
            .debug_bounds("sidebar-filter-viewport")
            .expect("resized filter");
        assert!(resized.size.height < original.size.height - px(30.0));
        cx.update(|window, app| {
            let view = browser.read(app);
            assert!(view.preferred_filter_height.is_some());
            assert!(view.pane_resize_drag.is_none());
            assert!(
                view.search_input
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            );
        });
        let saved = browser.read_with(cx, |view, _| view.preferred_filter_height);
        let rail = reveal_rail(cx);
        cx.simulate_mouse_down(rail.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(rail.center().x, rail.center().y - px(4.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(rail.center().x, rail.center().y - px(40.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_keystrokes("escape");
        cx.simulate_mouse_up(rail.center(), MouseButton::Left, Modifiers::none());
        browser.update(cx, |view, _| {
            assert_eq!(view.preferred_filter_height, saved)
        });
        cx.refresh().expect("canceled filter drag");

        // The contents exceed their viewport and can be scrolled to their end.
        let before = cx
            .debug_bounds("sidebar-filter-content")
            .expect("filter content");
        let viewport = cx
            .debug_bounds("sidebar-filter-viewport")
            .expect("filter viewport");
        assert!(before.size.height > viewport.size.height);
        let auxiliary = cx
            .debug_bounds("sidebar-auxiliary")
            .expect("outer viewport");
        let visible_top = viewport.top().max(auxiliary.top());
        let visible_bottom = viewport.bottom().min(auxiliary.bottom());
        assert!(visible_bottom > visible_top);
        cx.simulate_event(ScrollWheelEvent {
            position: point(viewport.center().x, (visible_top + visible_bottom) / 2.0),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-2000.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.refresh().expect("filter controls scrolled");
        let after = cx
            .debug_bounds("sidebar-filter-content")
            .expect("scrolled content");
        assert!(after.top() < before.top());
        assert!(after.bottom() <= viewport.bottom() + px(1.0));
        assert!(
            cx.debug_bounds("sidebar-folder-list")
                .expect("folder tree")
                .size
                .height
                >= px(88.0)
        );
    }

    #[gpui::test]
    fn pointer_resize_cancel_focus_and_scroll_reachability(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.simulate_resize(size(px(960.0), px(560.0)));
        pointer_contract(browser, cx);
    }

    #[gpui::test]
    fn enlarged_pointer_resize_cancel_and_scroll_reachability(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(
            crate::theme::with_text_scale(&Theme::default(), 200),
        )));
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| view.general_settings.text_scale_percent = 200);
        cx.simulate_resize(size(px(960.0), px(560.0)));
        pointer_contract(browser, cx);
    }

    #[gpui::test]
    fn resize_context_changes_restore_preference_and_preserve_selection_viewport(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.preferred_filter_height = Some(100.0);
                view.selected_folder = std::path::PathBuf::from("drums");
                view.waveform_viewport =
                    yield_core::WaveformViewport::full().zoom_to_region(48_000, 0.2, 0.6);
                let selected = view.selected_folder.clone();
                let viewport = view.waveform_viewport;
                view.begin_pane_resize(PaneAxis::Filter, point(px(0.0), px(100.0)), window, cx);
                view.update_pane_resize(point(px(0.0), px(120.0)), window, cx);
                assert_eq!(view.selected_folder, selected);
                assert_eq!(view.waveform_viewport, viewport);
            })
        });
        cx.simulate_resize(size(px(960.0), px(560.0)));
        cx.refresh().expect("changed viewport");
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.pane_resize_drag.is_none());
                assert_eq!(view.preferred_filter_height, Some(100.0));
                view.begin_pane_resize(PaneAxis::Filter, point(px(0.0), px(100.0)), window, cx);
            });
            let enlarged = crate::theme::with_text_scale(crate::theme::active(app), 200);
            app.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(enlarged)));
        });
        cx.refresh().expect("changed text scale");
        browser.update(cx, |view, _| {
            assert!(view.pane_resize_drag.is_none());
            assert_eq!(view.preferred_filter_height, Some(100.0));
        });
    }

    #[gpui::test]
    fn window_deactivation_retires_filter_drag_without_committing_preview(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh().expect("install activation observer");
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.preferred_filter_height = Some(100.0);
                view.begin_pane_resize(PaneAxis::Filter, point(px(0.0), px(100.0)), window, cx);
                view.update_pane_resize(point(px(0.0), px(120.0)), window, cx);
                assert_ne!(view.preferred_filter_height, Some(100.0));
            })
        });
        let other = cx.cx.add_window(|_, cx| Browser::new(cx));
        other
            .update(&mut cx.cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        browser.update(cx, |view, _| {
            assert!(view.pane_resize_drag.is_none());
            assert_eq!(view.preferred_filter_height, Some(100.0));
        });
    }

    #[test]
    fn clamp_reserves_workspace_at_both_text_scales_and_restores_size() {
        for percent in [100, 200] {
            let theme = crate::theme::with_text_scale(&Theme::default(), percent);
            for viewport in [400.0, 560.0, 1000.0] {
                assert!(height(Some(10000.0), viewport, &theme) <= viewport * 0.3);
                assert_eq!(
                    height(Some(f32::NAN), viewport, &theme),
                    theme.spacing.control_height
                );
            }
            let preference = Some(240.0);
            assert!(height(preference, 400.0, &theme) < height(preference, 1000.0, &theme));
            assert_eq!(height(preference, 1000.0, &theme), 240.0);
        }
    }
}
