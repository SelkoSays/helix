use super::Editor;
use crate::{view::ViewPosition, ViewId};
use anyhow::{bail, Result};
use std::collections::HashMap;

/// Editor-owned, edit-independent links between equal-row views.
///
/// Each view may participate in at most one link. Re-linking either endpoint
/// removes its previous pair first, which keeps lifecycle cleanup local and
/// deterministic.
#[derive(Default)]
pub(super) struct LinkedScrollLinks {
    peers: HashMap<ViewId, ViewId>,
}

impl LinkedScrollLinks {
    fn link(&mut self, first: ViewId, second: ViewId) {
        self.remove(first);
        self.remove(second);
        self.peers.insert(first, second);
        self.peers.insert(second, first);
    }

    fn peer(&self, view: ViewId) -> Option<ViewId> {
        self.peers.get(&view).copied()
    }

    fn remove(&mut self, view: ViewId) -> bool {
        let Some(peer) = self.peers.remove(&view) else {
            return false;
        };
        self.peers.remove(&peer);
        true
    }
}

impl Editor {
    /// Link vertical viewport movement for two live, equal-row views.
    pub fn create_linked_scroll(&mut self, first: ViewId, second: ViewId) -> Result<()> {
        if first == second {
            bail!("linked scrolling requires two different views");
        }
        if !self.tree.contains(first) || !self.tree.contains(second) {
            bail!("linked scrolling requires two live views");
        }

        let first_doc = self.tree.get(first).doc;
        let second_doc = self.tree.get(second).doc;
        let first_rows = self.documents[&first_doc].text().len_lines();
        let second_rows = self.documents[&second_doc].text().len_lines();
        if first_rows != second_rows {
            bail!(
                "linked scrolling requires equal-row buffers (left: {first_rows}, right: {second_rows})"
            );
        }

        self.linked_scroll_links.link(first, second);
        let source = if self.tree.focus == second {
            second
        } else {
            first
        };
        self.synchronize_linked_scroll_from(source);
        self.needs_redraw = true;
        Ok(())
    }

    /// Return the peer linked to `view`, if both endpoints are still live.
    pub fn linked_scroll_peer(&mut self, view: ViewId) -> Option<ViewId> {
        let peer = self.linked_scroll_links.peer(view)?;
        if self.tree.contains(view) && self.tree.contains(peer) {
            Some(peer)
        } else {
            self.linked_scroll_links.remove(view);
            None
        }
    }

    /// Remove the link containing `view`.
    pub fn remove_linked_scroll(&mut self, view: ViewId) -> bool {
        self.linked_scroll_links.remove(view)
    }

    /// Copy the focused view's vertical row to its linked peer.
    ///
    /// Horizontal offsets, selections, documents, and edits remain independent.
    pub fn synchronize_linked_scroll(&mut self) {
        self.synchronize_linked_scroll_from(self.tree.focus);
    }

    fn synchronize_linked_scroll_from(&mut self, source: ViewId) {
        let Some(target) = self.linked_scroll_peer(source) else {
            return;
        };

        let source_doc_id = self.tree.get(source).doc;
        let target_doc_id = self.tree.get(target).doc;
        let source_doc = &self.documents[&source_doc_id];
        let source_offset = source_doc.view_offset(source);
        let source_row = source_doc
            .text()
            .char_to_line(source_offset.anchor.min(source_doc.text().len_chars()));

        let target_doc = self.documents.get_mut(&target_doc_id).unwrap();
        let target_row = source_row.min(target_doc.text().len_lines().saturating_sub(1));
        let mut target_offset = target_doc.view_offset(target);
        let next = ViewPosition {
            anchor: target_doc.text().line_to_char(target_row),
            vertical_offset: source_offset.vertical_offset,
            horizontal_offset: target_offset.horizontal_offset,
        };
        if target_offset != next {
            target_offset = next;
            target_doc.set_view_offset(target, target_offset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LinkedScrollLinks;
    use crate::ViewId;
    use slotmap::SlotMap;

    fn view_ids(count: usize) -> Vec<ViewId> {
        let mut views = SlotMap::<ViewId, ()>::with_key();
        (0..count).map(|_| views.insert(())).collect()
    }

    #[test]
    fn links_are_bidirectional_and_removable_from_either_endpoint() {
        let ids = view_ids(2);
        let mut links = LinkedScrollLinks::default();
        links.link(ids[0], ids[1]);
        assert_eq!(links.peer(ids[0]), Some(ids[1]));
        assert_eq!(links.peer(ids[1]), Some(ids[0]));
        assert!(links.remove(ids[1]));
        assert_eq!(links.peer(ids[0]), None);
        assert_eq!(links.peer(ids[1]), None);
    }

    #[test]
    fn relinking_replaces_both_previous_pairs_without_stale_peers() {
        let ids = view_ids(4);
        let mut links = LinkedScrollLinks::default();
        links.link(ids[0], ids[1]);
        links.link(ids[2], ids[3]);
        links.link(ids[1], ids[2]);
        assert_eq!(links.peer(ids[0]), None);
        assert_eq!(links.peer(ids[1]), Some(ids[2]));
        assert_eq!(links.peer(ids[2]), Some(ids[1]));
        assert_eq!(links.peer(ids[3]), None);
    }
}
