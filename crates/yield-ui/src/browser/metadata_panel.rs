//! Bounded, session-only presentation of the existing metadata/tag controls.

use gpui::{
    Context, DragMoveEvent, MouseButton, MouseDownEvent, MouseUpEvent, Render, Window, div,
    prelude::*, px,
};
use yield_design::Theme;

use super::{Browser, panel_resize::bounded_size, tag_chips, tag_completion, tags, view::PaneAxis};
use crate::theme::color;

pub(super) fn height(preferred: Option<f32>, viewport_height: f32, theme: &Theme) -> f32 {
    // Reserve most of the window for the browser and sibling controls. Two rows
    // keep a header and a scrollable body usable at either shipped text scale.
    let minimum = theme.spacing.row_height * 2.0;
    let maximum = (viewport_height * 0.25)
        .max(minimum)
        .min(320.0_f32.max(minimum));
    bounded_size(
        preferred.unwrap_or(theme.spacing.row_height * 5.0),
        minimum,
        maximum,
    )
}

#[derive(Clone, Copy)]
struct MetadataResizeToken;

struct ResizeGhost;

impl Render for ResizeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub(super) fn render(
    browser: &Browser,
    theme: Theme,
    viewport_height: f32,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let hover_color = color(theme.colors.border_emphasis);
    let header = div()
        .id("metadata-panel-resize")
        .debug_selector(|| "metadata-panel-resize".into())
        .flex_none()
        .w_full()
        .h(px(theme.spacing.row_height))
        .px(px(theme.spacing.panel_padding))
        .cursor_row_resize()
        .bg(color(theme.colors.surface_overlay))
        .text_color(color(theme.colors.text_muted))
        .text_size(px(theme.typography.small_size))
        .hover(move |style| style.bg(hover_color))
        .child("METADATA")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event: &MouseDownEvent, window, cx| {
                window.prevent_default();
                view.begin_pane_resize(PaneAxis::Metadata, event.position, window, cx);
                cx.stop_propagation();
            }),
        )
        .on_drag(MetadataResizeToken, |_, _, _, cx| cx.new(|_| ResizeGhost))
        .on_drag_move::<MetadataResizeToken>(cx.listener(
            |view, event: &DragMoveEvent<MetadataResizeToken>, window, cx| {
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
        .h(px(height(
            browser.preferred_metadata_height,
            viewport_height,
            &theme,
        )))
        .debug_selector(|| "metadata-panel".into())
        .child(header)
        .child(
            div()
                .id("metadata-panel-scroll")
                .debug_selector(|| "metadata-panel-scroll".into())
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .w_full()
                        .debug_selector(|| "metadata-panel-content".into())
                        .child(tags::render(browser, theme.clone(), cx))
                        .child(tag_completion::render(browser, theme.clone(), cx))
                        .child(tag_chips::render(browser, theme, cx)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::{
        AvailableSpace, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        point, size,
    };

    struct PanelFixture(gpui::Entity<Browser>);

    impl Render for PanelFixture {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.0.update(cx, |view, cx| {
                render(
                    view,
                    crate::theme::active(cx).clone(),
                    f32::from(window.bounds().size.height),
                    cx,
                )
                .into_any_element()
            })
        }
    }

    fn draw_panel(fixture: &gpui::Entity<PanelFixture>, cx: &mut gpui::VisualTestContext) {
        cx.draw(
            point(px(0.0), px(0.0)),
            size(
                AvailableSpace::Definite(px(240.0)),
                AvailableSpace::Definite(px(560.0)),
            ),
            |_, _| fixture.clone(),
        );
    }

    #[gpui::test]
    fn metadata_rail_pointer_commit_cancel_and_overflow_are_presentation_only(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (fixture, cx) = cx.add_window_view(|_, cx| PanelFixture(cx.new(|cx| Browser::new(cx))));
        let browser = cx.update(|_, app| fixture.read(app).0.clone());
        cx.simulate_resize(size(px(960.0), px(560.0)));
        for scale in [100, 200] {
            let theme = crate::theme::with_text_scale(&Theme::default(), scale);
            cx.set_global(crate::theme::ActiveTheme(std::sync::Arc::new(theme)));
            browser.update(cx, |view, _| view.preferred_metadata_height = None);
            draw_panel(&fixture, cx);
            let rail = cx
                .debug_bounds("metadata-panel-resize")
                .expect("metadata rail");
            let start = rail.center();
            let end = point(start.x, start.y - px(300.0));
            cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
            cx.simulate_mouse_move(
                point(start.x, start.y - px(4.0)),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
            cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
            cx.update(|_, app| {
                let view = browser.read(app);
                assert!(view.pane_resize_drag.is_none());
                assert!(view.preferred_metadata_height.is_some());
                assert!(view.preferred_sidebar_width.is_none());
                assert!(view.preferred_waveform_height.is_none());
                assert!(view.selected.is_none());
                assert_eq!(view.playback_epoch, 0);
            });
            draw_panel(&fixture, cx);
            let panel = cx.debug_bounds("metadata-panel").unwrap();
            let body = cx.debug_bounds("metadata-panel-scroll").unwrap();
            let rail = cx.debug_bounds("metadata-panel-resize").unwrap();
            assert!(body.size.height > px(0.0));
            assert!(body.bottom() <= panel.bottom());
            assert!(panel.size.height <= px(140.0));
            let content_before = cx
                .debug_bounds("metadata-panel-content")
                .expect("tag content");
            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta: ScrollDelta::Pixels(point(px(0.0), px(-400.0))),
                modifiers: Modifiers::none(),
                touch_phase: TouchPhase::Moved,
            });
            draw_panel(&fixture, cx);
            let content_after = cx
                .debug_bounds("metadata-panel-content")
                .expect("scrolled tag content");
            assert!(content_after.top() < content_before.top());
            assert!(content_after.bottom() <= body.bottom());
            let saved = cx.update(|_, app| browser.read(app).preferred_metadata_height);
            cx.simulate_mouse_down(rail.center(), MouseButton::Left, Modifiers::none());
            cx.simulate_mouse_move(
                point(rail.center().x, rail.center().y + px(4.0)),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.simulate_mouse_move(
                point(rail.center().x, rail.center().y + px(90.0)),
                MouseButton::Left,
                Modifiers::none(),
            );
            browser.update(cx, |view, cx| view.cancel_pane_resize(cx));
            cx.update(|_, app| {
                let view = browser.read(app);
                assert_eq!(view.preferred_metadata_height, saved);
                assert!(view.pane_resize_drag.is_none());
            });
        }
    }

    #[gpui::test]
    fn metadata_resize_escape_and_viewport_changes_restore_preference(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.refresh()
            .expect("browser layout and environment observers");
        cx.update(|window, app| browser.update(app, |view, _| window.focus(&view.focus_handle)));
        let start = point(px(100.0), px(300.0));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.preferred_metadata_height = Some(100.0);
                view.begin_pane_resize(PaneAxis::Metadata, start, window, cx);
                view.update_pane_resize(point(px(100.0), px(380.0)), window, cx);
                assert_ne!(view.preferred_metadata_height, Some(100.0));
                assert!(view.focus_handle.is_focused(window));
            })
        });
        cx.simulate_keystrokes("escape");
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert_eq!(view.preferred_metadata_height, Some(100.0));
                assert!(view.pane_resize_drag.is_none());
                view.begin_pane_resize(PaneAxis::Metadata, start, window, cx);
                view.update_pane_resize(point(px(100.0), px(380.0)), window, cx);
            })
        });
        cx.simulate_resize(size(px(960.0), px(560.0)));
        cx.run_until_parked();
        cx.update(|_, app| {
            let view = browser.read(app);
            assert_eq!(view.preferred_metadata_height, Some(100.0));
            assert!(view.pane_resize_drag.is_none());
            assert!(view.selected.is_none());
        });
    }

    #[test]
    fn metadata_height_keeps_a_scroll_body_and_reserves_sibling_space() {
        for scale in [100, 200] {
            let theme = crate::theme::with_text_scale(&Theme::default(), scale);
            for viewport in [560.0, 900.0, 1440.0] {
                for request in [-1000.0, 10000.0, f32::NAN] {
                    let size = height(Some(request), viewport, &theme);
                    assert!(size.is_finite());
                    assert!(size >= theme.spacing.row_height * 2.0);
                    assert!(size <= viewport * 0.25);
                }
            }
        }
        let theme = Theme::default();
        assert_eq!(height(Some(250.0), 1200.0, &theme), 250.0);
        assert_eq!(height(Some(250.0), 560.0, &theme), 140.0);
        assert_eq!(height(Some(250.0), 1200.0, &theme), 250.0);
    }
}
