use super::{Content, Node, Tree};
use crate::{View, ViewId};

/// Opaque identifier for a suspended editor layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemporaryLayoutToken(pub(crate) u64);

#[derive(Debug)]
pub(super) struct TemporaryLayout {
    pub(super) token: TemporaryLayoutToken,
    pub(super) root: ViewId,
    pub(super) focus: ViewId,
    pub(super) fullscreen: Option<ViewId>,
}

impl Tree {
    pub(super) fn node_is_active(&self, mut id: ViewId) -> bool {
        if !self.nodes.contains_key(id) {
            return false;
        }
        loop {
            if id == self.root {
                return true;
            }
            let parent = self.nodes[id].parent;
            if parent == id || !self.nodes.contains_key(parent) {
                return false;
            }
            id = parent;
        }
    }

    /// Suspend the active tree and install a new single-view temporary layout.
    pub fn enter_temporary_layout(&mut self, view: View) -> (TemporaryLayoutToken, ViewId) {
        let token = TemporaryLayoutToken(self.next_temporary_layout_token);
        self.next_temporary_layout_token = self.next_temporary_layout_token.wrapping_add(1);

        self.temporary_layouts.push(TemporaryLayout {
            token,
            root: self.root,
            focus: self.focus,
            fullscreen: self.fullscreen.take(),
        });

        let root = self.nodes.insert(Node::container(super::Layout::Vertical));
        self.nodes[root].parent = root;
        self.root = root;
        self.focus = root;

        let view_id = self.nodes.insert(Node::view(view));
        if let Content::View(stored) = &mut self.nodes[view_id].content {
            stored.id = view_id;
        }
        self.nodes[view_id].parent = root;
        if let Content::Container(container) = &mut self.nodes[root].content {
            container.children.push(view_id);
            container.weights.push(1);
        }
        self.focus = view_id;
        self.recalculate();
        (token, view_id)
    }

    /// Restore the most recently suspended layout and return discarded view IDs.
    pub fn restore_temporary_layout(
        &mut self,
        token: TemporaryLayoutToken,
    ) -> Result<Vec<ViewId>, &'static str> {
        let Some(snapshot) = self.temporary_layouts.last() else {
            return Err("no temporary layout is active");
        };
        if snapshot.token != token {
            return Err("temporary layouts must be restored in LIFO order");
        }

        let snapshot = self.temporary_layouts.pop().unwrap();
        let mut stack = vec![self.root];
        let mut nodes = Vec::new();
        let mut views = Vec::new();
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(id) {
                match &node.content {
                    Content::Container(container) => stack.extend(&container.children),
                    Content::View(_) => views.push(id),
                }
                nodes.push(id);
            }
        }
        for id in nodes.into_iter().rev() {
            self.nodes.remove(id);
        }

        self.root = snapshot.root;
        self.focus = snapshot.focus;
        self.fullscreen = snapshot.fullscreen;
        self.recalculate();
        Ok(views)
    }

    pub fn active_temporary_layout_token(&self) -> Option<TemporaryLayoutToken> {
        self.temporary_layouts.last().map(|layout| layout.token)
    }

    pub(super) fn remove_suspended(&mut self, index: ViewId) {
        let mut root = index;
        while self.nodes[root].parent != root {
            root = self.nodes[root].parent;
        }
        let Some(layout_index) = self
            .temporary_layouts
            .iter()
            .position(|layout| layout.root == root)
        else {
            return;
        };

        let mut stack = vec![root];
        let mut views = Vec::new();
        while let Some(id) = stack.pop() {
            match &self.nodes[id].content {
                Content::Container(container) => stack.extend(&container.children),
                Content::View(_) => views.push(id),
            }
        }
        let replacement_focus = views
            .iter()
            .position(|id| *id == index)
            .and_then(|position| {
                (views.len() > 1).then(|| views[(position + views.len() - 1) % views.len()])
            });

        let parent = self.nodes[index].parent;
        let parent_is_root = parent == root;
        self.remove_or_replace(index, None);
        let parent_container = self.container_mut(parent);
        if parent_container.children.len() == 1 && !parent_is_root {
            let sibling = parent_container.children.pop().unwrap();
            parent_container.weights.pop();
            self.remove_or_replace(parent, Some(sibling));
        }

        let layout = &mut self.temporary_layouts[layout_index];
        if layout.focus == index {
            layout.focus = replacement_focus.unwrap_or(root);
        }
        if layout.fullscreen == Some(index) {
            layout.fullscreen = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{graphics::Rect, DocumentId};

    #[test]
    fn temporary_layout_restores_exact_tree_and_supports_nesting() {
        let mut tree = Tree::new(Rect::new(0, 0, 90, 30));
        let original = tree.insert(View::new(DocumentId::default(), Vec::new().into()));
        let original_area = tree.get(original).area;

        let (outer, outer_view) =
            tree.enter_temporary_layout(View::new(DocumentId::default(), Vec::new().into()));
        assert!(tree.node_is_active(outer_view));
        assert!(!tree.node_is_active(original));

        let (inner, inner_view) =
            tree.enter_temporary_layout(View::new(DocumentId::default(), Vec::new().into()));
        assert_eq!(
            tree.restore_temporary_layout(outer),
            Err("temporary layouts must be restored in LIFO order")
        );
        assert_eq!(
            tree.restore_temporary_layout(inner).unwrap(),
            vec![inner_view]
        );
        assert!(tree.node_is_active(outer_view));
        assert_eq!(
            tree.restore_temporary_layout(outer).unwrap(),
            vec![outer_view]
        );
        assert!(tree.node_is_active(original));
        assert_eq!(tree.focus, original);
        assert_eq!(tree.get(original).area, original_area);
    }

    #[test]
    fn removing_a_suspended_view_keeps_the_snapshot_restorable() {
        let mut tree = Tree::new(Rect::new(0, 0, 90, 30));
        let left = tree.insert(View::new(DocumentId::default(), Vec::new().into()));
        let right = tree.split(
            View::new(DocumentId::default(), Vec::new().into()),
            super::super::Layout::Vertical,
        );
        let (token, temporary) =
            tree.enter_temporary_layout(View::new(DocumentId::default(), Vec::new().into()));

        tree.remove(right);
        assert!(tree.node_is_active(temporary));
        tree.restore_temporary_layout(token).unwrap();
        assert!(tree.node_is_active(left));
        assert!(!tree.contains(right));
        assert_eq!(tree.active_views().count(), 1);
    }

    #[test]
    fn temporary_layout_restores_custom_split_proportions() {
        let mut tree = Tree::new(Rect::new(0, 0, 101, 30));
        let left = tree.insert(View::new(DocumentId::default(), Vec::new().into()));
        let right = tree.split(
            View::new(DocumentId::default(), Vec::new().into()),
            super::super::Layout::Vertical,
        );
        tree.focus = left;
        assert_eq!(tree.resize_focused(super::super::ResizeAxis::Width, 20), 20);
        let left_area = tree.get(left).area;
        let right_area = tree.get(right).area;

        let (token, _) =
            tree.enter_temporary_layout(View::new(DocumentId::default(), Vec::new().into()));
        tree.restore_temporary_layout(token).unwrap();
        assert_eq!(tree.get(left).area, left_area);
        assert_eq!(tree.get(right).area, right_area);
    }
}
