use super::Editor;
use crate::{tree::TemporaryLayoutToken, DocumentId, View};
use anyhow::{bail, Result};

impl Editor {
    /// Suspend the current split tree and enter a temporary single-document layout.
    pub fn enter_temporary_layout(
        &mut self,
        document_id: DocumentId,
    ) -> Result<TemporaryLayoutToken> {
        if !self.documents.contains_key(&document_id) {
            bail!("temporary layout document does not exist");
        }

        self.enter_normal_mode();
        let gutters = self.config().gutters.clone();
        let (token, view_id) = self
            .tree
            .enter_temporary_layout(View::new(document_id, gutters));
        let document = self.documents.get_mut(&document_id).unwrap();
        document.ensure_view_init(view_id);
        document.mark_as_focused();
        self._refresh();
        Ok(token)
    }

    /// Restore the matching most-recent temporary layout.
    pub fn restore_temporary_layout(&mut self, token: TemporaryLayoutToken) -> Result<()> {
        let removed = self
            .tree
            .restore_temporary_layout(token)
            .map_err(anyhow::Error::msg)?;
        for document in self.documents.values_mut() {
            for view_id in &removed {
                document.remove_view(*view_id);
            }
        }
        for view_id in &removed {
            self.remove_linked_scroll(*view_id);
        }
        if let Some(document_id) = self.tree.try_get(self.tree.focus).map(|view| view.doc) {
            if let Some(document) = self.documents.get_mut(&document_id) {
                document.mark_as_focused();
            }
        }
        let gutters = self.config().gutters.clone();
        let view_ids = self.tree.active_view_ids().collect::<Vec<_>>();
        for view_id in view_ids {
            let document_id = self.tree.get(view_id).doc;
            let view = self.tree.get_mut(view_id);
            let document = self.documents.get_mut(&document_id).unwrap();
            view.sync_changes(document);
            view.gutters = gutters.clone();
        }
        self.needs_redraw = true;
        Ok(())
    }

    pub fn active_temporary_layout_token(&self) -> Option<TemporaryLayoutToken> {
        self.tree.active_temporary_layout_token()
    }

    /// Discard every temporary view and restore the base layout.
    pub fn restore_all_temporary_layouts(&mut self) {
        while let Some(token) = self.active_temporary_layout_token() {
            if let Err(error) = self.restore_temporary_layout(token) {
                log::error!("failed to restore temporary layout during cleanup: {error}");
                break;
            }
        }
    }
}
