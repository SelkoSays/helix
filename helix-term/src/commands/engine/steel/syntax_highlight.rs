//! Syntax highlight spans for plugin-rendered previews.
//!
//! A plugin that wants to draw a file the way the editor draws it cannot
//! reproduce Helix's highlighting in Steel: it would have to locate the query
//! files, resolve injections, and map captures onto theme scopes. This adapter
//! reuses Helix's own highlighter and theme instead, and hands back plain
//! spans.
//!
//! Two things are deliberate. Offsets cross the boundary as **character**
//! indices, because Steel strings are indexed by character while Helix
//! highlights in bytes; returning bytes would style multi-byte text at the
//! wrong columns. And every failure is an empty span list rather than an
//! exception, because callers ask for spans from render paths where raising is
//! not a useful outcome.

use helix_core::syntax::{HighlightEvent, Syntax};
use helix_core::{Rope, RopeSlice};
use helix_stdx::rope::RopeSliceExt;
use helix_view::graphics::Style;
use std::path::Path;
use steel::rvals::IntoSteelVal;
use steel::steel_vm::builtin::BuiltInModule;
use steel::SteelVal;

use crate::commands::engine::steel::{Context, RegisterFn, CTX};

/// Highlighting is requested from plugin render paths, so an unbounded parse
/// would stall the frame loop. Above this many bytes the adapter declines.
const HIGHLIGHT_BYTE_BUDGET: usize = 1024 * 1024;

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn_with_ctx(CTX, "syntax-highlight-language-for-path", language_for_path)
        .register_fn_with_ctx(CTX, "syntax-highlight-language-known?", language_known)
        .register_fn_with_ctx(CTX, "syntax-highlight-byte-budget", byte_budget)
        .register_fn_with_ctx(CTX, "syntax-highlight-spans", highlight_spans);
}

fn byte_budget(_cx: &mut Context) -> usize {
    HIGHLIGHT_BYTE_BUDGET
}

/// Resolve the language Helix would use for `path`, so a caller holding only a
/// path does not have to keep an extension table in Steel. The file need not
/// exist; only its name is consulted.
fn language_for_path(cx: &mut Context, path: String) -> Option<String> {
    let loader = cx.editor.syn_loader.load();
    loader
        .language_for_filename(Path::new(&path))
        .map(|language| loader.language(language).config().language_id.clone())
}

fn language_known(cx: &mut Context, language: String) -> bool {
    cx.editor
        .syn_loader
        .load()
        .language_for_name(language)
        .is_some()
}

/// Highlight `text` as `language`, returning `(start end style)` triples in
/// character indices, sorted by start and non-overlapping.
///
/// Nested captures are merged by patching outer styles with inner ones, so the
/// innermost capture wins and a consumer can render segments in one
/// left-to-right pass without keeping a style stack.
fn highlight_spans(cx: &mut Context, text: String, language: String) -> SteelVal {
    SteelVal::ListV(spans(cx, &text, &language).into())
}

fn spans(cx: &mut Context, text: &str, language: &str) -> Vec<SteelVal> {
    // Refuse before parsing rather than after: the budget exists to bound work,
    // not to bound the result.
    if text.len() > HIGHLIGHT_BYTE_BUDGET {
        return Vec::new();
    }

    let loader = cx.editor.syn_loader.load();
    let Some(language) = loader.language_for_name(language.to_string()) else {
        return Vec::new();
    };

    let rope = Rope::from_str(text);
    let source = rope.slice(..);
    // An unparsable text, or a language with no highlight query, is a normal
    // outcome for a preview of an arbitrary file.
    let Ok(syntax) = Syntax::new(source, language, &loader) else {
        return Vec::new();
    };

    let theme = &cx.editor.theme;
    let mut highlighter = syntax.highlighter(source, &loader, ..);
    let mut collector = SpanCollector::new(source);

    loop {
        let offset = highlighter.next_event_offset();
        if offset == u32::MAX {
            break;
        }
        collector.advance_to(offset);

        let (event, highlights) = highlighter.advance();
        // The base is the default style rather than `ui.text`: a span is a patch
        // the caller applies over whatever base it renders with, so an
        // unhighlighted region must produce no span at all.
        let base = match event {
            HighlightEvent::Refresh => Style::default(),
            HighlightEvent::Push => collector.style,
        };
        collector.style = highlights.fold(base, |accumulated, highlight| {
            accumulated.patch(theme.highlight(highlight))
        });
    }

    collector.finish()
}

/// Accumulates highlight events into non-overlapping character spans.
///
/// Kept separate from the highlighter so the part that is easy to get wrong —
/// byte-to-character conversion, span ordering, and dropping empty or unstyled
/// regions — can be tested without an editor.
struct SpanCollector<'a> {
    source: RopeSlice<'a>,
    collected: Vec<SteelVal>,
    start: usize,
    style: Style,
}

impl<'a> SpanCollector<'a> {
    fn new(source: RopeSlice<'a>) -> Self {
        Self {
            source,
            collected: Vec::new(),
            start: 0,
            style: Style::default(),
        }
    }

    /// Close the open span at `offset`, a byte offset into the source. Byte
    /// offsets can land inside a character, so round up to keep span
    /// boundaries on character boundaries.
    fn advance_to(&mut self, offset: u32) {
        let offset = (offset as usize).min(self.source.len_bytes());
        let position = self
            .source
            .byte_to_char(self.source.ceil_char_boundary(offset));
        self.push(position);
    }

    fn finish(mut self) -> Vec<SteelVal> {
        let end = self.source.len_chars();
        self.push(end);
        self.collected
    }

    fn push(&mut self, end: usize) {
        if end <= self.start {
            return;
        }
        if self.style != Style::default() {
            self.collected.push(SteelVal::ListV(
                vec![
                    self.start.into_steelval().unwrap(),
                    end.into_steelval().unwrap(),
                    self.style.into_steelval().unwrap(),
                ]
                .into(),
            ));
        }
        self.start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::Color;

    fn ranges(values: &[SteelVal]) -> Vec<(usize, usize)> {
        values
            .iter()
            .map(|value| {
                let SteelVal::ListV(fields) = value else {
                    panic!("span is not a list")
                };
                (
                    integer(fields.get(0).unwrap()),
                    integer(fields.get(1).unwrap()),
                )
            })
            .collect()
    }

    fn integer(value: &SteelVal) -> usize {
        match value {
            SteelVal::IntV(value) => *value as usize,
            other => panic!("span field is not an integer: {other:?}"),
        }
    }

    fn styled() -> Style {
        Style::default().fg(Color::Red)
    }

    #[test]
    fn converts_byte_offsets_to_character_indices() {
        let rope = Rope::from_str("a界b界c");
        let mut collector = SpanCollector::new(rope.slice(..));
        // "a界" is four bytes but two characters.
        collector.advance_to(4);
        collector.style = styled();
        // "a界b界" is eight bytes but four characters.
        collector.advance_to(8);
        collector.style = Style::default();
        assert_eq!(ranges(&collector.finish()), vec![(2, 4)]);
    }

    #[test]
    fn rounds_an_interior_byte_offset_up_to_a_character() {
        let rope = Rope::from_str("界界");
        let mut collector = SpanCollector::new(rope.slice(..));
        collector.style = styled();
        // Byte 1 is inside the first character; the span must not split it.
        collector.advance_to(1);
        assert_eq!(ranges(&collector.finish()), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn drops_unstyled_and_empty_regions() {
        let rope = Rope::from_str("abcdef");
        let mut collector = SpanCollector::new(rope.slice(..));
        collector.advance_to(2);
        collector.style = styled();
        collector.advance_to(2);
        collector.advance_to(4);
        collector.style = Style::default();
        assert_eq!(ranges(&collector.finish()), vec![(2, 4)]);
    }

    #[test]
    fn spans_are_sorted_and_non_overlapping() {
        let rope = Rope::from_str("abcdefgh");
        let mut collector = SpanCollector::new(rope.slice(..));
        collector.style = styled();
        collector.advance_to(2);
        collector.style = Style::default().fg(Color::Blue);
        collector.advance_to(5);
        collector.style = Style::default().fg(Color::Green);
        let spans = collector.finish();
        let ranges = ranges(&spans);
        assert_eq!(ranges, vec![(0, 2), (2, 5), (5, 8)]);
        for pair in ranges.windows(2) {
            assert!(pair[0].1 <= pair[1].0);
        }
    }

    #[test]
    fn a_style_never_extends_past_the_end_of_the_source() {
        let rope = Rope::from_str("ab");
        let mut collector = SpanCollector::new(rope.slice(..));
        collector.style = styled();
        // A highlighter that reports an offset past the end must still produce
        // a span clamped to the source.
        collector.advance_to(64);
        assert_eq!(ranges(&collector.finish()), vec![(0, 2)]);
    }
}
