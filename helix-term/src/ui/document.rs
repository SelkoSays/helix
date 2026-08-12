use std::cmp::min;

use helix_core::doc_formatter::{DocumentFormatter, FormattedGrapheme, GraphemeSource, TextFormat};
use helix_core::graphemes::Grapheme;
use helix_core::str_utils::char_to_byte_idx;
use helix_core::syntax::{self, HighlightEvent, Highlighter, OverlayHighlights};
use helix_core::text_annotations::TextAnnotations;
use helix_core::{
    unicode::segmentation::UnicodeSegmentation, visual_offset_from_block, Position, RopeSlice,
};
use helix_stdx::rope::RopeSliceExt;
use helix_view::editor::{WhitespaceConfig, WhitespaceRenderValue};
use helix_view::graphics::Rect;
use helix_view::theme::Style;
use helix_view::view::ViewPosition;
use helix_view::{Document, Theme};
use tui::buffer::Buffer as Surface;

use crate::ui::text_decorations::custom_text::ConcreteStyleRange;
use crate::ui::text_decorations::DecorationManager;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct LinePos {
    /// Indicates whether the given visual line
    /// is the first visual line of the given document line
    pub first_visual_line: bool,
    /// The line index of the document line that contains the given visual line
    pub doc_line: usize,
    /// Vertical offset from the top of the inner view area
    pub visual_line: u16,
}

#[allow(clippy::too_many_arguments)]
pub fn render_document(
    surface: &mut Surface,
    viewport: Rect,
    doc: &Document,
    offset: ViewPosition,
    doc_annotations: &TextAnnotations,
    syntax_highlighter: Option<Highlighter<'_>>,
    overlay_highlights: Vec<syntax::OverlayHighlights>,
    concrete_highlights: Vec<ConcreteStyleRange>,
    theme: &Theme,
    decorations: DecorationManager,
) {
    let mut renderer = TextRenderer::new(
        surface,
        doc,
        theme,
        Position::new(offset.vertical_offset, offset.horizontal_offset),
        viewport,
    );

    render_text(
        &mut renderer,
        doc.text().slice(..),
        offset.anchor,
        &doc.text_format(viewport.width, Some(theme)),
        doc_annotations,
        syntax_highlighter,
        overlay_highlights,
        concrete_highlights,
        theme,
        decorations,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_text(
    renderer: &mut TextRenderer,
    text: RopeSlice<'_>,
    anchor: usize,
    text_fmt: &TextFormat,
    text_annotations: &TextAnnotations,
    syntax_highlighter: Option<Highlighter<'_>>,
    overlay_highlights: Vec<syntax::OverlayHighlights>,
    concrete_highlights: Vec<ConcreteStyleRange>,
    theme: &Theme,
    mut decorations: DecorationManager,
) {
    let row_off = visual_offset_from_block(text, anchor, anchor, text_fmt, text_annotations)
        .0
        .row;

    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, text_annotations, anchor);
    let mut syntax_highlighter =
        SyntaxHighlighter::new(syntax_highlighter, text, theme, renderer.text_style);
    let mut overlay_highlighter = OverlayHighlighter::new_at(overlay_highlights, theme, anchor);
    let (background_highlights, concrete_highlights): (Vec<_>, Vec<_>) = concrete_highlights
        .into_iter()
        .partition(|range| range.behind_overlays);
    let mut background_highlighter =
        ConcreteStyleHighlighter::new_at(background_highlights, anchor);
    let mut concrete_highlighter = ConcreteStyleHighlighter::new_at(concrete_highlights, anchor);

    let mut last_line_pos = LinePos {
        first_visual_line: false,
        doc_line: usize::MAX,
        visual_line: u16::MAX,
    };
    let mut last_line_end = 0;
    let mut is_in_indent_area = true;
    let mut last_line_indent_level = 0;
    let mut reached_view_top = false;

    loop {
        let Some(mut grapheme) = formatter.next() else {
            break;
        };

        // Skip formatter checkpoint rows before the block containing the
        // anchor, but keep traversing rows hidden by the view's vertical
        // offset. Decorations need those hidden document rows to render a
        // virtual-line block when the viewport begins in the middle of it.
        if grapheme.visual_pos.row < row_off {
            continue;
        }
        grapheme.visual_pos.row -= row_off;
        if !reached_view_top {
            decorations.prepare_for_rendering(grapheme.char_idx);
            reached_view_top = true;
        }

        // if the end of the viewport is reached stop rendering
        if grapheme.visual_pos.row as u16 >= renderer.viewport.height + renderer.offset.row as u16 {
            break;
        }

        // apply decorations before rendering a new line
        if grapheme.visual_pos.row as u16 != last_line_pos.visual_line {
            // we initiate doc_line with usize::MAX because no file
            // can reach that size (memory allocations are limited to isize::MAX)
            // initially there is no "previous" line (so doc_line is set to usize::MAX)
            // in that case we don't need to draw indent guides/virtual text
            if last_line_pos.doc_line != usize::MAX {
                // draw indent guides for the last line
                renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
                is_in_indent_area = true;
                decorations.render_virtual_lines(renderer, last_line_pos, last_line_end)
            }
            last_line_pos = LinePos {
                first_visual_line: grapheme.line_idx != last_line_pos.doc_line,
                doc_line: grapheme.line_idx,
                visual_line: grapheme.visual_pos.row as u16,
            };
            decorations.decorate_line(renderer, last_line_pos);
        }

        // Hidden real text is not drawn, but decorations still consume it and
        // line transitions above still render any visible tail of a virtual
        // block anchored to the preceding document row.
        if grapheme.visual_pos.row < renderer.offset.row {
            decorations.decorate_grapheme(renderer, &grapheme);
            continue;
        }

        // acquire the correct grapheme style
        while grapheme.char_idx >= syntax_highlighter.pos {
            syntax_highlighter.advance();
        }
        while grapheme.char_idx >= overlay_highlighter.pos {
            overlay_highlighter.advance();
        }
        background_highlighter.advance_to(grapheme.char_idx);
        concrete_highlighter.advance_to(grapheme.char_idx);

        let grapheme_style = if let GraphemeSource::VirtualText { highlight } = grapheme.source {
            let mut style = renderer.text_style;
            if let Some(highlight) = highlight {
                style = style.patch(theme.highlight(highlight));
            }
            GraphemeStyle {
                syntax_style: style,
                overlay_style: Style::default(),
            }
        } else {
            GraphemeStyle {
                syntax_style: syntax_highlighter.style.patch(background_highlighter.style),
                overlay_style: overlay_highlighter.style.patch(concrete_highlighter.style),
            }
        };
        decorations.decorate_grapheme(renderer, &grapheme);

        let virt = grapheme.is_virtual();
        let grapheme_width = renderer.draw_grapheme(
            &grapheme,
            grapheme_style,
            virt,
            &mut last_line_indent_level,
            &mut is_in_indent_area,
            grapheme.visual_pos,
        );
        last_line_end = grapheme.visual_pos.col + grapheme_width;
    }

    renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
    decorations.render_virtual_lines(renderer, last_line_pos, last_line_end)
}

struct ConcreteStyleHighlighter {
    ranges: Vec<ConcreteStyleRange>,
    next: usize,
    active: Vec<(usize, Style)>,
    style: Style,
}

impl ConcreteStyleHighlighter {
    fn new_at(mut ranges: Vec<ConcreteStyleRange>, position: usize) -> Self {
        ranges.sort_by_key(|range| (range.range.start, range.range.end));
        let next = ranges.partition_point(|range| range.range.start <= position);
        let active = ranges[..next]
            .iter()
            .filter(|range| range.range.end > position)
            .map(|range| (range.range.end, range.style))
            .collect();
        Self {
            ranges,
            next,
            active,
            style: Style::default(),
        }
    }

    fn advance_to(&mut self, position: usize) {
        self.active.retain(|(end, _)| *end > position);
        while let Some(range) = self.ranges.get(self.next) {
            if range.range.start > position {
                break;
            }
            if range.range.end > position {
                self.active.push((range.range.end, range.style));
            }
            self.next += 1;
        }
        self.style = self
            .active
            .iter()
            .fold(Style::default(), |style, (_, next)| style.patch(*next));
    }
}

#[derive(Debug)]
pub struct TextRenderer<'a> {
    surface: &'a mut Surface,
    pub text_style: Style,
    pub whitespace_style: Style,
    pub indent_guide_char: String,
    pub indent_guide_style: Style,
    pub newline: String,
    pub nbsp: String,
    pub nnbsp: String,
    pub space: String,
    pub tab: String,
    pub virtual_tab: String,
    pub indent_width: u16,
    pub starting_indent: usize,
    pub draw_indent_guides: bool,
    pub viewport: Rect,
    pub offset: Position,
}

pub struct GraphemeStyle {
    syntax_style: Style,
    overlay_style: Style,
}

impl<'a> TextRenderer<'a> {
    pub fn new(
        surface: &'a mut Surface,
        doc: &Document,
        theme: &Theme,
        offset: Position,
        viewport: Rect,
    ) -> TextRenderer<'a> {
        let editor_config = doc.config.load();
        let WhitespaceConfig {
            render: ws_render,
            characters: ws_chars,
        } = &editor_config.whitespace;

        let tab_width = doc.tab_width();
        let tab = if ws_render.tab() == WhitespaceRenderValue::All {
            std::iter::once(ws_chars.tab)
                .chain(std::iter::repeat_n(ws_chars.tabpad, tab_width - 1))
                .collect()
        } else {
            " ".repeat(tab_width)
        };
        let virtual_tab = " ".repeat(tab_width);
        let newline = if ws_render.newline() == WhitespaceRenderValue::All {
            ws_chars.newline.into()
        } else {
            " ".to_owned()
        };

        let space = if ws_render.space() == WhitespaceRenderValue::All {
            ws_chars.space.into()
        } else {
            " ".to_owned()
        };
        let nbsp = if ws_render.nbsp() == WhitespaceRenderValue::All {
            ws_chars.nbsp.into()
        } else {
            " ".to_owned()
        };
        let nnbsp = if ws_render.nnbsp() == WhitespaceRenderValue::All {
            ws_chars.nnbsp.into()
        } else {
            " ".to_owned()
        };

        let text_style = theme.get("ui.text");

        let indent_width = doc.indent_style.indent_width(tab_width) as u16;

        TextRenderer {
            surface,
            indent_guide_char: editor_config.indent_guides.character.into(),
            newline,
            nbsp,
            nnbsp,
            space,
            tab,
            virtual_tab,
            whitespace_style: theme.get("ui.virtual.whitespace"),
            indent_width,
            starting_indent: offset.col / indent_width as usize
                + !offset.col.is_multiple_of(indent_width as usize) as usize
                + editor_config.indent_guides.skip_levels as usize,
            indent_guide_style: text_style.patch(
                theme
                    .try_get("ui.virtual.indent-guide")
                    .unwrap_or_else(|| theme.get("ui.virtual.whitespace")),
            ),
            text_style,
            draw_indent_guides: editor_config.indent_guides.render,
            viewport,
            offset,
        }
    }
    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_decoration_grapheme(
        &mut self,
        grapheme: Grapheme,
        mut style: Style,
        mut row: u16,
        col: u16,
    ) -> bool {
        if (row as usize) < self.offset.row
            || row as usize >= self.offset.row + self.viewport.height as usize
            || col >= self.viewport.width
        {
            return false;
        }
        row -= self.offset.row as u16;
        // TODO is it correct to apply the whitspace style to all unicode white spaces?
        if grapheme.is_whitespace() {
            style = style.patch(self.whitespace_style);
        }

        let grapheme = match grapheme {
            Grapheme::Tab { width } => {
                let grapheme_tab_width = char_to_byte_idx(&self.virtual_tab, width);
                &self.virtual_tab[..grapheme_tab_width]
            }
            Grapheme::Other { ref g } if g == "\u{00A0}" => " ",
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => " ",
        };

        self.surface.set_string(
            self.viewport.x + col,
            self.viewport.y + row,
            grapheme,
            style,
        );
        true
    }

    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_grapheme(
        &mut self,
        grapheme: &FormattedGrapheme,
        grapheme_style: GraphemeStyle,
        is_virtual: bool,
        last_indent_level: &mut usize,
        is_in_indent_area: &mut bool,
        mut position: Position,
    ) -> usize {
        if position.row < self.offset.row {
            return 0;
        }
        position.row -= self.offset.row;
        let cut_off_start = self.offset.col.saturating_sub(position.col);
        let is_whitespace = grapheme.is_whitespace();

        // TODO is it correct to apply the whitespace style to all unicode white spaces?
        let mut style = grapheme_style.syntax_style;
        if is_whitespace {
            style = style.patch(self.whitespace_style);
        }
        style = style.patch(grapheme_style.overlay_style);

        let width = grapheme.width();
        let mut is_tab = false;
        let space = if is_virtual { " " } else { &self.space };
        let nbsp = if is_virtual { " " } else { &self.nbsp };
        let nnbsp = if is_virtual { " " } else { &self.nnbsp };
        let tab = if is_virtual {
            &self.virtual_tab
        } else {
            &self.tab
        };
        let grapheme = match grapheme.raw {
            Grapheme::Tab { width } => {
                is_tab = true;
                let grapheme_tab_width = char_to_byte_idx(tab, width);
                &tab[..grapheme_tab_width]
            }
            // TODO special rendering for other whitespaces?
            Grapheme::Other { ref g } if g == " " && !grapheme.source.is_eof() => space,
            Grapheme::Other { ref g } if g == "\u{00A0}" => nbsp,
            Grapheme::Other { ref g } if g == "\u{202F}" => nnbsp,
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => &self.newline,
        };

        let in_bounds = self.column_in_bounds(position.col, width);

        if in_bounds {
            let x = self.viewport.x + (position.col - self.offset.col) as u16;
            let y = self.viewport.y + position.row as u16;
            if is_tab {
                // A tab expands to `width` single-column cells; writing them
                // individually keeps background styles (selection, cursorline)
                // across the whole tab and avoids the redraw diff clipping
                // `render-whitespace` pads. A single `set_grapheme` would pack
                // them into one wide cell and leave the rest unstyled.
                self.surface.set_tab(x, y, grapheme, style);
            } else {
                self.surface.set_grapheme(x, y, grapheme, width, style);
            }
        } else if cut_off_start != 0 && cut_off_start < width {
            // partially on screen
            let rect = Rect::new(
                self.viewport.x,
                self.viewport.y + position.row as u16,
                (width - cut_off_start) as u16,
                1,
            );
            self.surface.set_style(rect, style);
        }
        if *is_in_indent_area && !is_whitespace {
            *last_indent_level = position.col;
            *is_in_indent_area = false;
        }

        width
    }

    pub fn column_in_bounds(&self, colum: usize, width: usize) -> bool {
        self.offset.col <= colum && colum + width <= self.offset.col + self.viewport.width as usize
    }

    /// Overlay indentation guides ontop of a rendered line
    /// The indentation level is computed in `draw_lines`.
    /// Therefore this function must always be called afterwards.
    pub fn draw_indent_guides(&mut self, indent_level: usize, mut row: u16) {
        if !self.draw_indent_guides
            || self.offset.row > row as usize
            || row as usize >= self.offset.row + self.viewport.height as usize
        {
            return;
        }
        row -= self.offset.row as u16;

        // Don't draw indent guides outside of view
        let end_indent = min(
            indent_level,
            // Add indent_width - 1 to round up, since the first visible
            // indent might be a bit after offset.col
            self.offset.col + self.viewport.width as usize + (self.indent_width as usize - 1),
        ) / self.indent_width as usize;

        for i in self.starting_indent..end_indent {
            let x = (self.viewport.x as usize + (i * self.indent_width as usize) - self.offset.col)
                as u16;
            let y = self.viewport.y + row;
            debug_assert!(self.surface.in_bounds(x, y));
            self.surface
                .set_string(x, y, &self.indent_guide_char, self.indent_guide_style);
        }
    }

    pub fn set_string(&mut self, x: u16, y: u16, string: &str, style: Style) {
        if (y as usize) < self.offset.row
            || y as usize >= self.offset.row + self.viewport.height as usize
        {
            return;
        }
        self.surface.set_string(
            x,
            self.viewport.y + y - self.offset.row as u16,
            string,
            style,
        )
    }

    pub fn set_stringn(&mut self, x: u16, y: u16, string: &str, width: usize, style: Style) {
        if (y as usize) < self.offset.row
            || y as usize >= self.offset.row + self.viewport.height as usize
        {
            return;
        }
        self.surface.set_stringn(
            x,
            self.viewport.y + y - self.offset.row as u16,
            string,
            width,
            style,
        );
    }

    /// Sets the style of an area **within the text viewport* this accounts
    /// both for the renderers vertical offset and its viewport
    pub fn set_style(&mut self, mut area: Rect, style: Style) {
        let top = area.y.max(self.offset.row as u16);
        let bottom = area
            .bottom()
            .min(self.offset.row as u16 + self.viewport.height);
        area.y = top;
        area.height = bottom.saturating_sub(top);
        if area.area() == 0 {
            return;
        }
        area.y = self.viewport.y + area.y - self.offset.row as u16;
        self.surface.set_style(area, style);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_string_truncated(
        &mut self,
        x: u16,
        y: u16,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style, // Map a grapheme's string offset to a style
        ellipsis: bool,
        truncate_start: bool,
    ) -> (u16, u16) {
        if (y as usize) < self.offset.row
            || y as usize >= self.offset.row + self.viewport.height as usize
        {
            return (x, y);
        }
        self.surface.set_string_truncated(
            x,
            self.viewport.y + y - self.offset.row as u16,
            string,
            width,
            style,
            ellipsis,
            truncate_start,
        )
    }

    /// Render custom virtual text with the same grapheme, tab-stop,
    /// whitespace-marker, horizontal-scroll, and viewport semantics as
    /// document text.
    pub fn draw_custom_text_line(&mut self, row: u16, text: &str, style: Style) {
        if (row as usize) < self.offset.row
            || row as usize >= self.offset.row + self.viewport.height as usize
        {
            return;
        }

        let y = self.viewport.y + row - self.offset.row as u16;
        let visible_start = self.offset.col;
        let visible_end = visible_start + self.viewport.width as usize;
        let tab_width = self.tab.chars().count() as u16;
        let mut col = 0usize;

        for value in text.graphemes(true) {
            let grapheme = Grapheme::new(value.into(), col, tab_width);
            let width = grapheme.width();
            let end = col + width;
            if end <= visible_start {
                col = end;
                continue;
            }
            if col >= visible_end {
                break;
            }

            let mut grapheme_style = style;
            if grapheme.is_whitespace() {
                grapheme_style = grapheme_style.patch(self.whitespace_style);
            }
            match grapheme {
                Grapheme::Tab { width } => {
                    let tab = &self.tab[..char_to_byte_idx(&self.tab, width)];
                    let skip = visible_start.saturating_sub(col);
                    let take = (visible_end.min(end) - col.max(visible_start)).min(width);
                    let start_byte = char_to_byte_idx(tab, skip);
                    let end_byte = char_to_byte_idx(tab, skip + take);
                    let x = self.viewport.x
                        + col.max(visible_start).saturating_sub(visible_start) as u16;
                    self.surface
                        .set_tab(x, y, &tab[start_byte..end_byte], grapheme_style);
                }
                Grapheme::Other { ref g } if g == " " => {
                    if col >= visible_start && end <= visible_end {
                        let x = self.viewport.x + (col - visible_start) as u16;
                        self.surface
                            .set_grapheme(x, y, &self.space, 1, grapheme_style);
                    }
                }
                Grapheme::Other { ref g } if g == "\u{00A0}" => {
                    if col >= visible_start && end <= visible_end {
                        let x = self.viewport.x + (col - visible_start) as u16;
                        self.surface
                            .set_grapheme(x, y, &self.nbsp, 1, grapheme_style);
                    }
                }
                Grapheme::Other { ref g } if g == "\u{202F}" => {
                    if col >= visible_start && end <= visible_end {
                        let x = self.viewport.x + (col - visible_start) as u16;
                        self.surface
                            .set_grapheme(x, y, &self.nnbsp, 1, grapheme_style);
                    }
                }
                Grapheme::Other { ref g } if col >= visible_start && end <= visible_end => {
                    let x = self.viewport.x + (col - visible_start) as u16;
                    self.surface.set_grapheme(x, y, g, width, grapheme_style);
                }
                _ => {
                    let start = col.max(visible_start);
                    let clipped_width = visible_end.min(end).saturating_sub(start);
                    if clipped_width != 0 {
                        self.surface.set_style(
                            Rect::new(
                                self.viewport.x + (start - visible_start) as u16,
                                y,
                                clipped_width as u16,
                                1,
                            ),
                            grapheme_style,
                        );
                    }
                }
            }
            col = end;
        }
    }
}

struct SyntaxHighlighter<'h, 'r, 't> {
    inner: Option<Highlighter<'h>>,
    text: RopeSlice<'r>,
    /// The character index of the next highlight event, or `usize::MAX` if the highlighter is
    /// finished.
    pos: usize,
    theme: &'t Theme,
    text_style: Style,
    style: Style,
}

impl<'h, 'r, 't> SyntaxHighlighter<'h, 'r, 't> {
    fn new(
        inner: Option<Highlighter<'h>>,
        text: RopeSlice<'r>,
        theme: &'t Theme,
        text_style: Style,
    ) -> Self {
        let mut highlighter = Self {
            inner,
            text,
            pos: 0,
            theme,
            style: text_style,
            text_style,
        };
        highlighter.update_pos();
        highlighter
    }

    fn update_pos(&mut self) {
        self.pos = self
            .inner
            .as_ref()
            .and_then(|highlighter| {
                let next_byte_idx = highlighter.next_event_offset();
                (next_byte_idx != u32::MAX).then(|| {
                    // Move the byte index to the nearest character boundary (rounding up) and
                    // convert it to a character index.
                    self.text
                        .byte_to_char(self.text.ceil_char_boundary(next_byte_idx as usize))
                })
            })
            .unwrap_or(usize::MAX);
    }

    fn advance(&mut self) {
        let Some(highlighter) = self.inner.as_mut() else {
            return;
        };

        let (event, highlights) = highlighter.advance();
        let base = match event {
            HighlightEvent::Refresh => self.text_style,
            HighlightEvent::Push => self.style,
        };

        self.style = highlights.fold(base, |acc, highlight| {
            acc.patch(self.theme.highlight(highlight))
        });
        self.update_pos();
    }
}

struct OverlayHighlighter<'t> {
    inner: syntax::OverlayHighlighter,
    pos: usize,
    theme: &'t Theme,
    style: Style,
}

impl<'t> OverlayHighlighter<'t> {
    fn new_at(overlays: Vec<OverlayHighlights>, theme: &'t Theme, position: usize) -> Self {
        let inner = syntax::OverlayHighlighter::new_at(overlays, position);
        let mut highlighter = Self {
            inner,
            pos: 0,
            theme,
            style: Style::default(),
        };
        highlighter.update_pos();
        highlighter
    }

    fn update_pos(&mut self) {
        self.pos = self.inner.next_event_offset();
    }

    fn advance(&mut self) {
        let (event, highlights) = self.inner.advance();
        let base = match event {
            HighlightEvent::Refresh => Style::default(),
            HighlightEvent::Push => self.style,
        };

        self.style = highlights.fold(base, |acc, highlight| {
            acc.patch(self.theme.highlight(highlight))
        });
        self.update_pos();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope};
    use helix_view::{
        annotations::custom_text::{
            CustomLineBackground, CustomTextAnnotations, CustomVirtualLine,
        },
        editor::{Config, GutterConfig, WhitespaceRender, WhitespaceRenderValue},
        graphics::{Color, Rect, Style},
        view::ViewPosition,
        Document, DocumentId, Theme, View,
    };

    use super::{render_document, TextRenderer};
    use crate::ui::text_decorations::{
        custom_text::add_custom_text_annotations, DecorationManager,
    };
    use tui::buffer::Buffer;

    fn document(contents: &str, whitespace: bool) -> Document {
        let mut config = Config::default();
        if whitespace {
            config.whitespace.render = WhitespaceRender::Basic(WhitespaceRenderValue::All);
        }
        Document::from(
            Rope::from_str(contents),
            None,
            Arc::new(ArcSwap::new(Arc::new(config))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    #[test]
    fn renderer_translates_and_clips_styles_to_visible_rows() {
        let doc = document("", false);
        let theme = Theme::default();
        let mut surface = Buffer::empty(Rect::new(0, 0, 10, 5));
        surface.set_string(0, 4, "STATUS", Style::default());
        let mut renderer = TextRenderer::new(
            &mut surface,
            &doc,
            &theme,
            helix_core::Position::new(2, 0),
            Rect::new(2, 1, 6, 3),
        );
        let tint = Style::default().bg(Color::Rgb(1, 2, 3));

        renderer.set_style(Rect::new(2, 3, 6, 1), tint);
        renderer.set_style(Rect::new(2, 5, 6, 1), tint);

        assert_eq!(surface[(2, 2)].bg, Color::Rgb(1, 2, 3));
        assert_eq!(surface[(2, 4)].bg, Color::Reset);
        assert_eq!(surface[(0, 4)].symbol.as_str(), "S");
    }

    #[test]
    fn custom_text_honors_tabs_horizontal_scroll_unicode_and_whitespace() {
        let doc = document("", true);
        let mut theme = Theme::default();
        theme.set(
            "diff.minus".into(),
            Style::default().fg(Color::Rgb(200, 20, 20)),
        );
        let mut surface = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut renderer = TextRenderer::new(
            &mut surface,
            &doc,
            &theme,
            helix_core::Position::new(0, 2),
            Rect::new(0, 0, 6, 1),
        );

        renderer.draw_custom_text_line(0, "a\t界 x", theme.get("diff.minus"));

        assert_eq!(surface[(0, 0)].symbol.as_str(), " ");
        assert_eq!(surface[(1, 0)].symbol.as_str(), " ");
        assert_eq!(surface[(2, 0)].symbol.as_str(), "界");
        assert_eq!(surface[(4, 0)].symbol.as_str(), "·");
        assert_eq!(surface[(5, 0)].symbol.as_str(), "x");
        assert_eq!(surface[(5, 0)].fg, Color::Rgb(200, 20, 20));
    }

    #[test]
    fn viewport_can_begin_inside_consecutive_virtual_lines() {
        let mut doc = document("anchor\nnext\n", false);
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 10, 2);
        doc.ensure_view_init(view.id);
        doc.set_custom_text_annotations(
            view.id,
            "test".into(),
            CustomTextAnnotations {
                virtual_lines: ["- one", "- two", "- three", "- four"]
                    .into_iter()
                    .map(|text| CustomVirtualLine {
                        line: 0,
                        text: text.into(),
                        scope: "diff.minus".into(),
                        background_opacity: None,
                    })
                    .collect(),
                ..Default::default()
            },
        );
        let mut theme = Theme::default();
        theme.set(
            "diff.minus".into(),
            Style::default().fg(Color::Rgb(200, 20, 20)),
        );
        let annotations = view.text_annotations(&doc, Some(&theme));
        let mut decorations = DecorationManager::default();
        let mut overlays = Vec::new();
        let mut concrete = Vec::new();
        add_custom_text_annotations(
            &doc,
            view.id,
            &theme,
            &mut overlays,
            &mut concrete,
            &mut decorations,
        );
        let mut surface = Buffer::empty(Rect::new(0, 0, 10, 3));
        surface.set_string(0, 2, "STATUS", Style::default());

        render_document(
            &mut surface,
            Rect::new(0, 0, 10, 2),
            &doc,
            ViewPosition {
                anchor: 0,
                horizontal_offset: 0,
                vertical_offset: 2,
            },
            &annotations,
            None,
            overlays,
            concrete,
            &theme,
            decorations,
        );

        let row = |y| {
            (0..10)
                .map(|x| surface[(x, y)].symbol.as_str())
                .collect::<String>()
        };
        assert!(row(0).starts_with("- two"));
        assert!(row(1).starts_with("- three"));
        assert!(row(2).starts_with("STATUS"));
    }

    #[test]
    fn selection_background_stays_above_full_row_tint() {
        let mut doc = document("added\n", false);
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 10, 1);
        doc.ensure_view_init(view.id);
        doc.set_custom_text_annotations(
            view.id,
            "test".into(),
            CustomTextAnnotations {
                line_backgrounds: vec![CustomLineBackground {
                    line: 0,
                    scope: "diff.plus".into(),
                    opacity: 20,
                }],
                ..Default::default()
            },
        );
        let mut theme = helix_view::theme::DEFAULT_THEME.clone();
        let tint = Color::Rgb(10, 40, 10);
        let selection = Color::Rgb(30, 60, 180);
        theme.set("diff.plus".into(), Style::default().bg(tint));
        theme.set("ui.selection".into(), Style::default().bg(selection));
        let selection_scope = theme.find_highlight_exact("ui.selection").unwrap();
        let annotations = view.text_annotations(&doc, Some(&theme));
        let mut decorations = DecorationManager::default();
        let mut overlays = vec![syntax::OverlayHighlights::single(selection_scope, 0..5)];
        let mut concrete = Vec::new();
        add_custom_text_annotations(
            &doc,
            view.id,
            &theme,
            &mut overlays,
            &mut concrete,
            &mut decorations,
        );
        let mut surface = Buffer::empty(Rect::new(0, 0, 10, 1));

        render_document(
            &mut surface,
            Rect::new(0, 0, 10, 1),
            &doc,
            ViewPosition::default(),
            &annotations,
            None,
            overlays,
            concrete,
            &theme,
            decorations,
        );

        assert_eq!(surface[(0, 0)].bg, selection);
        assert_eq!(surface[(9, 0)].bg, tint);
    }

    #[test]
    fn stale_line_background_is_ignored_after_document_replacement() {
        let mut doc = document("short\n", false);
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 10, 1);
        doc.ensure_view_init(view.id);
        doc.set_custom_text_annotations(
            view.id,
            "test".into(),
            CustomTextAnnotations {
                line_backgrounds: vec![CustomLineBackground {
                    line: 8,
                    scope: "diff.plus".into(),
                    opacity: 20,
                }],
                ..Default::default()
            },
        );
        let theme = helix_view::theme::DEFAULT_THEME.clone();
        let annotations = view.text_annotations(&doc, Some(&theme));
        let mut decorations = DecorationManager::default();
        let mut overlays = Vec::new();
        let mut concrete = Vec::new();
        add_custom_text_annotations(
            &doc,
            view.id,
            &theme,
            &mut overlays,
            &mut concrete,
            &mut decorations,
        );
        let mut surface = Buffer::empty(Rect::new(0, 0, 10, 1));

        render_document(
            &mut surface,
            Rect::new(0, 0, 10, 1),
            &doc,
            ViewPosition::default(),
            &annotations,
            None,
            overlays,
            concrete,
            &theme,
            decorations,
        );

        assert_eq!(surface[(9, 0)].bg, Color::Reset);
    }
}
