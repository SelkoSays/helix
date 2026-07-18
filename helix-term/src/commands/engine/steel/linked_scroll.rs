use super::Context;
use helix_view::ViewId;

pub fn create(cx: &mut Context, first: ViewId, second: ViewId) -> anyhow::Result<bool> {
    cx.editor.create_linked_scroll(first, second)?;
    Ok(true)
}

pub fn peer(cx: &mut Context, view: ViewId) -> Option<ViewId> {
    cx.editor.linked_scroll_peer(view)
}

pub fn remove(cx: &mut Context, view: ViewId) -> bool {
    cx.editor.remove_linked_scroll(view)
}
