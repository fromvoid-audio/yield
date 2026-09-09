//! Native GPUI text input used by the browser's filename filter.
//!
//! The editing implementation is adapted from GPUI 0.2.2's `examples/input.rs`, copyright
//! Zed Industries, Inc., under Apache-2.0:
//! <https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs>.

use std::borrow::Cow;
use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;
use wavecrate_core::{MAX_TAG_FILTER_INPUT_BYTES, MAX_TAG_INPUT_BYTES};

use crate::theme::{active, color};

const MAX_NUMERIC_INPUT_BYTES: usize = 64 * 1024;
const MAX_NUMERIC_DIGITS: usize = 3;

actions!(
    search_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
    ]
);

/// Event emitted whenever the search text changes.
pub(crate) struct SearchChanged;

/// Event emitted when a configured input rejects an edit for exceeding its byte bound.
pub(crate) struct SearchInputRejected;

/// Install the standard GPUI editing actions used by [`SearchInput`].
pub(crate) fn install_keybindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("cmd-a", SelectAll, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("ctrl-a", SelectAll, None),
        KeyBinding::new("ctrl-v", Paste, None),
        KeyBinding::new("ctrl-c", Copy, None),
        KeyBinding::new("ctrl-x", Cut, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
    ]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SearchInputOptions {
    placeholder: &'static str,
    max_bytes: Option<usize>,
    max_characters: Option<usize>,
    paste_newlines: PasteNewlines,
    underlined: bool,
    ascii_digits_only: bool,
    compact_height: Option<u16>,
}

impl SearchInputOptions {
    pub(crate) const fn filename() -> Self {
        Self {
            placeholder: "Search filenames",
            max_bytes: None,
            max_characters: None,
            paste_newlines: PasteNewlines::AsSpaces,
            underlined: false,
            ascii_digits_only: false,
            compact_height: None,
        }
    }

    pub(crate) const fn tags() -> Self {
        Self {
            placeholder: "ADD TAG",
            max_bytes: Some(MAX_TAG_INPUT_BYTES),
            max_characters: None,
            paste_newlines: PasteNewlines::AsCommas,
            underlined: true,
            ascii_digits_only: false,
            compact_height: None,
        }
    }

    pub(crate) const fn tag_filter() -> Self {
        Self {
            placeholder: "ANY",
            max_bytes: Some(MAX_TAG_FILTER_INPUT_BYTES),
            max_characters: None,
            paste_newlines: PasteNewlines::Preserve,
            underlined: true,
            ascii_digits_only: false,
            compact_height: None,
        }
    }

    pub(crate) const fn beat_count() -> Self {
        Self {
            placeholder: "",
            max_bytes: Some(3),
            max_characters: None,
            paste_newlines: PasteNewlines::Preserve,
            underlined: false,
            ascii_digits_only: true,
            compact_height: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasteNewlines {
    AsSpaces,
    AsCommas,
    Preserve,
}

/// Text editing state for the browser search field.
pub(crate) struct SearchInput {
    focus_handle: FocusHandle,
    options: SearchInputOptions,
    content: SharedString,
    placeholder: SharedString,
    ghost: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl EventEmitter<SearchChanged> for SearchInput {}
impl EventEmitter<SearchInputRejected> for SearchInput {}

impl SearchInput {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self::new_with_options(cx, SearchInputOptions::filename())
    }

    pub(crate) fn new_with_options(cx: &mut Context<Self>, options: SearchInputOptions) -> Self {
        Self::new_with_options_and_content(cx, options, "")
    }

    pub(crate) fn new_with_options_and_content(
        cx: &mut Context<Self>,
        options: SearchInputOptions,
        content: impl Into<SharedString>,
    ) -> Self {
        let content = content.into();
        let content = if options.ascii_digits_only {
            filter_ascii_digits(&content)
                .unwrap_or_default()
                .into_owned()
                .into()
        } else {
            content
        };
        let content = if characters_within_limit(&content, options.max_characters) {
            content
        } else {
            "".into()
        };
        Self {
            focus_handle: cx.focus_handle(),
            options,
            content,
            placeholder: options.placeholder.into(),
            ghost: "".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// Set the noncommittal completion suffix shown after the current content.
    pub(crate) fn set_ghost(&mut self, ghost: impl Into<SharedString>, cx: &mut Context<Self>) {
        let ghost = ghost.into();
        if self.ghost == ghost {
            return;
        }
        self.ghost = ghost;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    pub(crate) fn is_composing(&self) -> bool {
        self.marked_range.is_some()
    }

    /// Replace the query from browser navigation and reset editing state without inserting text.
    pub(crate) fn set_content(&mut self, content: impl Into<SharedString>, cx: &mut Context<Self>) {
        let content = content.into();
        let content = if self.options.ascii_digits_only {
            let Ok(content) = filter_ascii_digits(&content) else {
                self.reject_edit(cx);
                return;
            };
            content.into_owned().into()
        } else {
            content
        };
        if !input_within_limit(content.len(), self.options.max_bytes)
            || !characters_within_limit(&content, self.options.max_characters)
        {
            self.reject_edit(cx);
            return;
        }
        self.content = content;
        self.ghost = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        self.is_selecting = false;
        cx.emit(SearchChanged);
        cx.notify();
    }

    pub(crate) fn select_all_content(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    pub(crate) fn theme_changed(&mut self, cx: &mut Context<Self>) {
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    fn reject_edit(&self, cx: &mut Context<Self>) {
        cx.emit(SearchInputRejected);
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            if self.options.ascii_digits_only {
                if text.len() > MAX_NUMERIC_INPUT_BYTES {
                    self.reject_edit(cx);
                    return;
                }
                let replacement = paste_replacement_text(&text, self.options.paste_newlines);
                let Ok(replacement) = filter_ascii_digits(replacement.as_ref()) else {
                    self.reject_edit(cx);
                    return;
                };
                if replacement.is_empty() {
                    return;
                }
                self.replace_text_in_range(None, replacement.as_ref(), window, cx);
                return;
            }
            if !input_within_limit(text.len(), self.options.max_bytes) {
                self.reject_edit(cx);
                return;
            }
            let replacement = paste_replacement_text(&text, self.options.paste_newlines);
            self.replace_text_in_range(None, replacement.as_ref(), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
            .min(self.content.len())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        offset_from_utf16(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let replacement = if self.options.ascii_digits_only {
            let Ok(replacement) = filter_ascii_digits(new_text) else {
                self.reject_edit(cx);
                return;
            };
            if !new_text.is_empty() && replacement.is_empty() {
                return;
            }
            replacement
        } else {
            Cow::Borrowed(new_text)
        };
        let Some(resulting_len) = plan_replacement(
            &self.content,
            &range,
            replacement.as_ref(),
            self.options.max_bytes,
        ) else {
            self.reject_edit(cx);
            return;
        };
        if !replacement_within_character_limit(
            &self.content,
            &range,
            replacement.as_ref(),
            self.options.max_characters,
        ) {
            self.reject_edit(cx);
            return;
        }
        self.content =
            replace_content(&self.content, &range, replacement.as_ref(), resulting_len).into();
        self.ghost = "".into();
        self.selected_range = range.start + replacement.len()..range.start + replacement.len();
        self.selection_reversed = false;
        self.marked_range = None;
        cx.emit(SearchChanged);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let replacement = if self.options.ascii_digits_only {
            let Ok(replacement) = filter_ascii_digits(new_text) else {
                self.reject_edit(cx);
                return;
            };
            if !new_text.is_empty() && replacement.is_empty() {
                return;
            }
            replacement
        } else {
            Cow::Borrowed(new_text)
        };
        let Some(resulting_len) = plan_replacement(
            &self.content,
            &range,
            replacement.as_ref(),
            self.options.max_bytes,
        ) else {
            self.reject_edit(cx);
            return;
        };
        if !replacement_within_character_limit(
            &self.content,
            &range,
            replacement.as_ref(),
            self.options.max_characters,
        ) {
            self.reject_edit(cx);
            return;
        }
        self.content =
            replace_content(&self.content, &range, replacement.as_ref(), resulting_len).into();
        self.ghost = "".into();
        self.marked_range =
            (!replacement.is_empty()).then_some(range.start..range.start + replacement.len());
        let selected_range = if self.options.ascii_digits_only {
            new_selected_range_utf16
                .as_ref()
                .map(|selection| filtered_selection_range_utf16(new_text, selection))
        } else {
            new_selected_range_utf16.clone()
        };
        self.selected_range =
            marked_selection_range(range.start, replacement.as_ref(), selected_range.as_ref());
        self.selection_reversed = false;
        cx.emit(SearchChanged);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        Some(
            self.offset_to_utf16(
                last_layout
                    .index_for_x(point.x - line_point.x)?
                    .min(self.content.len()),
            ),
        )
    }
}

struct TextElement {
    input: Entity<SearchInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let theme = active(cx);
        let content = input.content.clone();
        let ghost = input.ghost.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();
        let ghost_visible = !content.is_empty()
            && !ghost.is_empty()
            && selected_range.is_empty()
            && input.marked_range.is_none()
            && cursor == content.len()
            && input.focus_handle.is_focused(window);
        let (display_text, runs) = if content.is_empty() {
            let text_color = color(theme.colors.text_muted);
            let display_text = input.placeholder.clone();
            let run = TextRun {
                len: display_text.len(),
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: input.marked_range.as_ref().map(|_| UnderlineStyle {
                    color: Some(text_color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                strikethrough: None,
            };
            (display_text, vec![run])
        } else if ghost_visible {
            let mut display_text = String::with_capacity(content.len() + ghost.len());
            display_text.push_str(&content);
            display_text.push_str(&ghost);
            let content_run = TextRun {
                len: content.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let ghost_run = TextRun {
                len: ghost.len(),
                font: style.font(),
                color: color(theme.colors.text_muted),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            (display_text.into(), vec![content_run, ghost_run])
        } else {
            let run = TextRun {
                len: content.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: input.marked_range.as_ref().map(|_| UnderlineStyle {
                    color: Some(style.color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                strikethrough: None,
            };
            (content, vec![run])
        };
        let line = window.text_system().shape_line(
            display_text,
            style.font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        );
        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    color(theme.colors.accent),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    color(theme.colors.selection_hover_fill),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line = prepaint.line.take().expect("search input line");
        line.paint(bounds.origin, window.line_height(), window, cx)
            .expect("paint search input line");
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for SearchInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active(cx);
        let underlined = self.options.underlined;
        let compact_height = self.options.compact_height.map(f32::from);
        let border_edges = if underlined { 1.0 } else { 2.0 };
        let border_width = compact_height.map_or(theme.spacing.border_width, |height| {
            theme
                .spacing
                .border_width
                .max(theme.interaction.focus_outline_width)
                .min((height - 1.0).max(0.0) / border_edges)
        });
        let inner_height = compact_height.map_or(theme.spacing.control_height, |height| {
            (height - border_width * border_edges).max(1.0)
        });
        let line_height = compact_height.map_or(theme.typography.line_height, |_| {
            theme.typography.line_height.min(inner_height)
        });
        let font_size = compact_height.map_or(theme.typography.body_size, |_| {
            theme.typography.body_size.min(inner_height)
        });
        div()
            .flex()
            .key_context("SearchInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .when(!underlined, |style| {
                style.bg(color(theme.colors.surface_overlay))
            })
            .line_height(px(line_height))
            .text_size(px(font_size))
            .text_color(color(theme.colors.text_primary))
            .when(underlined, |style| style.border_b(px(border_width)))
            .when(!underlined, |style| style.border(px(border_width)))
            .border_color(color(theme.colors.border))
            .focus({
                let focus_color = color(theme.interaction.focus_outline);
                let focus_width =
                    compact_height.map_or(theme.interaction.focus_outline_width, |_| border_width);
                move |style| {
                    let style = if underlined {
                        style.border_b(px(focus_width))
                    } else {
                        style.border(px(focus_width))
                    };
                    style.border_color(focus_color)
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(inner_height))
                    .w_full()
                    .px(px(compact_height.map_or(theme.spacing.control_gap, |_| {
                        theme.spacing.control_gap.min(3.0)
                    })))
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for SearchInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn input_within_limit(value_len: usize, max_bytes: Option<usize>) -> bool {
    max_bytes.is_none_or(|max_bytes| value_len <= max_bytes)
}

fn characters_within_limit(value: &str, max_characters: Option<usize>) -> bool {
    max_characters.is_none_or(|limit| value.chars().take(limit.saturating_add(1)).count() <= limit)
}

fn replacement_within_character_limit(
    content: &str,
    range: &Range<usize>,
    replacement: &str,
    max_characters: Option<usize>,
) -> bool {
    let Some(limit) = max_characters else {
        return true;
    };
    if range.start > range.end {
        return false;
    }
    let (Some(before), Some(after)) = (content.get(..range.start), content.get(range.end..)) else {
        return false;
    };
    before
        .chars()
        .chain(replacement.chars())
        .chain(after.chars())
        .take(limit.saturating_add(1))
        .count()
        <= limit
}

fn plan_replacement(
    content: &str,
    range: &Range<usize>,
    replacement: &str,
    max_bytes: Option<usize>,
) -> Option<usize> {
    if range.start > range.end
        || range.end > content.len()
        || !content.is_char_boundary(range.start)
        || !content.is_char_boundary(range.end)
    {
        return None;
    }
    let resulting_len = content
        .len()
        .checked_sub(range.end - range.start)?
        .checked_add(replacement.len())?;
    input_within_limit(resulting_len, max_bytes).then_some(resulting_len)
}

fn replace_content(
    content: &str,
    range: &Range<usize>,
    replacement: &str,
    capacity: usize,
) -> String {
    let mut result = String::with_capacity(capacity);
    result.push_str(&content[..range.start]);
    result.push_str(replacement);
    result.push_str(&content[range.end..]);
    result
}

fn paste_replacement_text(text: &str, newlines: PasteNewlines) -> Cow<'_, str> {
    match newlines {
        PasteNewlines::AsSpaces => {
            if text.contains('\n') {
                Cow::Owned(text.replace('\n', " "))
            } else {
                Cow::Borrowed(text)
            }
        }
        PasteNewlines::AsCommas => paste_tag_newlines(text),
        PasteNewlines::Preserve => Cow::Borrowed(text),
    }
}

fn paste_tag_newlines(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|character| character == '\r' || character == '\n')
    {
        return Cow::Borrowed(text);
    }

    let mut normalized = String::with_capacity(text.len());
    let mut copied_through = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if character != '\r' && character != '\n' {
            continue;
        }
        normalized.push_str(&text[copied_through..index]);
        normalized.push(',');
        copied_through = index + character.len_utf8();
        if character == '\r'
            && let Some(&(next_index, next_character)) = characters.peek()
            && next_character == '\n'
        {
            characters.next();
            copied_through = next_index + next_character.len_utf8();
        }
    }
    normalized.push_str(&text[copied_through..]);
    Cow::Owned(normalized)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumericFilterError {
    InputTooLarge,
    TooManyDigits,
}

fn filter_ascii_digits(text: &str) -> Result<Cow<'_, str>, NumericFilterError> {
    if text.len() > MAX_NUMERIC_INPUT_BYTES {
        return Err(NumericFilterError::InputTooLarge);
    }

    let mut filtered = String::with_capacity(text.len().min(MAX_NUMERIC_DIGITS));
    let mut has_non_digit = false;
    for character in text.chars() {
        if character.is_ascii_digit() {
            if filtered.len() == MAX_NUMERIC_DIGITS {
                return Err(NumericFilterError::TooManyDigits);
            }
            filtered.push(character);
        } else {
            has_non_digit = true;
        }
    }
    if has_non_digit {
        Ok(Cow::Owned(filtered))
    } else {
        Ok(Cow::Borrowed(text))
    }
}

fn filtered_selection_range_utf16(text: &str, selection: &Range<usize>) -> Range<usize> {
    filtered_offset_from_utf16(text, selection.start)
        ..filtered_offset_from_utf16(text, selection.end)
}

fn filtered_offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut filtered_offset = 0;
    let mut utf16_offset = 0;
    for character in text.chars() {
        if utf16_offset >= offset {
            break;
        }
        utf16_offset += character.len_utf16();
        if character.is_ascii_digit() {
            filtered_offset += character.len_utf16();
        }
    }
    filtered_offset
}

fn offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

// AppKit's selection is relative to the inserted marked text, not the document or
// the replaced range. Convert against that text before applying its document offset.
fn marked_selection_range(
    insertion_start: usize,
    text: &str,
    selection_utf16: Option<&Range<usize>>,
) -> Range<usize> {
    let Some(selection) = selection_utf16 else {
        let end = insertion_start + text.len();
        return end..end;
    };
    let start = offset_from_utf16(text, selection.start);
    let end = offset_from_utf16(text, selection.end).max(start);
    insertion_start + start..insertion_start + end
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_NUMERIC_INPUT_BYTES, MAX_TAG_FILTER_INPUT_BYTES, MAX_TAG_INPUT_BYTES,
        NumericFilterError, PasteNewlines, SearchInputOptions, filter_ascii_digits,
        filtered_selection_range_utf16, input_within_limit, marked_selection_range,
        paste_replacement_text, plan_replacement,
    };

    #[test]
    fn composition_selection_uses_inserted_utf16_text_at_nonzero_offset() {
        assert_eq!(marked_selection_range(7, "é😀a", Some(&(1..3))), 9..13);
        assert_eq!(marked_selection_range(7, "é😀a", Some(&(3..3))), 13..13);
    }

    #[test]
    fn composition_selection_stays_within_replacement_text() {
        assert_eq!(marked_selection_range(4, "a", Some(&(0..99))), 4..5);
        assert_eq!(marked_selection_range(4, "a", None), 5..5);
        assert_eq!(marked_selection_range(4, "", Some(&(0..0))), 4..4);
    }

    #[test]
    fn filename_and_tag_options_keep_their_distinct_defaults() {
        let filename = SearchInputOptions::filename();
        assert_eq!(filename.placeholder, "Search filenames");
        assert_eq!(filename.max_bytes, None);
        assert_eq!(filename.paste_newlines, PasteNewlines::AsSpaces);

        let tags = SearchInputOptions::tags();
        assert_eq!(tags.placeholder, "ADD TAG");
        assert_eq!(tags.max_bytes, Some(MAX_TAG_INPUT_BYTES));
        assert_eq!(tags.paste_newlines, PasteNewlines::AsCommas);

        let tag_filter = SearchInputOptions::tag_filter();
        assert_eq!(tag_filter.placeholder, "ANY");
        assert_eq!(tag_filter.max_bytes, Some(MAX_TAG_FILTER_INPUT_BYTES));
        assert_eq!(tag_filter.paste_newlines, PasteNewlines::Preserve);
        assert!(tag_filter.underlined);

        let beat_count = SearchInputOptions::beat_count();
        assert_eq!(beat_count.placeholder, "");
        assert_eq!(beat_count.max_bytes, Some(3));
        assert_eq!(beat_count.paste_newlines, PasteNewlines::Preserve);
        assert!(beat_count.ascii_digits_only);
    }

    #[test]
    fn replacement_plan_reclaims_selected_utf8_bytes_before_checking_bound() {
        let content = "a😀b";
        assert_eq!(content.len(), 6);
        assert_eq!(plan_replacement(content, &(1..5), "é", Some(4)), Some(4));
        assert_eq!(plan_replacement(content, &(1..5), "é", Some(3)), None);
        assert_eq!(plan_replacement(content, &(1..5), "😀", Some(6)), Some(6));
    }

    #[test]
    fn oversize_replacement_plan_is_atomic_and_does_not_mutate_inputs() {
        let content = String::from("abc");
        let range = 1..1;
        assert_eq!(plan_replacement(&content, &range, "é", Some(3)), None);
        assert_eq!(content, "abc");
        assert_eq!(range, 1..1);
    }

    #[test]
    fn tag_paste_uses_separators_without_merging_crlf_labels() {
        assert_eq!(
            paste_replacement_text(
                "one\r\ntwo\nthree\rfour, five; six",
                PasteNewlines::AsCommas
            )
            .as_ref(),
            "one,two,three,four, five; six"
        );
        assert_eq!(
            paste_replacement_text("one\ntwo", PasteNewlines::AsSpaces).as_ref(),
            "one two"
        );
        assert_eq!(
            paste_replacement_text("one\ntwo", PasteNewlines::Preserve).as_ref(),
            "one\ntwo"
        );
    }

    #[test]
    fn raw_clipboard_bound_is_checked_in_bytes_before_paste_translation() {
        assert!(input_within_limit("é".len(), Some(2)));
        assert!(!input_within_limit("é".len(), Some(1)));
        assert!(!input_within_limit(
            "x".len() * (MAX_TAG_INPUT_BYTES + 1),
            Some(MAX_TAG_INPUT_BYTES)
        ));
        assert!(input_within_limit(
            "x".len() * MAX_TAG_INPUT_BYTES,
            Some(MAX_TAG_INPUT_BYTES)
        ));
    }

    #[test]
    fn beat_count_text_filters_non_ascii_digits_before_the_byte_bound() {
        assert_eq!(filter_ascii_digits("a1٢2\n3").unwrap().as_ref(), "123");
        assert_eq!(filter_ascii_digits("abc").unwrap().as_ref(), "");
        assert_eq!(filter_ascii_digits("123").unwrap().as_ref(), "123");
    }

    #[test]
    fn beat_count_large_or_overfull_paste_is_rejected_before_collection() {
        let oversized = "x".repeat(MAX_NUMERIC_INPUT_BYTES + 1);
        assert_eq!(
            filter_ascii_digits(&oversized),
            Err(NumericFilterError::InputTooLarge)
        );

        let too_many_digits = format!("x1x2x3x4{}", "x".repeat(MAX_NUMERIC_INPUT_BYTES));
        assert_eq!(
            filter_ascii_digits(&too_many_digits),
            Err(NumericFilterError::InputTooLarge)
        );
        assert_eq!(
            filter_ascii_digits("x1x2x3x4"),
            Err(NumericFilterError::TooManyDigits)
        );
    }

    #[test]
    fn beat_count_filter_maps_marked_utf16_selection() {
        assert_eq!(filtered_selection_range_utf16("a1😀2", &(1..4)), 0..1);
        assert_eq!(filtered_selection_range_utf16("a1😀2", &(2..3)), 1..1);
        assert_eq!(filtered_selection_range_utf16("abc", &(1..2)), 0..0);
    }

    #[test]
    fn character_limit_counts_scalars_and_reclaims_the_replaced_selection() {
        use super::{characters_within_limit, replacement_within_character_limit};
        assert!(characters_within_limit("a😀b", Some(3)));
        assert!(!characters_within_limit("a😀bc", Some(3)));
        assert!(!characters_within_limit("e\u{301}", Some(1)));
        assert!(replacement_within_character_limit(
            "a😀b",
            &(1..5),
            "é",
            Some(3)
        ));
        assert!(!replacement_within_character_limit(
            "a😀b",
            &(1..5),
            "éé",
            Some(3)
        ));
        assert!(!replacement_within_character_limit(
            "a😀b",
            &(2..5),
            "é",
            Some(3)
        ));
        assert!(replacement_within_character_limit(
            "a😀b",
            &(1..5),
            "long",
            None
        ));
    }

    #[gpui::test]
    fn bounded_input_preserves_text_and_composition_when_an_edit_is_rejected(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::EntityInputHandler;
        cx.set_global(crate::theme::ActiveTheme::default());
        let mut options = SearchInputOptions::filename();
        options.max_bytes = Some(12);
        options.max_characters = Some(3);
        let (input, cx) = cx.add_window_view(|_, cx| {
            super::SearchInput::new_with_options_and_content(cx, options, "a😀b")
        });
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_text_in_range(Some(1..3), "é", window, cx);
                assert_eq!(input.content(), "aéb");
                input.replace_and_mark_text_in_range(Some(1..2), "😀", Some(0..2), window, cx);
                assert_eq!(input.content(), "a😀b");
                let marked = input.marked_range.clone();
                let selected = input.selected_range.clone();
                input.replace_and_mark_text_in_range(Some(4..4), "z", Some(1..1), window, cx);
                assert_eq!(input.content(), "a😀b");
                assert_eq!(input.marked_range, marked);
                assert_eq!(input.selected_range, selected);
                input.set_content("abcd", cx);
                assert_eq!(input.content(), "a😀b");
                assert_eq!(input.marked_range, marked);
            })
        });
    }

    #[gpui::test]
    fn compact_input_keeps_eighteen_pixel_height_when_focused(cx: &mut gpui::TestAppContext) {
        use gpui::{ParentElement, Styled};
        use std::{cell::Cell, rc::Rc};
        cx.set_global(crate::theme::ActiveTheme::default());
        let mut options = SearchInputOptions::filename();
        options.compact_height = Some(18);
        let (input, cx) = cx.add_window_view(|_, cx| {
            super::SearchInput::new_with_options_and_content(cx, options, "426 ms")
        });
        for focused in [false, true] {
            if focused {
                cx.update(|window, app| window.focus(&input.read(app).focus_handle()));
            }
            let height = Rc::new(Cell::new(None));
            let measured = height.clone();
            cx.draw(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(
                    gpui::AvailableSpace::Definite(gpui::px(150.0)),
                    gpui::AvailableSpace::MinContent,
                ),
                |_, _| {
                    gpui::div()
                        .relative()
                        .w(gpui::px(150.0))
                        .child(input.clone())
                        .child(
                            gpui::canvas(
                                move |bounds, _, _| {
                                    measured.set(Some(f32::from(bounds.size.height)));
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0(),
                        )
                },
            );
            assert_eq!(height.get(), Some(18.0), "focused={focused}");
        }
    }
}
