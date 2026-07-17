use super::*;
use helix_core::Transaction;
use helix_view::{
    editor::{Action, ExternalDiagnostic},
    view::ViewPosition,
};

#[tokio::test(flavor = "multi_thread")]
async fn temporary_layout_restores_editor_view_state_and_dirty_document() -> anyhow::Result<()> {
    let contents = (0..120)
        .map(|line| format!("line {line:03}\n"))
        .collect::<String>();
    let file = helpers::temp_file_with_contents(contents)?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    let (view, document) = helix_view::current!(app.editor);
    let transaction = Transaction::insert(
        document.text(),
        document.selection(view.id),
        " changed".into(),
    );
    document.apply(&transaction, view.id);
    let document_id = app.editor.tree.get(app.editor.tree.focus).doc;
    let first_view = app.editor.tree.focus;
    app.editor.switch(document_id, Action::VerticalSplit);
    let focused_view = app.editor.tree.focus;
    let saved_area = app.editor.tree.get(focused_view).area;
    let saved_selection = helix_core::Selection::single(540, 541);
    let requested_offset = ViewPosition {
        anchor: 450,
        horizontal_offset: 0,
        vertical_offset: 10,
    };
    let document = app.editor.document_mut(document_id).unwrap();
    document.set_selection(focused_view, saved_selection.clone());
    document.set_view_offset(focused_view, requested_offset);
    app.editor.enter_normal_mode();
    let saved_offset = app
        .editor
        .document(document_id)
        .unwrap()
        .view_offset(focused_view);

    let outer = app.editor.enter_temporary_layout(document_id)?;
    assert_eq!(app.editor.tree.active_views().count(), 1);
    assert_eq!(app.editor.tree.views().count(), 3);
    let inner = app.editor.enter_temporary_layout(document_id)?;
    assert!(app.editor.restore_temporary_layout(outer).is_err());
    app.editor.restore_temporary_layout(inner)?;
    app.editor.restore_temporary_layout(outer)?;

    assert_eq!(app.editor.tree.focus, focused_view);
    assert_eq!(app.editor.tree.active_views().count(), 2);
    assert_eq!(app.editor.tree.get(focused_view).area, saved_area);
    let document = app.editor.document(document_id).unwrap();
    assert!(document.is_modified());
    assert_eq!(document.selection(focused_view), &saved_selection);
    assert_eq!(document.view_offset(focused_view), saved_offset);
    assert!(app.editor.tree.contains(first_view));

    let _token = app.editor.enter_temporary_layout(document_id)?;
    let temporary_view = app.editor.tree.focus;
    app.editor.close(temporary_view);
    assert_eq!(app.editor.active_temporary_layout_token(), None);
    assert_eq!(app.editor.tree.focus, focused_view);

    let diagnostic_file = helpers::temp_file_with_contents("problem\n")?;
    let document_count = app.editor.documents().count();
    let focus_before_publish = app.editor.tree.focus;
    app.editor.publish_external_diagnostics(
        "integration-test".into(),
        vec![ExternalDiagnostic {
            path: diagnostic_file.path().into(),
            start_line: 1,
            start_column: 1,
            end_line: 99,
            end_column: 99,
            severity: helix_core::diagnostic::Severity::Error,
            message: "external problem".into(),
            code: Some("E-test".into()),
            source: Some("integration".into()),
        }],
    )?;
    assert_eq!(app.editor.documents().count(), document_count);
    assert_eq!(app.editor.tree.focus, focus_before_publish);

    let diagnostic_id = app.editor.open(diagnostic_file.path(), Action::Load)?;
    assert_eq!(
        app.editor
            .document(diagnostic_id)
            .unwrap()
            .diagnostics()
            .len(),
        1
    );
    assert!(app.editor.close_document(diagnostic_id, true).is_ok());
    let reopened_id = app.editor.open(diagnostic_file.path(), Action::Load)?;
    assert_eq!(
        app.editor
            .document(reopened_id)
            .unwrap()
            .diagnostics()
            .len(),
        1
    );
    app.editor.clear_external_diagnostics("integration-test");
    assert!(app
        .editor
        .document(reopened_id)
        .unwrap()
        .diagnostics()
        .is_empty());

    assert!(app.close().await.is_empty());
    Ok(())
}
