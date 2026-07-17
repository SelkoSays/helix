use super::{Content, Tree};
use crate::ViewId;

impl Tree {
    /// Returns the view currently rendered fullscreen, if any.
    pub fn fullscreen_view(&self) -> Option<ViewId> {
        self.fullscreen
    }

    /// Returns whether a view should be rendered in the active layout.
    pub fn view_is_visible(&self, view_id: ViewId) -> bool {
        self.node_is_active(view_id)
            && self
                .fullscreen
                .is_none_or(|fullscreen| fullscreen == view_id)
    }

    /// Render the requested active view fullscreen without removing its siblings.
    pub fn enter_fullscreen(&mut self, view_id: ViewId) -> bool {
        if !self.node_is_active(view_id)
            || !matches!(self.nodes[view_id].content, Content::View(_))
            || self.fullscreen == Some(view_id)
        {
            return false;
        }
        self.fullscreen = Some(view_id);
        self.recalculate();
        true
    }

    /// Leave fullscreen rendering and restore the stored split geometry.
    pub fn leave_fullscreen(&mut self) -> bool {
        if self.fullscreen.take().is_none() {
            return false;
        }
        self.recalculate();
        true
    }

    /// Toggle fullscreen rendering for the focused view.
    pub fn toggle_fullscreen(&mut self) -> bool {
        if self.fullscreen.is_some() {
            self.leave_fullscreen();
        } else {
            self.enter_fullscreen(self.focus);
        }
        self.fullscreen.is_some()
    }

    /// Keep fullscreen active while focus moves to another active view.
    pub(crate) fn transfer_fullscreen(&mut self, view_id: ViewId) {
        if self.fullscreen.is_some() && self.node_is_active(view_id) {
            self.fullscreen = Some(view_id);
            self.recalculate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{graphics::Rect, DocumentId, View};

    fn two_view_tree() -> (Tree, ViewId, ViewId) {
        let mut tree = Tree::new(Rect::new(0, 0, 100, 40));
        let left = tree.insert(View::new(DocumentId::default(), Vec::new().into()));
        let right = tree.split(
            View::new(DocumentId::default(), Vec::new().into()),
            super::super::Layout::Vertical,
        );
        (tree, left, right)
    }

    #[test]
    fn fullscreen_preserves_siblings_and_restores_geometry() {
        let (mut tree, left, right) = two_view_tree();
        let left_area = tree.get(left).area;
        let right_area = tree.get(right).area;

        assert!(tree.enter_fullscreen(right));
        assert_eq!(tree.views().count(), 2);
        assert!(!tree.view_is_visible(left));
        assert!(tree.view_is_visible(right));
        assert_eq!(tree.get(right).area, tree.area());

        assert!(tree.leave_fullscreen());
        assert_eq!(tree.get(left).area, left_area);
        assert_eq!(tree.get(right).area, right_area);
    }

    #[test]
    fn fullscreen_follows_focus_and_resize() {
        let (mut tree, left, right) = two_view_tree();
        tree.enter_fullscreen(right);
        tree.transfer_fullscreen(left);
        assert_eq!(tree.fullscreen_view(), Some(left));

        let resized = Rect::new(2, 3, 77, 19);
        tree.resize(resized);
        assert_eq!(tree.get(left).area, resized);
    }

    #[test]
    fn splitting_and_closing_leave_fullscreen() {
        let (mut tree, _left, right) = two_view_tree();
        tree.enter_fullscreen(right);
        let third = tree.split(
            View::new(DocumentId::default(), Vec::new().into()),
            super::super::Layout::Horizontal,
        );
        assert_eq!(tree.fullscreen_view(), None);
        assert_eq!(tree.active_views().count(), 3);

        tree.enter_fullscreen(third);
        tree.remove(third);
        assert_eq!(tree.fullscreen_view(), None);
        assert_eq!(tree.active_views().count(), 2);
    }
}
