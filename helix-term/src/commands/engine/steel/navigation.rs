//! Cursor navigation adapters exposed to Steel plugins.
//!
//! These bindings deliberately use the editor's ordinary scrolloff-aware
//! visibility behavior. Plugin jumps therefore behave like native navigation:
//! destinations already in the comfortable viewport do not move it, while
//! off-screen vertical and horizontal targets are brought into view.

use crate::commands::Context;
use helix_core::{graphemes, line_ending::line_end_char_index};
use helix_view::{Document, View};

pub(super) fn goto_line(cx: &mut Context, line: usize, extend: bool) {
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    crate::commands::push_jump(view, doc);
    move_to_line(view, doc, line, extend, scrolloff);
}

pub(super) fn goto_column(cx: &mut Context, char_index: usize, extend: bool) {
    let count = cx.count();
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    crate::commands::push_jump(view, doc);
    move_to_column(view, doc, char_index, count, extend, scrolloff);
}

fn move_to_line(view: &mut View, doc: &mut Document, line: usize, extend: bool, scrolloff: usize) {
    let text = doc.text().slice(..);
    let line = line.min(text.len_lines()).saturating_sub(1);
    let selection = doc.selection(view.id).clone().transform(|range| {
        let line_start = text.line_to_char(line);
        range.put_cursor(text, line_start, extend)
    });
    doc.set_selection(view.id, selection);
    view.ensure_cursor_in_view(doc, scrolloff);
}

fn move_to_column(
    view: &mut View,
    doc: &mut Document,
    char_index: usize,
    count: usize,
    extend: bool,
    scrolloff: usize,
) {
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id).clone().transform(|range| {
        let line = range.cursor_line(text);
        let line_start = text.line_to_char(line) + char_index;
        let line_end = line_end_char_index(&text, line);
        let pos = graphemes::nth_next_grapheme_boundary(text, line_start, count - 1).min(line_end);
        range.put_cursor(text, pos, extend)
    });
    doc.set_selection(view.id, selection);
    view.ensure_cursor_in_view(doc, scrolloff);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{move_to_column, move_to_line};
    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope, Selection};
    use helix_view::{
        editor::{Config, GutterConfig},
        graphics::Rect,
        Document, DocumentId, View,
    };

    fn document_and_view(contents: &str, width: u16, height: u16) -> (View, Document) {
        let config = Arc::new(ArcSwap::new(Arc::new(Config::default())));
        let loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let mut document = Document::from(Rope::from_str(contents), None, config, loader);
        let mut view = View::new(
            DocumentId::default(),
            GutterConfig {
                layout: Vec::new(),
                ..GutterConfig::default()
            },
        );
        view.area = Rect::new(0, 0, width, height);
        document.ensure_view_init(view.id);
        (view, document)
    }

    #[test]
    fn offscreen_line_jump_uses_scrolloff() {
        let contents = (1..=30)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let (mut view, mut document) = document_and_view(&contents, 40, 10);

        move_to_line(&mut view, &mut document, 16, false, 2);

        let text = document.text().slice(..);
        let cursor = document.selection(view.id).primary().cursor(text);
        assert_eq!(text.char_to_line(cursor), 15);
        assert_eq!(text.char_to_line(document.view_offset(view.id).anchor), 9);
        assert!(view.offset_coords_to_in_view(&document, 2).is_none());
    }

    #[test]
    fn visible_line_jump_keeps_viewport_stable() {
        let (mut view, mut document) = document_and_view("zero\none\ntwo\nthree\nfour\n", 40, 8);
        let before = document.view_offset(view.id);

        move_to_line(&mut view, &mut document, 3, false, 2);

        assert_eq!(document.view_offset(view.id), before);
    }

    #[test]
    fn offscreen_column_jump_uses_scrolloff() {
        let (mut view, mut document) = document_and_view("0123456789abcdefghij\n", 8, 3);

        move_to_column(&mut view, &mut document, 12, 1, false, 2);

        assert_eq!(document.selection(view.id), &Selection::single(12, 13));
        assert_eq!(document.view_offset(view.id).horizontal_offset, 7);
        assert!(view.offset_coords_to_in_view(&document, 2).is_none());
    }

    #[test]
    fn visible_unicode_column_keeps_viewport_and_grapheme_boundary() {
        let (mut view, mut document) = document_and_view("a界e\u{301}bcdef\n", 20, 3);
        let before = document.view_offset(view.id);

        // Character index 3 is inside the displayed e + combining-mark
        // grapheme, so navigation selects that complete grapheme.
        move_to_column(&mut view, &mut document, 3, 1, false, 2);

        assert_eq!(document.selection(view.id), &Selection::single(2, 4));
        assert_eq!(document.view_offset(view.id), before);
    }

    #[test]
    fn tiny_viewport_clamps_scrolloff() {
        let (mut view, mut document) = document_and_view("zero\none\ntwo\nthree\n", 1, 2);

        move_to_line(&mut view, &mut document, 4, false, 20);

        assert!(view.offset_coords_to_in_view(&document, 20).is_none());
    }
}
