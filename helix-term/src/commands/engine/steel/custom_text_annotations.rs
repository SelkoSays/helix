use helix_core::text_annotations::InlineAnnotation;
use helix_view::annotations::custom_text::{
    CustomHighlight, CustomHighlightStyle, CustomInlineAnnotation, CustomLineBackground,
    CustomTextAnnotations, CustomVirtualLine,
};
use helix_view::graphics::Style;
use steel::{
    rvals::{AsRefSteelVal, SteelString},
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    SteelVal,
};

use super::{Context, CTX};

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn_with_ctx(
            CTX,
            "current-visible-line-range",
            current_visible_line_range,
        )
        .register_fn_with_ctx(
            CTX,
            "set-custom-text-annotations!",
            set_custom_text_annotations,
        )
        .register_fn_with_ctx(
            CTX,
            "set-custom-text-annotations-v2!",
            set_custom_text_annotations_v2,
        )
        .register_fn_with_ctx(
            CTX,
            "clear-custom-text-annotations!",
            clear_custom_text_annotations,
        );
}

fn current_visible_line_range(cx: &mut Context) -> Option<(usize, usize)> {
    let view_id = cx.editor.tree.focus;
    let view = cx.editor.tree.get(view_id);
    let doc = cx.editor.documents.get(&view.doc)?;
    let text = doc.text();
    let first = text.char_to_line(doc.view_offset(view_id).anchor.min(text.len_chars()));
    let last = first
        .saturating_add(view.inner_height())
        .min(text.len_lines());
    Some((first, last))
}

fn set_custom_text_annotations(
    cx: &mut Context,
    namespace: SteelString,
    inline: SteelVal,
    highlights: SteelVal,
    virtual_lines: SteelVal,
) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    let annotations = parse_annotations(
        doc.text().len_chars(),
        doc.text().len_lines().saturating_sub(1),
        inline,
        highlights,
        virtual_lines,
        SteelVal::ListV(Default::default()),
    );
    doc.set_custom_text_annotations(view_id, namespace.to_string(), annotations);
    true
}

fn set_custom_text_annotations_v2(
    cx: &mut Context,
    namespace: SteelString,
    inline: SteelVal,
    highlights: SteelVal,
    virtual_lines: SteelVal,
    line_backgrounds: SteelVal,
) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    let annotations = parse_annotations(
        doc.text().len_chars(),
        doc.text().len_lines().saturating_sub(1),
        inline,
        highlights,
        virtual_lines,
        line_backgrounds,
    );
    doc.set_custom_text_annotations(view_id, namespace.to_string(), annotations);
    true
}

fn clear_custom_text_annotations(cx: &mut Context, namespace: SteelString) -> bool {
    let view_id = cx.editor.tree.focus;
    let doc_id = cx.editor.tree.get(view_id).doc;
    let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
        return false;
    };
    doc.clear_custom_text_annotations(view_id, namespace.as_str());
    true
}

fn parse_annotations(
    char_len: usize,
    last_line: usize,
    inline: SteelVal,
    highlights: SteelVal,
    virtual_lines: SteelVal,
    line_backgrounds: SteelVal,
) -> CustomTextAnnotations {
    let inline = rows(inline)
        .into_iter()
        .filter_map(|row| match row.as_slice() {
            [char_idx, text, scope] => Some((integer(char_idx)?, string(text)?, string(scope)?)),
            _ => None,
        })
        .filter(|(char_idx, _, _)| *char_idx <= char_len)
        .map(|(char_idx, text, scope)| CustomInlineAnnotation {
            annotation: InlineAnnotation::new(char_idx, text),
            scope,
        })
        .collect();
    let highlights = rows(highlights)
        .into_iter()
        .filter_map(|row| match row.as_slice() {
            [start, end, style] => Some((integer(start)?, integer(end)?, highlight_style(style)?)),
            _ => None,
        })
        .filter_map(|(start, end, style)| {
            (start < end && end <= char_len).then_some(CustomHighlight {
                range: start..end,
                style,
            })
        })
        .collect();
    let virtual_lines = rows(virtual_lines)
        .into_iter()
        .filter_map(|row| match row.as_slice() {
            [line, text, scope] => Some((integer(line)?, string(text)?, string(scope)?, None)),
            [line, text, scope, opacity] => Some((
                integer(line)?,
                string(text)?,
                string(scope)?,
                Some(percentage(opacity)?),
            )),
            _ => None,
        })
        .filter(|(line, _, _, opacity)| *line <= last_line && opacity.is_none_or(|v| v <= 100))
        .map(
            |(line, text, scope, background_opacity)| CustomVirtualLine {
                line,
                text,
                scope,
                background_opacity,
            },
        )
        .collect();
    let line_backgrounds = rows(line_backgrounds)
        .into_iter()
        .filter_map(|row| match row.as_slice() {
            [line, scope, opacity] => Some((integer(line)?, string(scope)?, percentage(opacity)?)),
            _ => None,
        })
        .filter(|(line, _, opacity)| *line <= last_line && *opacity <= 100)
        .map(|(line, scope, opacity)| CustomLineBackground {
            line,
            scope,
            opacity,
        })
        .collect();

    CustomTextAnnotations {
        inline,
        highlights,
        virtual_lines,
        line_backgrounds,
    }
}

fn rows(value: SteelVal) -> Vec<Vec<SteelVal>> {
    match value {
        SteelVal::ListV(rows) => rows
            .iter()
            .filter_map(|row| match row {
                SteelVal::ListV(values) => Some(values.iter().cloned().collect()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn integer(value: &SteelVal) -> Option<usize> {
    match value {
        SteelVal::IntV(value) if *value >= 0 => Some(*value as usize),
        _ => None,
    }
}

fn percentage(value: &SteelVal) -> Option<u8> {
    integer(value).and_then(|value| u8::try_from(value).ok())
}

fn string(value: &SteelVal) -> Option<String> {
    match value {
        SteelVal::StringV(value) | SteelVal::SymbolV(value) => Some(value.to_string()),
        _ => None,
    }
}

fn highlight_style(value: &SteelVal) -> Option<CustomHighlightStyle> {
    if let Some(scope) = string(value) {
        return Some(CustomHighlightStyle::Scope(scope));
    }
    Style::as_ref(value)
        .ok()
        .map(|style| *style)
        .map(CustomHighlightStyle::Concrete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(values: Vec<SteelVal>) -> SteelVal {
        SteelVal::ListV(values.into())
    }

    fn string(value: &str) -> SteelVal {
        SteelVal::StringV(value.into())
    }

    #[test]
    fn parses_valid_annotations() {
        let annotations = parse_annotations(
            20,
            4,
            list(vec![list(vec![
                SteelVal::IntV(4),
                string(" blame"),
                string("ui.virtual"),
            ])]),
            list(vec![list(vec![
                SteelVal::IntV(2),
                SteelVal::IntV(8),
                string("diff.plus"),
            ])]),
            list(vec![list(vec![
                SteelVal::IntV(3),
                string("deleted"),
                string("diff.minus"),
                SteelVal::IntV(20),
            ])]),
            list(vec![list(vec![
                SteelVal::IntV(1),
                string("diff.plus"),
                SteelVal::IntV(20),
            ])]),
        );

        assert_eq!(annotations.inline.len(), 1);
        assert_eq!(annotations.highlights[0].range, 2..8);
        assert_eq!(annotations.virtual_lines[0].line, 3);
        assert_eq!(annotations.virtual_lines[0].background_opacity, Some(20));
        assert_eq!(annotations.line_backgrounds[0].line, 1);
        assert_eq!(annotations.line_backgrounds[0].opacity, 20);
    }

    #[test]
    fn ignores_malformed_and_out_of_range_annotations() {
        let annotations = parse_annotations(
            5,
            1,
            list(vec![
                list(vec![SteelVal::IntV(-1), string("bad"), string("scope")]),
                list(vec![SteelVal::IntV(6), string("bad"), string("scope")]),
                SteelVal::BoolV(false),
            ]),
            list(vec![
                list(vec![SteelVal::IntV(3), SteelVal::IntV(3), string("scope")]),
                list(vec![SteelVal::IntV(1), SteelVal::IntV(6), string("scope")]),
            ]),
            list(vec![list(vec![
                SteelVal::IntV(2),
                string("bad"),
                string("scope"),
            ])]),
            list(vec![
                list(vec![SteelVal::IntV(2), string("scope"), SteelVal::IntV(20)]),
                list(vec![
                    SteelVal::IntV(1),
                    string("scope"),
                    SteelVal::IntV(101),
                ]),
                list(vec![SteelVal::IntV(1), string("scope")]),
            ]),
        );

        assert!(annotations.inline.is_empty());
        assert!(annotations.highlights.is_empty());
        assert!(annotations.virtual_lines.is_empty());
        assert!(annotations.line_backgrounds.is_empty());
    }

    #[test]
    fn v1_virtual_rows_remain_valid_without_backgrounds() {
        let annotations = parse_annotations(
            5,
            1,
            list(vec![]),
            list(vec![]),
            list(vec![list(vec![
                SteelVal::IntV(1),
                string("old"),
                string("diff.minus"),
            ])]),
            list(vec![]),
        );

        assert_eq!(annotations.virtual_lines.len(), 1);
        assert_eq!(annotations.virtual_lines[0].background_opacity, None);
        assert!(annotations.line_backgrounds.is_empty());
    }
}
