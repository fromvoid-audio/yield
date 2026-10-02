//! Focus-owned unsupported-entry modal rendering.
use super::*;

/// Render a focus-owned modal with a bounded page, visible unknown/error states and owner-only inspection.
pub(in crate::browser) fn render(
    browser: &Browser,
    theme: &Theme,
    viewport: Size<Pixels>,
    cx: &mut Context<Browser>,
) -> impl IntoElement {
    let model = &browser.unsupported_entries.model;
    let epoch = model.page_epoch;
    let rows = model
        .page
        .as_ref()
        .map(|p| {
            p.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let selected = model.selected == Some(index);
                    div()
                        .id(("unsupported-entry", index))
                        .debug_selector(move || format!("unsupported-entry-{epoch}-{index}"))
                        .flex()
                        .flex_none()
                        .flex_col()
                        .w_full()
                        .px(px(8.0))
                        .py(px(4.0))
                        .when(selected, |e| e.bg(color(theme.colors.selection_hover_fill)))
                        .child(
                            div()
                                .whitespace_normal()
                                .child(display_path(&entry.relative_path)),
                        )
                        .when_some(
                            browser.ax_marker(format!("unsupported-entry-{epoch}-{index}")),
                            |e, m| e.relative().child(m),
                        )
                        .child(
                            div()
                                .text_size(px(theme.typography.small_size))
                                .text_color(color(theme.colors.text_muted))
                                .child(row_description(entry)),
                        )
                        .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                            window.focus(&view.unsupported_entries.focus);
                            if view
                                .unsupported_entries
                                .model
                                .select_row_for_page(index, epoch)
                            {
                                view.inspect_unsupported_file(cx);
                            }
                            cx.notify();
                        }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let pending = model.pending.is_some();
    div()
        .id(MODAL_ID)
        .debug_selector(|| MODAL_ID.into())
        .when_some(browser.ax_marker(MODAL_ID), |e, m| e.child(m))
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000088))
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|view, _: &MouseDownEvent, window, cx| {
                view.close_unsupported_files(window, cx);
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(720.0))
                .h(px(520.0))
                .max_w(viewport.width)
                .max_h(viewport.height)
                .bg(color(theme.colors.surface_raised))
                .text_color(color(theme.colors.text_primary))
                .border(px(1.0))
                .border_color(color(theme.colors.border_emphasis))
                .track_focus(&browser.unsupported_entries.focus)
                .occlude()
                .on_any_mouse_down(
                    cx.listener(|_: &mut Browser, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .p(px(8.0))
                        .child("Unsupported / retained entries")
                        .child(div().flex_1())
                        .child(
                            div()
                                .id(CLOSE_ID)
                                .debug_selector(|| CLOSE_ID.into())
                                .when_some(browser.ax_marker(CLOSE_ID), |e, m| {
                                    e.relative().child(m)
                                })
                                .cursor_pointer()
                                .child("Close")
                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                    view.close_unsupported_files(window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .id(STATUS_ID)
                        .debug_selector(|| STATUS_ID.into())
                        .when_some(browser.ax_marker(STATUS_ID), |e, m| e.relative().child(m))
                        .p(px(8.0))
                        .child(model.status),
                )
                .child(
                    div()
                        .id(LIST_ID)
                        .debug_selector(|| LIST_ID.into())
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&browser.unsupported_entries.scroll)
                        .children(rows),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(12.0))
                        .p(px(8.0))
                        .child(
                            div()
                                .id("unsupported-entries-back")
                                .when_some(browser.ax_marker("unsupported-entries-back"), |e, m| {
                                    e.relative().child(m)
                                })
                                .child("Back (up to 8 pages)")
                                .when(!pending && !model.history.is_empty(), |e| {
                                    e.cursor_pointer().on_click(cx.listener(
                                        |view, _: &ClickEvent, _, cx| {
                                            if view.unsupported_entries.model.change_page(false) {
                                                retry(view, cx);
                                                cx.notify();
                                            }
                                        },
                                    ))
                                }),
                        )
                        .child(
                            div()
                                .id("unsupported-entries-next")
                                .when_some(browser.ax_marker("unsupported-entries-next"), |e, m| {
                                    e.relative().child(m)
                                })
                                .child("Next")
                                .when(
                                    !pending
                                        && model.page.as_ref().is_some_and(|p| p.next.is_some()),
                                    |e| {
                                        e.cursor_pointer().on_click(cx.listener(
                                            |view, _: &ClickEvent, _, cx| {
                                                if view.unsupported_entries.model.change_page(true)
                                                {
                                                    retry(view, cx);
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                    },
                                ),
                        )
                        .child(
                            div()
                                .id("unsupported-entries-restart")
                                .when_some(
                                    browser.ax_marker("unsupported-entries-restart"),
                                    |e, m| e.relative().child(m),
                                )
                                .cursor_pointer()
                                .child("Restart")
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.restart_unsupported_files(cx)
                                })),
                        ),
                ),
        )
}
