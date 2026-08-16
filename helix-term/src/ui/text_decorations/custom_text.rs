use helix_core::{syntax::OverlayHighlights, Position};
use helix_view::{
    annotations::custom_text::{CustomHighlightStyle, CustomLineBackground, CustomVirtualLine},
    graphics::{Color, Rect, Style},
    Document, Theme, ViewId,
};

use super::{Decoration, DecorationManager};
use crate::ui::document::{LinePos, TextRenderer};

#[derive(Clone, Debug)]
pub(in crate::ui) struct ConcreteStyleRange {
    pub range: std::ops::Range<usize>,
    pub style: Style,
    pub behind_overlays: bool,
}

struct CustomVirtualText<'a> {
    lines: Vec<&'a CustomVirtualLine>,
    theme: &'a Theme,
}

struct CustomLineBackgrounds<'a> {
    lines: Vec<&'a CustomLineBackground>,
    theme: &'a Theme,
}

impl CustomVirtualText<'_> {
    fn render_lines(
        &self,
        renderer: &mut TextRenderer,
        lines: &[&CustomVirtualLine],
        start_row: u16,
    ) {
        for (index, line) in lines.iter().enumerate() {
            let row = start_row + index as u16;
            let mut style = self.theme.get(&line.scope);
            if let Some(opacity) = line.background_opacity {
                let background = background_style(self.theme, &line.scope, opacity);
                renderer.set_style(
                    Rect::new(renderer.viewport.x, row, renderer.viewport.width, 1),
                    background,
                );
                style = style.patch(background);
            }
            renderer.draw_custom_text_line(row, &line.text, style);
        }
    }
}

impl Decoration for CustomLineBackgrounds<'_> {
    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        for line in self.lines.iter().filter(|line| line.line == pos.doc_line) {
            let style = background_style(self.theme, &line.scope, line.opacity);
            renderer.set_style(
                Rect::new(
                    renderer.viewport.x,
                    pos.visual_line,
                    renderer.viewport.width,
                    1,
                ),
                style,
            );
        }
    }
}

impl Decoration for CustomVirtualText<'_> {
    fn render_leading_lines(
        &mut self,
        renderer: &mut TextRenderer,
        virt_off: Position,
    ) -> Position {
        let lines: Vec<_> = self
            .lines
            .iter()
            .copied()
            .filter(|line| line.line == -1)
            .collect();
        self.render_lines(renderer, &lines, virt_off.row as u16);
        Position::new(lines.len(), 0)
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let lines: Vec<_> = self
            .lines
            .iter()
            .copied()
            .filter(|line| line.line >= 0 && line.line as usize == pos.doc_line)
            .collect();
        self.render_lines(renderer, &lines, pos.visual_line + virt_off.row as u16);
        Position::new(lines.len(), 0)
    }
}

pub(in crate::ui) fn add_custom_text_annotations<'a>(
    doc: &'a Document,
    view_id: ViewId,
    theme: &'a Theme,
    overlays: &mut Vec<OverlayHighlights>,
    concrete: &mut Vec<ConcreteStyleRange>,
    decorations: &mut DecorationManager<'a>,
) {
    let Some(namespaces) = doc.custom_text_annotations(view_id) else {
        return;
    };

    let mut scoped: Vec<(helix_core::syntax::Highlight, Vec<std::ops::Range<usize>>)> = Vec::new();
    let text = doc.text();
    let mut line_backgrounds = Vec::new();
    for annotations in namespaces.values() {
        for highlight in &annotations.highlights {
            match &highlight.style {
                CustomHighlightStyle::Scope(scope) => {
                    if let Some(scope) = theme.find_highlight(scope) {
                        if let Some((_, ranges)) =
                            scoped.iter_mut().find(|(existing, _)| *existing == scope)
                        {
                            ranges.push(highlight.range.clone());
                        } else {
                            scoped.push((scope, vec![highlight.range.clone()]));
                        }
                    }
                }
                CustomHighlightStyle::Concrete(style) => concrete.push(ConcreteStyleRange {
                    range: highlight.range.clone(),
                    style: *style,
                    behind_overlays: false,
                }),
            }
        }
        for line in &annotations.line_backgrounds {
            // Annotation namespaces may briefly outlive the document text
            // they describe while a generated buffer is being replaced or
            // closed. Treat stale rows like every other out-of-range custom
            // annotation instead of indexing the shorter rope.
            if line.line >= text.len_lines() {
                continue;
            }
            let start = text.line_to_char(line.line);
            let end = text.line_to_char((line.line + 1).min(text.len_lines()));
            if start < end {
                concrete.push(ConcreteStyleRange {
                    range: start..end,
                    style: background_style(theme, &line.scope, line.opacity),
                    behind_overlays: true,
                });
            }
            line_backgrounds.push(line);
        }
    }

    for (highlight, ranges) in scoped {
        overlays.push(OverlayHighlights::Homogeneous {
            highlight,
            ranges: merge_ranges(ranges),
        });
    }

    if !line_backgrounds.is_empty() {
        decorations.add_decoration(CustomLineBackgrounds {
            lines: line_backgrounds,
            theme,
        });
    }

    let virtual_lines = namespaces
        .values()
        .flat_map(|annotations| annotations.virtual_lines.iter())
        .collect::<Vec<_>>();
    if !virtual_lines.is_empty() {
        decorations.add_decoration(CustomVirtualText {
            lines: virtual_lines,
            theme,
        });
    }
}

fn background_style(theme: &Theme, scope: &str, opacity: u8) -> Style {
    if let Some(background) = theme.try_get_exact(scope).and_then(|style| style.bg) {
        return Style::default().bg(background);
    }

    let foreground = theme
        .try_get(scope)
        .and_then(|style| style.fg)
        .or_else(|| theme.try_get("ui.text").and_then(|style| style.fg));
    let background = theme.try_get("ui.background").and_then(|style| style.bg);
    let mixed = blend_rgb(
        color_rgb(foreground, [220, 220, 220]),
        color_rgb(background, [0, 0, 0]),
        opacity,
    );
    Style::default().bg(if theme.is_16_color() {
        indexed_color(mixed)
    } else {
        Color::Rgb(mixed[0], mixed[1], mixed[2])
    })
}

fn blend_rgb(foreground: [u8; 3], background: [u8; 3], opacity: u8) -> [u8; 3] {
    let opacity = u16::from(opacity.min(100));
    let inverse = 100 - opacity;
    std::array::from_fn(|index| {
        ((u16::from(foreground[index]) * opacity + u16::from(background[index]) * inverse + 50)
            / 100) as u8
    })
}

fn color_rgb(color: Option<Color>, reset: [u8; 3]) -> [u8; 3] {
    match color {
        Some(Color::Rgb(red, green, blue)) => [red, green, blue],
        Some(Color::Black) => [0, 0, 0],
        Some(Color::Red) => [128, 0, 0],
        Some(Color::Green) => [0, 128, 0],
        Some(Color::Yellow) => [128, 128, 0],
        Some(Color::Blue) => [0, 0, 128],
        Some(Color::Magenta) => [128, 0, 128],
        Some(Color::Cyan) => [0, 128, 128],
        Some(Color::Gray) => [128, 128, 128],
        Some(Color::LightRed) => [255, 0, 0],
        Some(Color::LightGreen) => [0, 255, 0],
        Some(Color::LightYellow) => [255, 255, 0],
        Some(Color::LightBlue) => [0, 0, 255],
        Some(Color::LightMagenta) => [255, 0, 255],
        Some(Color::LightCyan) => [0, 255, 255],
        Some(Color::LightGray) => [192, 192, 192],
        Some(Color::White) => [255, 255, 255],
        Some(Color::Indexed(index)) => ansi256_rgb(index),
        Some(Color::Reset) | None => reset,
    }
}

fn ansi256_rgb(index: u8) -> [u8; 3] {
    if index < 16 {
        const ANSI: [[u8; 3]; 16] = [
            [0, 0, 0],
            [128, 0, 0],
            [0, 128, 0],
            [128, 128, 0],
            [0, 0, 128],
            [128, 0, 128],
            [0, 128, 128],
            [192, 192, 192],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [255, 255, 0],
            [0, 0, 255],
            [255, 0, 255],
            [0, 255, 255],
            [255, 255, 255],
        ];
        return ANSI[index as usize];
    }
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return [level, level, level];
    }
    let index = index - 16;
    let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
    [level(index / 36), level((index % 36) / 6), level(index % 6)]
}

fn indexed_color(rgb: [u8; 3]) -> Color {
    let channel = |value: u8| ((u16::from(value) * 5 + 127) / 255) as u8;
    Color::Indexed(16 + 36 * channel(rgb[0]) + 6 * channel(rgb[1]) + channel(rgb[2]))
}

fn merge_ranges(mut ranges: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if range.start <= previous.end {
                previous.end = previous.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{background_style, blend_rgb, merge_ranges};
    use helix_view::graphics::{Color, Style};
    use helix_view::Theme;

    #[test]
    fn equal_styles_are_sorted_and_coalesced() {
        assert_eq!(
            merge_ranges(vec![20..25, 0..5, 4..12, 12..15]),
            vec![0..15, 20..25]
        );
    }

    #[test]
    fn blends_foreground_over_dark_and_light_backgrounds() {
        assert_eq!(blend_rgb([100, 200, 50], [0, 0, 0], 20), [20, 40, 10]);
        assert_eq!(
            blend_rgb([100, 200, 50], [255, 255, 255], 20),
            [224, 244, 214]
        );
    }

    #[test]
    fn explicit_diff_background_wins() {
        let mut theme = Theme::default();
        theme.set(
            "diff.plus".into(),
            Style::default()
                .fg(Color::Rgb(0, 255, 0))
                .bg(Color::Rgb(1, 2, 3)),
        );
        theme.set(
            "ui.background".into(),
            Style::default().bg(Color::Rgb(20, 20, 20)),
        );
        assert_eq!(
            background_style(&theme, "diff.plus", 20).bg,
            Some(Color::Rgb(1, 2, 3))
        );
    }

    #[test]
    fn rgb_and_palette_themes_resolve_tints() {
        let mut rgb = Theme::default();
        rgb.set(
            "diff.minus".into(),
            Style::default().fg(Color::Rgb(200, 0, 0)),
        );
        rgb.set(
            "ui.background".into(),
            Style::default().bg(Color::Rgb(250, 250, 250)),
        );
        assert_eq!(
            background_style(&rgb, "diff.minus", 20).bg,
            Some(Color::Rgb(240, 200, 200))
        );

        let mut palette = Theme::default();
        palette.set("diff.minus".into(), Style::default().fg(Color::Indexed(9)));
        palette.set(
            "ui.background".into(),
            Style::default().bg(Color::Indexed(0)),
        );
        assert!(matches!(
            background_style(&palette, "diff.minus", 20).bg,
            Some(Color::Indexed(_))
        ));
    }
}
