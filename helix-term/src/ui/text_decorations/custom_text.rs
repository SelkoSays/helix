use helix_core::{syntax::OverlayHighlights, Position};
use helix_view::{annotations::custom_text::CustomVirtualLine, Document, Theme, ViewId};

use super::{Decoration, DecorationManager};
use crate::ui::document::{LinePos, TextRenderer};

struct CustomVirtualText<'a> {
    lines: Vec<&'a CustomVirtualLine>,
    theme: &'a Theme,
}

impl Decoration for CustomVirtualText<'_> {
    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let lines: Vec<_> = self
            .lines
            .iter()
            .filter(|line| line.line == pos.doc_line)
            .collect();
        for (index, line) in lines.iter().enumerate() {
            let style = self.theme.get(&line.scope);
            renderer.set_stringn(
                renderer.viewport.x,
                pos.visual_line + virt_off.row as u16 + index as u16,
                &line.text,
                renderer.viewport.width as usize,
                style,
            );
        }
        Position::new(lines.len(), 0)
    }
}

pub(in crate::ui) fn add_custom_text_annotations<'a>(
    doc: &'a Document,
    view_id: ViewId,
    theme: &'a Theme,
    overlays: &mut Vec<OverlayHighlights>,
    decorations: &mut DecorationManager<'a>,
) {
    let Some(namespaces) = doc.custom_text_annotations(view_id) else {
        return;
    };

    for annotations in namespaces.values() {
        for highlight in &annotations.highlights {
            if let Some(scope) = theme.find_highlight(&highlight.scope) {
                overlays.push(OverlayHighlights::single(scope, highlight.range.clone()));
            }
        }
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
