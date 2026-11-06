//! Bounded native multiline text input for short issue descriptions.
//!
//! This intentionally stays private to yield-ui. It supports logical lines without soft wrapping;
//! the containing scroll view provides horizontal and vertical overflow for long reports.

use std::ops::Range;

use crate::theme::color;
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollHandle, ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

const MAX_EDIT_BYTES: usize = 60_000;

actions!(
    multiline_text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Enter,
        Paste,
        Copy,
        Cut,
    ]
);

/// Install the standard native editing actions used by the private feedback report editor.
pub(crate) fn install_keybindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("FeedbackTextInput")),
        KeyBinding::new("delete", Delete, Some("FeedbackTextInput")),
        KeyBinding::new("left", Left, Some("FeedbackTextInput")),
        KeyBinding::new("right", Right, Some("FeedbackTextInput")),
        KeyBinding::new("up", Up, Some("FeedbackTextInput")),
        KeyBinding::new("down", Down, Some("FeedbackTextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("FeedbackTextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("FeedbackTextInput")),
        KeyBinding::new("shift-up", SelectUp, Some("FeedbackTextInput")),
        KeyBinding::new("shift-down", SelectDown, Some("FeedbackTextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("FeedbackTextInput")),
        KeyBinding::new("ctrl-a", SelectAll, Some("FeedbackTextInput")),
        KeyBinding::new("home", Home, Some("FeedbackTextInput")),
        KeyBinding::new("end", End, Some("FeedbackTextInput")),
        KeyBinding::new("shift-home", SelectHome, Some("FeedbackTextInput")),
        KeyBinding::new("shift-end", SelectEnd, Some("FeedbackTextInput")),
        KeyBinding::new("enter", Enter, Some("FeedbackTextInput")),
        KeyBinding::new("cmd-v", Paste, Some("FeedbackTextInput")),
        KeyBinding::new("ctrl-v", Paste, Some("FeedbackTextInput")),
        KeyBinding::new("cmd-c", Copy, Some("FeedbackTextInput")),
        KeyBinding::new("ctrl-c", Copy, Some("FeedbackTextInput")),
        KeyBinding::new("cmd-x", Cut, Some("FeedbackTextInput")),
        KeyBinding::new("ctrl-x", Cut, Some("FeedbackTextInput")),
    ]);
}

/// Event emitted after accepted report text changes.
pub(crate) struct Changed;

/// Event emitted when an edit exceeds the report body's byte bound.
pub(crate) struct Rejected;

/// Private bounded multiline editor for the issue description.
pub(crate) struct MultilineTextInput {
    focus: FocusHandle,
    text: SharedString,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    lines: Vec<LineLayout>,
    bounds: Option<Bounds<Pixels>>,
    scroll: ScrollHandle,
    height: Pixels,
    reveal_caret_after_layout: bool,
    selecting: bool,
    preferred_x: Option<Pixels>,
}

impl gpui::EventEmitter<Changed> for MultilineTextInput {}
impl gpui::EventEmitter<Rejected> for MultilineTextInput {}

impl MultilineTextInput {
    /// Create an empty report editor.
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: "".into(),
            selection: 0..0,
            reversed: false,
            marked: None,
            lines: Vec::new(),
            bounds: None,
            scroll: ScrollHandle::new(),
            height: px(176.0),
            reveal_caret_after_layout: true,
            selecting: false,
            preferred_x: None,
        }
    }

    pub(crate) fn with_height(mut self, height: Pixels) -> Self {
        self.height = height;
        self
    }

    /// Current report text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(value) = replace_text_bounded("", 0..0, text) else {
            self.reject(cx);
            return;
        };
        self.text = value.into();
        self.selection = 0..0;
        self.reversed = false;
        self.marked = None;
        self.lines.clear();
        self.preferred_x = None;
        self.reveal_caret_after_layout = true;
        cx.emit(Changed);
        cx.notify();
    }

    fn reject(&self, cx: &mut Context<Self>) {
        cx.emit(Rejected);
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = floor_char_boundary(&self.text, offset);
        self.selection = offset..offset;
        self.reversed = false;
        self.preferred_x = None;
        self.reveal_caret_after_layout = true;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = floor_char_boundary(&self.text, offset);
        let anchor = if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        self.selection = anchor.min(offset)..anchor.max(offset);
        self.reversed = offset < anchor;
        self.preferred_x = None;
        self.reveal_caret_after_layout = true;
        cx.notify();
    }

    fn move_horizontal(&mut self, direction: isize, selecting: bool, cx: &mut Context<Self>) {
        let offset = if self.selection.is_empty() {
            grapheme_boundary(&self.text, self.cursor(), direction)
        } else if direction < 0 {
            self.selection.start
        } else {
            self.selection.end
        };
        if selecting {
            self.select_to(offset, cx)
        } else {
            self.set_cursor(offset, cx)
        }
    }

    fn move_vertical(&mut self, direction: isize, selecting: bool, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let line_index = line_index_for_offset(&self.text, cursor);
        let Some(current) = self.lines.get(line_index) else {
            return;
        };
        let x = *self
            .preferred_x
            .get_or_insert_with(|| current.shape.x_for_index(cursor - current.range.start));
        let target = if direction < 0 {
            line_index.checked_sub(1)
        } else {
            (line_index + 1 < self.lines.len()).then_some(line_index + 1)
        };
        let offset = target
            .map(|index| {
                let line = &self.lines[index];
                line.range.start
                    + line
                        .shape
                        .closest_index_for_x(x)
                        .min(line.range.end - line.range.start)
            })
            .unwrap_or_else(|| if direction < 0 { 0 } else { self.text.len() });
        if selecting {
            self.select_to(offset, cx)
        } else {
            self.set_cursor(offset, cx)
        }
        self.preferred_x = Some(x);
    }

    fn move_line_edge(&mut self, end: bool, selecting: bool, cx: &mut Context<Self>) {
        let index = line_index_for_offset(&self.text, self.cursor());
        if let Some(line) = self.lines.get(index) {
            let offset = if end {
                line.range.end
            } else {
                line.range.start
            };
            if selecting {
                self.select_to(offset, cx)
            } else {
                self.set_cursor(offset, cx)
            }
        }
    }

    fn replace(
        &mut self,
        range: Range<usize>,
        replacement: &str,
        composing: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(value) = replace_text_bounded(&self.text, range.clone(), replacement) else {
            self.reject(cx);
            return false;
        };
        self.text = value.into();
        self.selection = range.start + replacement.len()..range.start + replacement.len();
        self.reversed = false;
        self.marked = composing.then_some(range.start..range.start + replacement.len());
        self.lines.clear();
        self.preferred_x = None;
        self.reveal_caret_after_layout = true;
        cx.emit(Changed);
        cx.notify();
        true
    }

    fn ensure_cursor_visible(&mut self) {
        let index = line_index_for_offset(&self.text, self.cursor());
        let Some(line) = self.lines.get(index) else {
            return;
        };
        let mut offset = self.scroll.offset();
        let viewport = self.scroll.bounds().size;
        let max_offset = self.scroll.max_offset();
        let caret_x = line
            .shape
            .x_for_index(self.cursor().saturating_sub(line.range.start));
        let (left, top) = caret_scroll_position(
            f32::from(caret_x),
            f32::from(line.y),
            f32::from(line.height),
            f32::from(viewport.width),
            f32::from(viewport.height),
            f32::from(max_offset.width).max(0.0),
            f32::from(max_offset.height).max(0.0),
            -f32::from(offset.x),
            -f32::from(offset.y),
        );
        offset.x = px(-left);
        offset.y = px(-top);
        self.scroll.set_offset(offset);
    }

    fn mouse_offset(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line_height)) =
            (self.bounds, self.lines.first().map(|line| line.height))
        else {
            return 0;
        };
        if self.lines.is_empty() {
            return 0;
        }
        let relative_y = f32::from(position.y - bounds.top()).max(0.0);
        let line_index = hit_test_line(relative_y, f32::from(line_height), self.lines.len());
        let line = &self.lines[line_index];
        line.range.start
            + line
                .shape
                .closest_index_for_x(position.x - bounds.left())
                .min(line.range.end - line.range.start)
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            let prev = grapheme_boundary(&self.text, self.cursor(), -1);
            self.selection = prev..self.cursor();
        }
        self.replace(self.selection.clone(), "", false, cx);
        let _ = window;
    }
    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            let next = grapheme_boundary(&self.text, self.cursor(), 1);
            self.selection = self.cursor()..next;
        }
        self.replace(self.selection.clone(), "", false, cx);
    }
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.move_horizontal(-1, false, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.move_horizontal(1, false, cx);
    }
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, false, cx);
    }
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, false, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_horizontal(-1, true, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_horizontal(1, true, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, true, cx);
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_line_edge(false, false, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_line_edge(true, false, cx);
    }
    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_line_edge(false, true, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_line_edge(true, true, cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selection = 0..self.text.len();
        self.reversed = false;
        self.reveal_caret_after_layout = true;
        cx.notify();
    }
    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        let range = self
            .marked
            .clone()
            .unwrap_or_else(|| self.selection.clone());
        self.replace(range, "\n", false, cx);
        let _ = window;
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace(
                self.selection.clone(),
                &text.replace("\r\n", "\n").replace('\r', "\n"),
                false,
                cx,
            );
        }
        let _ = window;
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selection.clone()].to_owned(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selection.clone()].to_owned(),
            ));
            self.replace(self.selection.clone(), "", false, cx);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.selecting = true;
        let offset = self.mouse_offset(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx)
        } else {
            self.set_cursor(offset, cx)
        }
    }
    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.mouse_offset(event.position), cx);
        }
    }
}

impl EntityInputHandler for MultilineTextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = utf16_range(&self.text, range);
        actual.replace(to_utf16_range(&self.text, range.clone()));
        Some(self.text[range].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: to_utf16_range(&self.text, self.selection.clone()),
            reversed: self.reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|range| to_utf16_range(&self.text, range.clone()))
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| utf16_range(&self.text, r))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        self.replace(range, text, false, cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| utf16_range(&self.text, r))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        let start = range.start;
        if self.replace(range, text, !text.is_empty(), cx) {
            if let Some(selected) = selected {
                self.selection = marked_selection_range(start, text, selected);
            }
        }
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = utf16_range(&self.text, range);
        let first = line_index_for_offset(&self.text, range.start);
        let last = line_index_for_offset(&self.text, range.end);
        let a = self.lines.get(first)?;
        let b = self.lines.get(last)?;
        let first_x = a
            .shape
            .x_for_index(range.start.saturating_sub(a.range.start));
        let last_x = b.shape.x_for_index(range.end.saturating_sub(b.range.start));
        let left_x = first_x.min(last_x);
        let right_x = first_x.max(last_x);
        Some(Bounds::from_corners(
            point(bounds.left() + left_x, bounds.top() + a.y),
            point(bounds.left() + right_x, bounds.top() + b.y + b.height),
        ))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(to_utf16_offset(&self.text, self.mouse_offset(point)))
    }
}

impl Focusable for MultilineTextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[derive(Clone)]
struct LineLayout {
    range: Range<usize>,
    y: Pixels,
    height: Pixels,
    shape: ShapedLine,
}

struct TextElement {
    input: Entity<MultilineTextInput>,
}
struct PaintState {
    lines: Vec<(ShapedLine, Point<Pixels>)>,
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
}
impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PaintState;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let text = self.input.read(cx).text.to_string();
        let line_height = window.line_height();
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let mut widest = px(0.0);
        for logical in text.split('\n') {
            let display = SharedString::from(if logical.is_empty() {
                " ".to_owned()
            } else {
                logical.to_owned()
            });
            let run = TextRun {
                len: display.len(),
                font: text_style.font(),
                color: text_style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            widest = widest.max(
                window
                    .text_system()
                    .shape_line(display, font_size, &[run], None)
                    .width,
            );
        }
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.min_size.width = widest.into();
        style.size.height =
            (line_height * text.matches('\n').count().saturating_add(1) as f32).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PaintState {
        let (text, selection, cursor_offset, focused) = {
            let input = self.input.read(cx);
            (
                input.text.to_string(),
                input.selection.clone(),
                input.cursor(),
                input.focus.is_focused(window),
            )
        };
        let style = window.text_style();
        let line_height = window.line_height();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let theme = crate::theme::active(cx);
        let selection_color = color(theme.colors.selection_hover_fill);
        let cursor_color = color(theme.colors.accent);
        let mut rows = Vec::new();
        let mut start = 0;
        let mut y = px(0.0);
        for logical in text
            .split('\n')
            .chain(std::iter::once(""))
            .take(text.matches('\n').count() + 1)
        {
            let end = start + logical.len();
            let display = if logical.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(logical.to_owned())
            };
            let run = TextRun {
                len: display.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shape = window
                .text_system()
                .shape_line(display, font_size, &[run], None);
            rows.push(LineLayout {
                range: start..end,
                y,
                height: line_height,
                shape,
            });
            y += line_height;
            start = end.saturating_add(1);
        }
        let mut paint_lines = Vec::new();
        let mut selections = Vec::new();
        let cursor_line = line_index_for_offset(&text, cursor_offset);
        let mut cursor_quad = None;
        for (index, row) in rows.iter().enumerate() {
            let line_origin = point(bounds.left(), bounds.top() + row.y);
            paint_lines.push((row.shape.clone(), line_origin));
            if let Some(segment) = selection_for_line(&selection, &row.range) {
                selections.push(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + row.shape.x_for_index(segment.start - row.range.start),
                            line_origin.y,
                        ),
                        point(
                            bounds.left() + row.shape.x_for_index(segment.end - row.range.start),
                            line_origin.y + row.height,
                        ),
                    ),
                    selection_color,
                ));
            }
            if index == cursor_line && focused {
                cursor_quad = Some(fill(
                    Bounds::new(
                        point(
                            bounds.left()
                                + row
                                    .shape
                                    .x_for_index(cursor_offset.saturating_sub(row.range.start)),
                            line_origin.y,
                        ),
                        size(px(2.0), row.height),
                    ),
                    cursor_color,
                ));
            }
        }
        self.input.update(cx, |input, _| {
            input.lines = rows;
            input.bounds = Some(bounds);
            if take_caret_reveal_request(&mut input.reveal_caret_after_layout) {
                input.ensure_cursor_visible();
            }
        });
        PaintState {
            lines: paint_lines,
            selection: selections,
            cursor: cursor_quad,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut PaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for quad in state.selection.drain(..) {
            window.paint_quad(quad);
        }
        for (line, origin) in state.lines.drain(..) {
            line.paint(origin, window.line_height(), window, cx)
                .expect("feedback text line paints");
        }
        if focus.is_focused(window) {
            if let Some(cursor) = state.cursor.take() {
                window.paint_quad(cursor);
            }
        }
    }
}

impl Render for MultilineTextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .id("feedback-description-editor")
            .tab_stop(true)
            .key_context("FeedbackTextInput")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .overflow_scroll()
            .track_scroll(&self.scroll)
            .w_full()
            .h(self.height)
            .p(px(theme.spacing.control_gap))
            .bg(color(theme.colors.surface_overlay))
            .text_size(px(theme.typography.body_size))
            .text_color(color(theme.colors.text_primary))
            .line_height(px(theme.typography.line_height))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .focus({
                let focus = color(theme.interaction.focus_outline);
                move |style| style.border_color(focus)
            })
            .child(TextElement { input: cx.entity() })
    }
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn replace_text_bounded(text: &str, range: Range<usize>, replacement: &str) -> Option<String> {
    if range.start > range.end
        || range.end > text.len()
        || !text.is_char_boundary(range.start)
        || !text.is_char_boundary(range.end)
    {
        return None;
    }
    let new_len = text
        .len()
        .checked_sub(range.len())?
        .checked_add(replacement.len())?;
    if new_len > MAX_EDIT_BYTES {
        return None;
    }
    let mut value = String::with_capacity(new_len);
    value.push_str(&text[..range.start]);
    value.push_str(replacement);
    value.push_str(&text[range.end..]);
    Some(value)
}
fn hit_test_line(y: f32, line_height: f32, line_count: usize) -> usize {
    ((y.max(0.0) / line_height.max(1.0)).floor() as usize).min(line_count.saturating_sub(1))
}
fn selection_for_line(selection: &Range<usize>, line: &Range<usize>) -> Option<Range<usize>> {
    let start = selection.start.max(line.start);
    let end = selection.end.min(line.end);
    (start < end).then_some(start..end)
}
fn marked_selection_range(
    start: usize,
    marked_text: &str,
    selected_utf16: Range<usize>,
) -> Range<usize> {
    let inner = utf16_range(marked_text, selected_utf16);
    start + inner.start..start + inner.end
}
fn caret_scroll_position(
    x: f32,
    y: f32,
    line_height: f32,
    viewport_width: f32,
    viewport_height: f32,
    max_x: f32,
    max_y: f32,
    current_x: f32,
    current_y: f32,
) -> (f32, f32) {
    let left = if x < current_x {
        x
    } else if x > current_x + viewport_width {
        x - viewport_width + 2.0
    } else {
        current_x
    };
    let top = if y < current_y {
        y
    } else if y + line_height > current_y + viewport_height {
        y + line_height - viewport_height
    } else {
        current_y
    };
    (left.clamp(0.0, max_x), top.clamp(0.0, max_y))
}
fn take_caret_reveal_request(pending: &mut bool) -> bool {
    std::mem::take(pending)
}
fn utf16_offset(text: &str, offset: usize) -> usize {
    text.char_indices()
        .take_while(|(i, _)| *i < offset)
        .map(|(_, c)| c.len_utf16())
        .sum()
}
fn to_utf16_offset(text: &str, offset: usize) -> usize {
    utf16_offset(text, offset)
}
fn utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    utf16_byte_offset(text, range.start)..utf16_byte_offset(text, range.end)
}
fn utf16_byte_offset(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (i, ch) in text.char_indices() {
        if units + ch.len_utf16() > offset {
            return i;
        }
        units += ch.len_utf16();
        if units == offset {
            return i + ch.len_utf8();
        }
    }
    text.len()
}
fn to_utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    utf16_offset(text, range.start)..utf16_offset(text, range.end)
}
fn line_index_for_offset(text: &str, offset: usize) -> usize {
    text[..floor_char_boundary(text, offset)]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
}
fn grapheme_boundary(text: &str, offset: usize, direction: isize) -> usize {
    if direction < 0 {
        text.grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    } else {
        text.grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ranges_and_marked_selection_map_emoji_to_utf8_bytes() {
        let text = "a🪿é";
        assert_eq!(utf16_range(text, 1..3), 1..5);
        assert_eq!(to_utf16_range(text, 1..5), 1..3);
        assert_eq!(marked_selection_range(4, "🪿x", 0..2), 4..8);
    }

    #[test]
    fn bounded_replacement_accepts_newlines_and_rejects_oversize_edits() {
        assert_eq!(
            replace_text_bounded("a", 1..1, "\nb"),
            Some("a\nb".to_owned())
        );
        let text = "🪿".repeat(MAX_EDIT_BYTES / 4);
        assert_eq!(
            replace_text_bounded(&text, text.len()..text.len(), "x"),
            None
        );
        assert_eq!(replace_text_bounded("é", 1..1, "x"), None);
    }

    #[test]
    fn line_hit_testing_selection_and_caret_mapping_follow_logical_lines() {
        let text = "first\nsecond\nlast";
        assert_eq!(line_index_for_offset(text, 8), 1);
        assert_eq!(hit_test_line(2.0 * 20.0 + 3.0, 20.0, 3), 2);
        assert_eq!(selection_for_line(&(1..10), &(6..12)), Some(6..10));
        assert_eq!(selection_for_line(&(1..4), &(6..12)), None);
    }

    #[test]
    fn caret_scroll_keeps_late_far_right_position_visible_at_scaled_sizes() {
        let offset =
            caret_scroll_position(900.0, 640.0, 40.0, 320.0, 180.0, 700.0, 560.0, 0.0, 0.0);
        assert_eq!(offset, (582.0, 500.0));
        let doubled = caret_scroll_position(
            1_800.0, 1_280.0, 80.0, 640.0, 360.0, 1_400.0, 1_120.0, 0.0, 0.0,
        );
        assert_eq!(doubled, (1_162.0, 1_000.0));
    }

    #[test]
    fn caret_reveal_request_is_consumed_once_after_edit_or_navigation() {
        let mut pending = true;
        assert!(take_caret_reveal_request(&mut pending));
        assert!(!take_caret_reveal_request(&mut pending));
    }
}
