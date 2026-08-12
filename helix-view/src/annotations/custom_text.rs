use std::collections::HashMap;

use helix_core::{
    text_annotations::{InlineAnnotation, LineAnnotation, TextAnnotations},
    Position,
};

use crate::graphics::Style;
use crate::{Document, Theme, ViewId};

#[derive(Clone, Debug, Default)]
pub struct CustomTextAnnotations {
    pub inline: Vec<CustomInlineAnnotation>,
    pub highlights: Vec<CustomHighlight>,
    pub virtual_lines: Vec<CustomVirtualLine>,
    pub line_backgrounds: Vec<CustomLineBackground>,
}

#[derive(Clone, Debug)]
pub struct CustomInlineAnnotation {
    pub annotation: InlineAnnotation,
    pub scope: String,
}

#[derive(Clone, Debug)]
pub struct CustomHighlight {
    pub range: std::ops::Range<usize>,
    pub style: CustomHighlightStyle,
}

/// A custom highlight may follow a theme scope or carry an already-resolved
/// style. Concrete styles are used by generated buffers whose spans come from
/// Helix's syntax highlighter (and by half-block image cells).
#[derive(Clone, Debug)]
pub enum CustomHighlightStyle {
    Scope(String),
    Concrete(Style),
}

#[derive(Clone, Debug)]
pub struct CustomVirtualLine {
    /// The document line after which this virtual line is rendered.
    pub line: usize,
    pub text: String,
    pub scope: String,
    /// Blend percentage used for a full-row background. An explicitly themed
    /// scope background takes precedence over the blend.
    pub background_opacity: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct CustomLineBackground {
    pub line: usize,
    pub scope: String,
    pub opacity: u8,
}

#[derive(Debug, Default)]
pub(crate) struct CustomTextAnnotationStore {
    annotations: HashMap<ViewId, HashMap<String, CustomTextAnnotations>>,
}

impl CustomTextAnnotationStore {
    pub(crate) fn get(&self, view_id: ViewId) -> Option<&HashMap<String, CustomTextAnnotations>> {
        self.annotations.get(&view_id)
    }

    pub(crate) fn set(
        &mut self,
        view_id: ViewId,
        namespace: String,
        annotations: CustomTextAnnotations,
    ) {
        self.annotations
            .entry(view_id)
            .or_default()
            .insert(namespace, annotations);
    }

    pub(crate) fn clear(&mut self, view_id: ViewId, namespace: &str) {
        if let Some(namespaces) = self.annotations.get_mut(&view_id) {
            namespaces.remove(namespace);
            if namespaces.is_empty() {
                self.annotations.remove(&view_id);
            }
        }
    }

    pub(crate) fn remove_view(&mut self, view_id: ViewId) {
        self.annotations.remove(&view_id);
    }
}

impl Document {
    pub fn custom_text_annotations(
        &self,
        view_id: ViewId,
    ) -> Option<&HashMap<String, CustomTextAnnotations>> {
        self.custom_text_annotations.get(view_id)
    }

    pub fn set_custom_text_annotations(
        &mut self,
        view_id: ViewId,
        namespace: String,
        annotations: CustomTextAnnotations,
    ) {
        self.custom_text_annotations
            .set(view_id, namespace, annotations);
    }

    pub fn clear_custom_text_annotations(&mut self, view_id: ViewId, namespace: &str) {
        self.custom_text_annotations.clear(view_id, namespace);
    }
}

struct CustomVirtualLines<'a> {
    lines: Vec<&'a CustomVirtualLine>,
}

impl LineAnnotation for CustomVirtualLines<'_> {
    fn insert_virtual_lines(
        &mut self,
        _line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        Position::new(
            self.lines
                .iter()
                .filter(|line| line.line == doc_line)
                .count(),
            0,
        )
    }
}

pub(crate) fn add_to_text_annotations<'a>(
    doc: &'a Document,
    view_id: ViewId,
    theme: Option<&Theme>,
    text_annotations: &mut TextAnnotations<'a>,
) {
    let Some(namespaces) = doc.custom_text_annotations(view_id) else {
        return;
    };

    let mut virtual_lines = Vec::new();
    for annotations in namespaces.values() {
        for inline in &annotations.inline {
            let style = theme.and_then(|theme| theme.find_highlight(&inline.scope));
            text_annotations
                .add_inline_annotations(std::slice::from_ref(&inline.annotation), style);
        }
        virtual_lines.extend(annotations.virtual_lines.iter());
    }

    if !virtual_lines.is_empty() {
        text_annotations.add_line_annotation(Box::new(CustomVirtualLines {
            lines: virtual_lines,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_and_views_are_isolated() {
        let mut views = slotmap::SlotMap::<ViewId, ()>::with_key();
        let first_view = views.insert(());
        let second_view = views.insert(());
        let mut store = CustomTextAnnotationStore::default();

        store.set(
            first_view,
            "git.blame".into(),
            CustomTextAnnotations::default(),
        );
        store.set(
            first_view,
            "git.diff".into(),
            CustomTextAnnotations::default(),
        );
        store.set(
            second_view,
            "git.blame".into(),
            CustomTextAnnotations::default(),
        );
        store.clear(first_view, "git.blame");

        assert!(!store.get(first_view).unwrap().contains_key("git.blame"));
        assert!(store.get(first_view).unwrap().contains_key("git.diff"));
        assert!(store.get(second_view).unwrap().contains_key("git.blame"));
    }

    #[test]
    fn replacing_and_removing_annotations_cleans_up_the_store() {
        let view = ViewId::default();
        let mut store = CustomTextAnnotationStore::default();
        let replacement = CustomTextAnnotations {
            virtual_lines: vec![CustomVirtualLine {
                line: 2,
                text: "replacement".into(),
                scope: "ui.text".into(),
                background_opacity: None,
            }],
            ..Default::default()
        };

        store.set(view, "git.diff".into(), CustomTextAnnotations::default());
        store.set(view, "git.diff".into(), replacement);
        assert_eq!(
            store.get(view).unwrap()["git.diff"].virtual_lines[0].text,
            "replacement"
        );

        store.clear(view, "git.diff");
        assert!(store.get(view).is_none());

        store.set(view, "git.diff".into(), CustomTextAnnotations::default());
        store.remove_view(view);
        assert!(store.get(view).is_none());
    }
}
