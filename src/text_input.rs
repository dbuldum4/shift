//! Single-line text field adapted from the GPUI 0.2 input example.

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, hsla,
    point, prelude::*, px, relative, rgb, size,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// Return `true` when the callback fully handled the paste (skip default text insert).
type PasteCallback =
    Box<dyn Fn(&ClipboardItem, &mut Window, &mut Context<TextInput>) -> bool + 'static>;

actions!(
    text_input,
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
        Paste,
        Cut,
        Copy,
        Submit,
    ]
);

type SubmitCallback = Box<dyn Fn(&str, &mut Context<TextInput>) + 'static>;

fn utf8_offset_from_utf16(text: &str, offset: usize) -> usize {
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

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// When true, paint bullets instead of plaintext and block copy/cut.
    masked: bool,
    on_submit: Option<SubmitCallback>,
    on_paste: Option<PasteCallback>,
}

impl TextInput {
    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        initial: impl Into<SharedString>,
    ) -> Self {
        let content: SharedString = initial.into();
        let len = content.len();
        Self {
            focus_handle: cx.focus_handle(),
            content,
            placeholder: placeholder.into(),
            selected_range: len..len,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            masked: false,
            on_submit: None,
            on_paste: None,
        }
    }

    /// Mask the field for secrets (PDF passwords). Display uses bullets; copy/cut
    /// are disabled so the secret never enters the clipboard from this control.
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        if self.masked == masked {
            return;
        }
        self.masked = masked;
        cx.notify();
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    pub fn set_on_submit(&mut self, callback: impl Fn(&str, &mut Context<Self>) + 'static) {
        self.on_submit = Some(Box::new(callback));
    }

    /// Intercept clipboard paste. Return `true` to skip the default text insert.
    pub fn set_on_paste(
        &mut self,
        callback: impl Fn(&ClipboardItem, &mut Window, &mut Context<Self>) -> bool + 'static,
    ) {
        self.on_paste = Some(Box::new(callback));
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn set_content(&mut self, content: impl Into<SharedString>, cx: &mut Context<Self>) {
        let content: SharedString = content.into();
        let len = content.len();
        self.content = content;
        self.selected_range = len..len;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
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

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        if let Some(callback) = self.on_paste.as_ref() {
            if callback(&item, window, cx) {
                return;
            }
        }
        if let Some(text) = item.text() {
            // Keep newlines as spaces for single-line fields; magic-paste tokenizes on whitespace.
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // Never place secrets on the clipboard from a masked field.
        if self.masked {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.masked {
            // Allow clearing the selection without copying the secret.
            if !self.selected_range.is_empty() {
                self.replace_text_in_range(None, "", window, cx);
            }
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        let content = self.content.to_string();
        if let Some(callback) = self.on_submit.as_ref() {
            callback(&content, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
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
        utf8_offset_from_utf16(&self.content, offset)
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

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        // Accessibility / IME probes should not receive the plaintext secret.
        if self.masked {
            return Some("*".repeat(range.end.saturating_sub(range.start)));
        }
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
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
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| {
                utf8_offset_from_utf16(new_text, range_utf16.start)
                    ..utf8_offset_from_utf16(new_text, range_utf16.end)
            })
            .map(|new_range| range.start + new_range.start..range.start + new_range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
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
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();
        let masked = input.masked;

        let (display_text, text_color) = if content.is_empty() {
            (
                input.placeholder.clone(),
                // Muted gray placeholder — monochrome developer theme.
                hsla(0.0, 0.0, 0.40, 1.0),
            )
        } else if masked {
            // One ASCII mask glyph per UTF-8 byte so existing byte-offset cursor
            // math continues to line up with the shaped display string.
            let bullets = "*".repeat(content.len());
            (SharedString::from(bullets), style.color)
        } else {
            (content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    // White caret for black-and-white monospaced theme.
                    rgb(0xffffff),
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
                    hsla(0.0, 0.0, 0.55, 0.35),
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
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
        if let Some(line) = prepaint.line.take() {
            let _ = line.paint(bounds.origin, window.line_height(), window, cx);

            self.input.update(cx, |input, _cx| {
                input.last_layout = Some(line);
                input.last_bounds = Some(bounds);
            });
        }

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("text-input")
            .flex()
            .w_full()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
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
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(
                div()
                    .flex()
                    .items_center()
                    .w_full()
                    .h(px(36.0))
                    .px_3()
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Bind keyboard shortcuts used by every `TextInput` instance.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("backspace", Backspace, Some("TextInput")),
        gpui::KeyBinding::new("delete", Delete, Some("TextInput")),
        gpui::KeyBinding::new("left", Left, Some("TextInput")),
        gpui::KeyBinding::new("right", Right, Some("TextInput")),
        gpui::KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        gpui::KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        gpui::KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
        gpui::KeyBinding::new("cmd-v", Paste, Some("TextInput")),
        gpui::KeyBinding::new("cmd-c", Copy, Some("TextInput")),
        gpui::KeyBinding::new("cmd-x", Cut, Some("TextInput")),
        gpui::KeyBinding::new("home", Home, Some("TextInput")),
        gpui::KeyBinding::new("end", End, Some("TextInput")),
        gpui::KeyBinding::new("enter", Submit, Some("TextInput")),
    ]);
}

#[cfg(test)]
#[allow(unexpected_cfgs)]
mod tests {
    use super::{TextInput, utf8_offset_from_utf16};
    use gpui::{AppContext, Entity, TestAppContext};
    #[cfg(not(coverage))]
    use std::hint::black_box;
    #[cfg(not(coverage))]
    use std::time::{Duration, Instant};
    use unicode_segmentation::UnicodeSegmentation;

    /// Independent expected mapping: walk chars by UTF-16 units, return byte offset.
    fn expected_utf8_from_utf16(text: &str, offset: usize) -> usize {
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

    fn utf16_len(text: &str) -> usize {
        text.chars().map(|c| c.len_utf16()).sum()
    }

    #[test]
    fn converts_composition_offsets_within_the_inserted_text() {
        assert_eq!(utf8_offset_from_utf16("é", 0), 0);
        assert_eq!(utf8_offset_from_utf16("é", 1), "é".len());
        assert_eq!(utf8_offset_from_utf16("😀x", 2), "😀".len());
    }

    #[test]
    fn utf8_offset_from_utf16_empty_string() {
        assert_eq!(utf8_offset_from_utf16("", 0), 0);
        assert_eq!(utf8_offset_from_utf16("", 1), 0);
        assert_eq!(utf8_offset_from_utf16("", 100), 0);
    }

    #[test]
    fn utf8_offset_from_utf16_offset_zero_always_zero() {
        for text in [
            "",
            "a",
            "hello",
            "é",
            "😀",
            "日本語",
            "a🇺🇸b",
            "👨‍👩‍👧‍👦",
            "e\u{0301}",
        ] {
            assert_eq!(utf8_offset_from_utf16(text, 0), 0, "text={text:?}");
        }
    }

    #[test]
    fn utf8_offset_from_utf16_exhaustive_table() {
        // (text, utf16_offset, expected_utf8_offset)
        let cases: &[(&str, usize, usize)] = &[
            // empty
            ("", 0, 0),
            ("", 1, 0),
            ("", 99, 0),
            // ascii
            ("a", 0, 0),
            ("a", 1, 1),
            ("a", 2, 1),
            ("abc", 0, 0),
            ("abc", 1, 1),
            ("abc", 2, 2),
            ("abc", 3, 3),
            ("abc", 4, 3),
            ("hello world", 0, 0),
            ("hello world", 5, 5),
            ("hello world", 11, 11),
            ("hello world", 50, 11),
            // BMP multi-byte (1 UTF-16 unit each)
            ("é", 0, 0),
            ("é", 1, 2), // é is 2 UTF-8 bytes, 1 UTF-16 unit
            ("é", 2, 2),
            ("café", 0, 0),
            ("café", 3, 3),
            ("café", 4, 5), // é at end
            ("café", 5, 5),
            // CJK (3 UTF-8 bytes, 1 UTF-16 unit)
            ("日", 0, 0),
            ("日", 1, 3),
            ("日", 2, 3),
            ("日本語", 0, 0),
            ("日本語", 1, 3),
            ("日本語", 2, 6),
            ("日本語", 3, 9),
            ("日本語", 4, 9),
            ("a日b", 0, 0),
            ("a日b", 1, 1),
            ("a日b", 2, 4),
            ("a日b", 3, 5),
            ("a日b", 10, 5),
            // emoji / surrogate pairs (4 UTF-8 bytes, 2 UTF-16 units)
            ("😀", 0, 0),
            ("😀", 1, 4), // mid-surrogate-pair: advances whole char
            ("😀", 2, 4),
            ("😀", 3, 4),
            ("😀x", 0, 0),
            ("😀x", 1, 4),
            ("😀x", 2, 4),
            ("😀x", 3, 5),
            ("x😀", 0, 0),
            ("x😀", 1, 1),
            ("x😀", 2, 5),
            ("x😀", 3, 5),
            ("x😀", 4, 5),
            ("a😀b", 0, 0),
            ("a😀b", 1, 1),
            ("a😀b", 2, 5), // past first half of surrogate → full emoji
            ("a😀b", 3, 5),
            ("a😀b", 4, 6),
            ("a😀b", 100, 6),
            // mixed CJK + emoji + ascii
            ("a日本語😀z", 0, 0),
            ("a日本語😀z", 1, 1),  // after 'a'
            ("a日本語😀z", 2, 4),  // after 日
            ("a日本語😀z", 3, 7),  // after 本
            ("a日本語😀z", 4, 10), // after 語
            ("a日本語😀z", 5, 14), // mid emoji / after first surrogate → full
            ("a日本語😀z", 6, 14), // after emoji
            ("a日本語😀z", 7, 15), // after z
            ("a日本語😀z", 8, 15),
            // regional-indicator flag (🇺🇸 = 2 codepoints × 2 UTF-16 each)
            ("🇺🇸", 0, 0),
            ("🇺🇸", 1, 4), // mid first RI
            ("🇺🇸", 2, 4), // after first RI (U)
            ("🇺🇸", 3, 8), // mid second RI
            ("🇺🇸", 4, 8), // after both
            ("🇺🇸", 5, 8),
            ("a🇺🇸b", 0, 0),
            ("a🇺🇸b", 1, 1),
            ("a🇺🇸b", 2, 5),
            ("a🇺🇸b", 3, 5),
            ("a🇺🇸b", 4, 9),
            ("a🇺🇸b", 5, 9),
            ("a🇺🇸b", 6, 10),
            ("a🇺🇸b", 99, 10),
            // ZWJ family emoji 👨‍👩‍👧‍👦
            // 👨 U+1F468, ZWJ, 👩 U+1F469, ZWJ, 👧 U+1F467, ZWJ, 👦 U+1F466
            // Each emoji = 2 UTF-16; ZWJ U+200D = 1 UTF-16
            ("👨‍👩‍👧‍👦", 0, 0),
            ("👨‍👩‍👧‍👦", 1, 4),  // mid 👨
            ("👨‍👩‍👧‍👦", 2, 4),  // after 👨
            ("👨‍👩‍👧‍👦", 3, 7),  // after ZWJ (3 utf8)
            ("👨‍👩‍👧‍👦", 4, 11), // mid 👩
            ("👨‍👩‍👧‍👦", 5, 11), // after 👩
            ("👨‍👩‍👧‍👦", 11, "👨‍👩‍👧‍👦".len()),
            ("👨‍👩‍👧‍👦", 100, "👨‍👩‍👧‍👦".len()),
            // combining marks (e + combining acute)
            ("e\u{0301}", 0, 0),
            ("e\u{0301}", 1, 1), // after base e
            ("e\u{0301}", 2, 3), // after combining acute (2 utf8)
            ("e\u{0301}", 3, 3),
            ("ae\u{0301}b", 0, 0),
            ("ae\u{0301}b", 1, 1),
            ("ae\u{0301}b", 2, 2),
            ("ae\u{0301}b", 3, 4),
            ("ae\u{0301}b", 4, 5),
            // mixed punctuation / symbols
            ("✨", 0, 0),
            ("✨", 1, 3), // U+2728 BMP, 3 utf8, 1 utf16
            ("a✨b", 0, 0),
            ("a✨b", 1, 1),
            ("a✨b", 2, 4),
            ("a✨b", 3, 5),
            // past end far
            ("hi", 0, 0),
            ("hi", 2, 2),
            ("hi", 1000, 2),
        ];

        for &(text, utf16_offset, expected) in cases {
            let got = utf8_offset_from_utf16(text, utf16_offset);
            assert_eq!(
                got, expected,
                "utf8_offset_from_utf16({text:?}, {utf16_offset}) = {got}, expected {expected}"
            );
            // Cross-check against independent walker for char-aligned offsets
            assert_eq!(
                got,
                expected_utf8_from_utf16(text, utf16_offset),
                "independent walker disagrees for {text:?}@{utf16_offset}"
            );
            assert!(
                text.is_char_boundary(got),
                "mapped offset not on char boundary: {text:?}@{utf16_offset} → {got}"
            );
        }
    }

    #[test]
    fn utf8_offset_from_utf16_matches_walker_for_all_unit_positions() {
        let texts = [
            "",
            "ascii only",
            "café",
            "日本語テスト",
            "😀😃😄",
            "a🇺🇸b✨c",
            "👨‍👩‍👧‍👦 family",
            "e\u{0301} and o\u{0308}",
            "mixed: a日😀b🇺🇸c",
            "https://example.com/αβγ-🚀/path",
        ];
        for text in texts {
            let len = utf16_len(text);
            for offset in 0..=len + 5 {
                let got = utf8_offset_from_utf16(text, offset);
                let expect = expected_utf8_from_utf16(text, offset);
                assert_eq!(
                    got, expect,
                    "mismatch for {text:?} utf16={offset}: got {got} expect {expect}"
                );
                assert!(text.is_char_boundary(got));
            }
        }
    }

    #[test]
    fn utf8_offset_from_utf16_mid_surrogate_lands_after_full_char() {
        // For any astral char, offset of first half and second half of its
        // surrogate pair both map to the same end-of-char byte index once the
        // char is fully consumed (walker advances whole char when it starts).
        let text = "X𝄞Y"; // 𝄞 is U+1D11E musical symbol, 4 utf8 / 2 utf16
        assert_eq!(utf8_offset_from_utf16(text, 0), 0);
        assert_eq!(utf8_offset_from_utf16(text, 1), 1); // after X
        // offset 2: starts consuming 𝄞 (needs 2 utf16) → lands at end of 𝄞
        assert_eq!(utf8_offset_from_utf16(text, 2), 1 + "𝄞".len());
        assert_eq!(utf8_offset_from_utf16(text, 3), 1 + "𝄞".len());
        assert_eq!(utf8_offset_from_utf16(text, 4), text.len());
    }

    /// Caret / IME composition maps UTF-16 indices on every keystroke-adjacent path.
    /// This is a throughput benchmark; coverage instrumentation inflates wall time,
    /// so it is skipped under `cargo llvm-cov`.
    #[cfg(not(coverage))]
    #[test]
    fn utf16_offset_mapping_stays_fast_on_long_url_bar_content() {
        // Magic-paste / URL bar can hold a long path list or URL.
        let content = "https://example.com/articles/αβγ-🚀/".repeat(64)
            + &"file:///Users/me/Documents/very long name with spaces.docx;".repeat(32);
        let utf16_len: usize = content.chars().map(|c| c.len_utf16()).sum();

        let start = Instant::now();
        // 200×34 samples is enough to catch pathological O(n²) regressions
        // without flaking on a loaded CI/dev host in debug builds.
        for _ in 0..200 {
            // Sample many caret positions across the string.
            for step in 0..32 {
                let offset = (utf16_len * step) / 32;
                black_box(utf8_offset_from_utf16(&content, offset));
            }
            black_box(utf8_offset_from_utf16(&content, 0));
            black_box(utf8_offset_from_utf16(&content, utf16_len));
            black_box(utf8_offset_from_utf16(
                &content,
                utf16_len.saturating_add(50),
            ));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed <= Duration::from_secs(5),
            "utf8_offset_from_utf16×6.8k took {elapsed:?}"
        );
    }

    /// Functional counterpart to the perf test: exercises the same long content
    /// and boundary offsets so coverage is not lost when the benchmark is skipped.
    #[test]
    fn utf16_offset_mapping_is_correct_on_long_url_bar_content() {
        let content = "https://example.com/articles/αβγ-🚀/".repeat(64)
            + &"file:///Users/me/Documents/very long name with spaces.docx;".repeat(32);
        let utf16_len: usize = content.chars().map(|c| c.len_utf16()).sum();

        assert_eq!(utf8_offset_from_utf16(&content, 0), 0);
        assert_eq!(utf8_offset_from_utf16(&content, utf16_len), content.len());
        assert_eq!(
            utf8_offset_from_utf16(&content, utf16_len.saturating_add(50)),
            content.len()
        );

        for step in 0..32 {
            let offset = (utf16_len * step) / 32;
            let byte_offset = utf8_offset_from_utf16(&content, offset);
            assert!(byte_offset <= content.len());
            assert!(
                content.is_char_boundary(byte_offset),
                "offset {offset} mapped to non-char boundary {byte_offset}"
            );
        }
    }

    #[test]
    fn utf16_offset_mapping_is_monotonic() {
        let text = "a😀b✨c日本語d";
        let mut last = 0;
        let mut utf16 = 0;
        for ch in text.chars() {
            let at = utf8_offset_from_utf16(text, utf16);
            assert!(at >= last);
            last = at;
            utf16 += ch.len_utf16();
        }
        assert_eq!(utf8_offset_from_utf16(text, utf16), text.len());
    }

    fn new_input(cx: &mut TestAppContext, initial: &str) -> Entity<TextInput> {
        let initial = initial.to_owned();
        cx.new(|cx| TextInput::new(cx, "placeholder", initial))
    }

    #[gpui::test]
    async fn new_places_cursor_at_end_of_initial_content(cx: &mut TestAppContext) {
        let input = new_input(cx, "hello");
        input.read_with(cx, |input, _| {
            assert_eq!(input.content(), "hello");
            assert_eq!(input.selected_range, 5..5);
            assert!(!input.selection_reversed);
            assert!(input.marked_range.is_none());
        });

        let empty = new_input(cx, "");
        empty.read_with(cx, |input, _| {
            assert_eq!(input.content(), "");
            assert_eq!(input.selected_range, 0..0);
        });

        let unicode = new_input(cx, "日本語😀");
        unicode.read_with(cx, |input, _| {
            let len = "日本語😀".len();
            assert_eq!(input.content(), "日本語😀");
            assert_eq!(input.selected_range, len..len);
        });
    }

    #[gpui::test]
    async fn content_round_trips_unicode(cx: &mut TestAppContext) {
        let samples = [
            "",
            "ascii",
            "café résumé",
            "日本語テスト",
            "😀😃 family 👨‍👩‍👧‍👦",
            "a🇺🇸b",
            "e\u{0301} combining",
            "https://example.com/αβγ-🚀",
            "line\twith\ttabs",
        ];
        for sample in samples {
            let input = new_input(cx, sample);
            input.read_with(cx, |input, _| {
                assert_eq!(input.content(), sample);
            });
            input.update(cx, |input, cx| {
                input.set_content(format!("wrapped:{sample}"), cx);
            });
            input.read_with(cx, |input, _| {
                assert_eq!(input.content(), format!("wrapped:{sample}"));
            });
        }
    }

    #[gpui::test]
    async fn set_content_resets_selection_marked_range_and_reversed(cx: &mut TestAppContext) {
        let input = new_input(cx, "initial text");
        input.update(cx, |input, cx| {
            // Simulate a reversed selection with marked IME range.
            input.selected_range = 2..8;
            input.selection_reversed = true;
            input.marked_range = Some(3..6);
            input.is_selecting = true;

            input.set_content("new", cx);

            assert_eq!(input.content(), "new");
            assert_eq!(input.selected_range, 3..3);
            assert!(!input.selection_reversed);
            assert!(input.marked_range.is_none());
            // is_selecting is not part of set_content contract; leave as-is.
        });

        // Empty content → cursor at 0
        input.update(cx, |input, cx| {
            input.selected_range = 0..3;
            input.selection_reversed = true;
            input.marked_range = Some(0..1);
            input.set_content("", cx);
            assert_eq!(input.content(), "");
            assert_eq!(input.selected_range, 0..0);
            assert!(!input.selection_reversed);
            assert!(input.marked_range.is_none());
        });

        // Unicode length is byte length, not char count
        input.update(cx, |input, cx| {
            input.set_content("日😀", cx);
            let len = "日😀".len();
            assert_eq!(input.selected_range, len..len);
            assert_eq!(len, 3 + 4);
        });
    }

    #[gpui::test]
    async fn set_content_to_same_value_still_resets_selection(cx: &mut TestAppContext) {
        let input = new_input(cx, "same");
        input.update(cx, |input, cx| {
            input.selected_range = 0..2;
            input.selection_reversed = true;
            input.marked_range = Some(1..2);
            input.set_content("same", cx);
            assert_eq!(input.selected_range, 4..4);
            assert!(!input.selection_reversed);
            assert!(input.marked_range.is_none());
        });
    }

    #[gpui::test]
    async fn previous_and_next_boundary_respect_grapheme_clusters(cx: &mut TestAppContext) {
        // Flag is one extended grapheme (two regional indicators).
        let text = "a🇺🇸b";
        let input = new_input(cx, text);
        let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
        // Expect: "a", "🇺🇸", "b"
        assert_eq!(
            graphemes.len(),
            3,
            "unexpected grapheme split: {graphemes:?}"
        );

        input.read_with(cx, |input, _| {
            let end = text.len();
            // From end → start of last grapheme "b"
            assert_eq!(input.previous_boundary(end), graphemes[2].0);
            // From start of "b" → start of flag
            assert_eq!(input.previous_boundary(graphemes[2].0), graphemes[1].0);
            // From start of flag → start of "a" (0)
            assert_eq!(input.previous_boundary(graphemes[1].0), 0);
            // From 0 stays 0
            assert_eq!(input.previous_boundary(0), 0);

            // next from 0 → start of flag
            assert_eq!(input.next_boundary(0), graphemes[1].0);
            // next from flag start → "b"
            assert_eq!(input.next_boundary(graphemes[1].0), graphemes[2].0);
            // next from "b" → end
            assert_eq!(input.next_boundary(graphemes[2].0), end);
            // next from end stays end
            assert_eq!(input.next_boundary(end), end);
        });
    }

    #[gpui::test]
    async fn previous_and_next_boundary_family_emoji_and_combining(cx: &mut TestAppContext) {
        let family = "x👨‍👩‍👧‍👦y";
        let input = new_input(cx, family);
        let graphemes: Vec<(usize, &str)> = family.grapheme_indices(true).collect();
        assert!(
            graphemes.len() >= 3,
            "family emoji should be one cluster: {graphemes:?}"
        );

        input.read_with(cx, |input, _| {
            let end = family.len();
            // Stepping backward from end should land on grapheme starts only.
            let mut offset = end;
            let mut seen = Vec::new();
            while offset > 0 {
                let prev = input.previous_boundary(offset);
                assert!(prev < offset);
                seen.push(prev);
                offset = prev;
            }
            assert_eq!(seen.last().copied(), Some(0));
            for &idx in &seen {
                assert!(
                    graphemes.iter().any(|(g, _)| *g == idx),
                    "previous_boundary landed off grapheme at {idx}; graphemes={graphemes:?}"
                );
            }

            // Forward walk
            let mut offset = 0;
            while offset < end {
                let next = input.next_boundary(offset);
                assert!(next > offset);
                offset = next;
            }
            assert_eq!(offset, end);
        });

        // Combining mark: "e\u{0301}" is one grapheme
        let combining = "ae\u{0301}b";
        let input = new_input(cx, combining);
        let graphemes: Vec<(usize, &str)> = combining.grapheme_indices(true).collect();
        assert_eq!(
            graphemes.iter().map(|(_, g)| *g).collect::<Vec<_>>(),
            vec!["a", "e\u{0301}", "b"]
        );
        input.read_with(cx, |input, _| {
            // From after combining cluster → start of cluster (not mid base/mark)
            let after_cluster = graphemes[2].0; // start of "b"
            assert_eq!(input.previous_boundary(after_cluster), graphemes[1].0);
            assert_eq!(input.next_boundary(graphemes[1].0), after_cluster);
            // Mid-cluster byte offset still snaps to previous grapheme start
            let mid = graphemes[1].0 + 1; // inside "e\u{0301}"
            assert_eq!(input.previous_boundary(mid), graphemes[1].0);
            assert_eq!(input.next_boundary(mid), after_cluster);
        });
    }

    #[gpui::test]
    async fn previous_and_next_boundary_ascii_and_empty(cx: &mut TestAppContext) {
        let input = new_input(cx, "ab");
        input.read_with(cx, |input, _| {
            assert_eq!(input.previous_boundary(0), 0);
            assert_eq!(input.previous_boundary(1), 0);
            assert_eq!(input.previous_boundary(2), 1);
            assert_eq!(input.next_boundary(0), 1);
            assert_eq!(input.next_boundary(1), 2);
            assert_eq!(input.next_boundary(2), 2);
        });

        let empty = new_input(cx, "");
        empty.read_with(cx, |input, _| {
            assert_eq!(input.previous_boundary(0), 0);
            assert_eq!(input.next_boundary(0), 0);
        });
    }

    #[gpui::test]
    async fn move_to_collapses_selection(cx: &mut TestAppContext) {
        let input = new_input(cx, "abcdef");
        input.update(cx, |input, cx| {
            input.selected_range = 1..4;
            input.selection_reversed = true;
            input.move_to(2, cx);
            assert_eq!(input.selected_range, 2..2);
            assert_eq!(input.cursor_offset(), 2);
        });
    }

    #[gpui::test]
    async fn select_to_grows_and_can_reverse_selection(cx: &mut TestAppContext) {
        let input = new_input(cx, "abcdef");
        input.update(cx, |input, cx| {
            input.move_to(3, cx);
            // Extend right
            input.select_to(5, cx);
            assert_eq!(input.selected_range, 3..5);
            assert!(!input.selection_reversed);
            assert_eq!(input.cursor_offset(), 5);

            // Extend left past origin → reverse
            input.select_to(1, cx);
            assert_eq!(input.selected_range, 1..3);
            assert!(input.selection_reversed);
            assert_eq!(input.cursor_offset(), 1);

            // Extend further left while reversed
            input.select_to(0, cx);
            assert_eq!(input.selected_range, 0..3);
            assert!(input.selection_reversed);
            assert_eq!(input.cursor_offset(), 0);

            // Cross back past end → reverse again
            input.select_to(5, cx);
            assert_eq!(input.selected_range, 3..5);
            assert!(!input.selection_reversed);
            assert_eq!(input.cursor_offset(), 5);
        });
    }

    #[gpui::test]
    async fn cursor_offset_depends_on_selection_reversed(cx: &mut TestAppContext) {
        let input = new_input(cx, "hello");
        input.update(cx, |input, _cx| {
            input.selected_range = 1..4;
            input.selection_reversed = false;
            assert_eq!(input.cursor_offset(), 4);
            input.selection_reversed = true;
            assert_eq!(input.cursor_offset(), 1);
            input.selected_range = 2..2;
            assert_eq!(input.cursor_offset(), 2);
        });
    }

    #[gpui::test]
    async fn offset_and_range_utf16_round_trip(cx: &mut TestAppContext) {
        let text = "a😀日b";
        let input = new_input(cx, text);
        input.read_with(cx, |input, _| {
            let total_utf16 = utf16_len(text);
            for utf16 in 0..=total_utf16 {
                let utf8 = input.offset_from_utf16(utf16);
                assert_eq!(utf8, utf8_offset_from_utf16(text, utf16));
                // Round-trip only holds for char-aligned UTF-16 ends (not mid-surrogate).
                // offset_to_utf16 walks whole chars, so starting mid-pair is not recoverable.
            }
            // Char-aligned positions: after each char
            let mut utf8 = 0;
            let mut utf16 = 0;
            for ch in text.chars() {
                assert_eq!(input.offset_from_utf16(utf16), utf8);
                assert_eq!(input.offset_to_utf16(utf8), utf16);
                utf8 += ch.len_utf8();
                utf16 += ch.len_utf16();
            }
            assert_eq!(input.offset_to_utf16(text.len()), total_utf16);
            assert_eq!(input.offset_from_utf16(total_utf16), text.len());
            assert_eq!(input.offset_from_utf16(total_utf16 + 10), text.len());

            // Full range
            let r = input.range_to_utf16(&(0..text.len()));
            assert_eq!(r, 0..total_utf16);
            assert_eq!(input.range_from_utf16(&r), 0..text.len());

            // Partial: "😀日" span
            let start = "a".len();
            let end = "a😀日".len();
            let utf16_range = input.range_to_utf16(&(start..end));
            assert_eq!(input.range_from_utf16(&utf16_range), start..end);
        });
    }

    #[gpui::test]
    async fn index_for_mouse_position_without_layout_is_zero_or_len(cx: &mut TestAppContext) {
        use gpui::{point, px};

        let input = new_input(cx, "hello");
        input.read_with(cx, |input, _| {
            // No last_layout / last_bounds → 0
            assert_eq!(input.index_for_mouse_position(point(px(10.0), px(10.0))), 0);
        });

        let empty = new_input(cx, "");
        empty.read_with(cx, |input, _| {
            assert_eq!(input.index_for_mouse_position(point(px(0.0), px(0.0))), 0);
        });
    }

    #[gpui::test]
    async fn offset_to_utf16_clamps_past_end(cx: &mut TestAppContext) {
        let text = "a😀b";
        let input = new_input(cx, text);
        input.read_with(cx, |input, _| {
            let total = utf16_len(text);
            assert_eq!(input.offset_to_utf16(text.len()), total);
            assert_eq!(input.offset_to_utf16(text.len() + 50), total);
            assert_eq!(input.offset_to_utf16(0), 0);
            // Mid-byte of multi-byte char: walker advances whole chars only when
            // utf8_count reaches offset, so offset inside 😀 lands after it.
            let after_a = 1;
            let after_emoji = 1 + "😀".len();
            assert_eq!(input.offset_to_utf16(after_a), 1);
            assert_eq!(input.offset_to_utf16(after_emoji), 1 + 2);
        });
    }

    #[gpui::test]
    async fn range_utf16_empty_and_reversed_style_ranges(cx: &mut TestAppContext) {
        let text = "xy日z";
        let input = new_input(cx, text);
        input.read_with(cx, |input, _| {
            let empty = input.range_to_utf16(&(0..0));
            assert_eq!(empty, 0..0);
            assert_eq!(input.range_from_utf16(&empty), 0..0);

            let full = input.range_to_utf16(&(0..text.len()));
            assert_eq!(input.range_from_utf16(&full), 0..text.len());

            // Point range at end
            let end = text.len();
            let end_utf16 = input.offset_to_utf16(end);
            assert_eq!(input.range_to_utf16(&(end..end)), end_utf16..end_utf16);
        });
    }

    #[gpui::test]
    async fn previous_boundary_from_mid_grapheme_snaps_back(cx: &mut TestAppContext) {
        let text = "a🇺🇸b";
        let input = new_input(cx, text);
        let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
        assert_eq!(graphemes.len(), 3);
        let flag_start = graphemes[1].0;
        let flag_end = graphemes[2].0;
        assert!(flag_end > flag_start + 1);

        input.read_with(cx, |input, _| {
            // Any mid-flag offset should snap to flag start when going previous.
            for mid in (flag_start + 1)..flag_end {
                assert_eq!(
                    input.previous_boundary(mid),
                    flag_start,
                    "mid={mid} flag_start={flag_start}"
                );
            }
            // next from mid flag should land at flag_end ("b")
            for mid in flag_start..flag_end {
                assert_eq!(input.next_boundary(mid), flag_end);
            }
        });
    }

    #[gpui::test]
    async fn move_to_same_offset_and_beyond_len_behavior(cx: &mut TestAppContext) {
        let input = new_input(cx, "abcd");
        input.update(cx, |input, cx| {
            input.move_to(2, cx);
            assert_eq!(input.selected_range, 2..2);
            input.move_to(2, cx);
            assert_eq!(input.selected_range, 2..2);
            // Callers are expected to pass valid offsets; still record current contract.
            input.move_to(4, cx);
            assert_eq!(input.selected_range, 4..4);
            assert_eq!(input.cursor_offset(), 4);
        });
    }

    #[gpui::test]
    async fn select_to_same_offset_collapses_to_caret(cx: &mut TestAppContext) {
        let input = new_input(cx, "abcdef");
        input.update(cx, |input, cx| {
            input.move_to(2, cx);
            input.select_to(5, cx);
            assert_eq!(input.selected_range, 2..5);
            input.select_to(2, cx);
            assert_eq!(input.selected_range, 2..2);
            assert!(!input.selection_reversed);
            assert_eq!(input.cursor_offset(), 2);
        });
    }

    #[gpui::test]
    async fn set_content_from_shared_string_like_values(cx: &mut TestAppContext) {
        let input = new_input(cx, "old");
        input.update(cx, |input, cx| {
            input.set_content(String::from("owned"), cx);
            assert_eq!(input.content(), "owned");
            input.set_content("static", cx);
            assert_eq!(input.content(), "static");
        });
    }

    #[test]
    fn utf8_offset_from_utf16_null_and_control_chars() {
        let text = "a\0b\tc\nd";
        for offset in 0..=utf16_len(text) + 2 {
            let got = utf8_offset_from_utf16(text, offset);
            assert!(text.is_char_boundary(got) || got == text.len());
            assert_eq!(got, expected_utf8_from_utf16(text, offset));
        }
    }

    #[test]
    fn utf8_offset_from_utf16_mixed_scripts_dense() {
        let text = "Latin Ελληνικά 中文 한글 العربية 🚀✨";
        let len = utf16_len(text);
        for offset in 0..=len {
            let got = utf8_offset_from_utf16(text, offset);
            assert_eq!(got, expected_utf8_from_utf16(text, offset));
            assert!(text.is_char_boundary(got));
        }
    }

    #[gpui::test]
    async fn masked_secret_field_flags_and_preserves_content(cx: &mut TestAppContext) {
        let input = new_input(cx, "s3cret");
        input.update(cx, |input, cx| {
            assert!(!input.is_masked());
            input.set_masked(true, cx);
            assert!(input.is_masked());
            assert_eq!(input.content(), "s3cret");
            // Internal content stays plaintext for convert options; only display/copy are masked.
            input.set_content("other-secret", cx);
            assert!(input.is_masked());
            assert_eq!(input.content(), "other-secret");
            input.set_masked(false, cx);
            assert!(!input.is_masked());
            assert_eq!(input.content(), "other-secret");
        });
    }
}
