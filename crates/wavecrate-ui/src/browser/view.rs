#![deny(missing_docs)]
#![deny(warnings)]

//! Browser list, details, waveform, and sorting controls.

use gpui::{ClickEvent, Context, Render, Window, div, prelude::*, px, rgb, uniform_list};
use wavecrate_core::{SortColumn, SortDirection, format_duration};

use super::Browser;

const ROW_HEIGHT: f32 = 44.0;

impl Browser {
    fn render_waveform(&self) -> impl IntoElement {
        let mut bars = div()
            .flex()
            .items_center()
            .gap(px(1.0))
            .h(px(112.0))
            .w_full()
            .overflow_hidden()
            .px_2()
            .bg(rgb(0x0d1118));
        if let Some(preview) = self.waveform.as_ref() {
            for peak in &preview.peaks {
                bars = bars.child(
                    div()
                        .flex_1()
                        .h(px((peak * 92.0).max(3.0)))
                        .bg(rgb(0x67d9a2)),
                );
            }
        }
        bars
    }

    fn render_details(&self) -> impl IntoElement {
        let mut details = div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_none()
            .w(px(350.0))
            .p_4()
            .bg(rgb(0x171d28));
        if let Some(index) = self.selected {
            if let Some(entry) = self.entries.get(index) {
                details = details
                    .child(
                        div()
                            .text_lg()
                            .child(entry.relative_path.display().to_string()),
                    )
                    .child(div().text_sm().text_color(rgb(0x9ba7b8)).child(format!(
                                "{} · {} bytes · {} Hz",
                                format_duration(entry.duration_seconds),
                                entry.byte_len,
                                entry.sample_rate
                                    .map(|rate| rate.to_string())
                                    .unwrap_or_else(|| "unknown".to_string())
                    )))
                    .child(self.render_waveform());
                if let Some(preview) = self.waveform.as_ref() {
                    details =
                        details.child(div().text_sm().text_color(rgb(0x9ba7b8)).child(format!(
                            "Waveform preview: {}",
                            format_duration(Some(preview.duration_seconds))
                        )));
                }
                if let Some(error) = self.waveform_error.as_ref() {
                    details = details.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf0a6a6))
                            .child(error.clone()),
                    );
                } else if self.waveform.is_none() {
                    details = details.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9ba7b8))
                            .child("Loading bounded waveform preview…"),
                    );
                }
                details = details.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x9ba7b8))
                        .child("Space: play or stop · Up/Down: select · Esc: stop"),
                );
            }
        } else {
            details = details.justify_center().items_center().child(
                div()
                    .text_sm()
                    .child("Select a sample to inspect its waveform"),
            );
        }
        details
    }

    fn render_sort_button(
        &self,
        label: &'static str,
        column: SortColumn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sort = self.view_model.sort();
        let marker = if sort.column == column {
            match sort.direction {
                SortDirection::Ascending => " ↑",
                SortDirection::Descending => " ↓",
            }
        } else {
            ""
        };
        div()
            .id(label)
            .cursor_pointer()
            .rounded_sm()
            .px_2()
            .py_1()
            .bg(if sort.column == column {
                rgb(0x294d4b)
            } else {
                rgb(0x202936)
            })
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.toggle_sort(column, cx);
            }))
            .child(format!("{label}{marker}"))
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root_label = self
            .root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No folder selected".to_string());
        let list_status =
            if !self.entries.is_empty() && self.view_model.rows().len() != self.entries.len() {
                format!(
                    "{} of {} audio files visible",
                    self.view_model.rows().len(),
                    self.entries.len()
                )
            } else {
                self.scan_status.clone()
            };
        let list = if self.view_model.rows().is_empty() {
            div()
                .flex()
                .flex_1()
                .justify_center()
                .items_center()
                .text_color(rgb(0x9ba7b8))
                .child(if self.entries.is_empty() {
                    list_status.clone()
                } else {
                    "No matching filenames".to_string()
                })
        } else {
            div().flex_1().h_full().child(
                uniform_list(
                    "sample-list",
                    self.view_model.rows().len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|visible_index| {
                                let row = view.view_model.rows()[visible_index].clone();
                                let index = row.entry_index;
                                let entry = view.entries[index].clone();
                                let selected = view.selected == Some(index);
                                let background = if selected {
                                    rgb(0x294d4b)
                                } else {
                                    rgb(0x171d28)
                                };
                                div()
                                    .id(index)
                                    .flex()
                                    .flex_none()
                                    .w_full()
                                    .items_center()
                                    .gap_2()
                                    .h(px(ROW_HEIGHT))
                                    .px_3()
                                    .cursor_pointer()
                                    .bg(background)
                                    .border_1()
                                    .border_color(rgb(0x242e3d))
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.select_entry(index, cx);
                                    }))
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(entry.relative_path.display().to_string()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_sm()
                                            .text_color(rgb(0x9ba7b8))
                                            .child(format_duration(entry.duration_seconds)),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h_full()
                .track_scroll(self.list_scroll_handle.clone()),
            )
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x10141c))
            .text_color(rgb(0xe6edf5))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .bg(rgb(0x171d28))
                    .border_1()
                    .border_color(rgb(0x242e3d))
                    .child(div().text_xl().child("Wavecrate GPUI"))
                    .child(
                        div()
                            .flex_none()
                            .w(px(240.0))
                            .child(self.search_input.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(0x9ba7b8))
                            .child(root_label),
                    )
                    .child(
                        div()
                            .id("choose-folder")
                            .flex_none()
                            .cursor_pointer()
                            .rounded_sm()
                            .px_3()
                            .py_2()
                            .bg(rgb(0x2c8f79))
                            .on_click(cx.listener(Self::choose_folder))
                            .child("Choose folder"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .h_full()
                    .gap_3()
                    .p_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .gap_2()
                            .bg(rgb(0x171d28))
                            .border_1()
                            .border_color(rgb(0x242e3d))
                            .child(
                                div()
                                    .flex_none()
                                    .px_3()
                                    .py_2()
                                    .text_sm()
                                    .text_color(rgb(0x9ba7b8))
                                    .child(list_status),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_3()
                                    .pb_2()
                                    .text_sm()
                                    .child(self.render_sort_button("Name", SortColumn::Name, cx))
                                    .child(self.render_sort_button("Size", SortColumn::Size, cx))
                                    .child(self.render_sort_button(
                                        "Extension",
                                        SortColumn::Extension,
                                        cx,
                                    )),
                            )
                            .child(list),
                    )
                    .child(self.render_details()),
            )
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .text_sm()
                    .text_color(rgb(0x9ba7b8))
                    .child(self.playback_status.clone()),
            )
    }
}
