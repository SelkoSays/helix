use super::{configure_lsp_builtins, generate_module, Custom, Engine, RegisterFn, CTX};
use crate::commands::engine::steel::Context;
use helix_core::diagnostic::Severity;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::editor::ExternalDiagnostic;
use helix_view::tree::{ResizeAxis, TemporaryLayoutToken};
use std::{path::PathBuf, sync::Arc};
use steel::steel_vm::builtin::BuiltInModule;

// Declared here rather than in the Steel `mod.rs` so the whole extension
// surface stays behind this one registrar.
#[path = "archive.rs"]
mod archive;
#[path = "binary.rs"]
mod binary;
#[path = "diagnostic_snapshot.rs"]
mod diagnostic_snapshot;
#[path = "file_snapshot.rs"]
mod file_snapshot;
#[path = "json.rs"]
mod json;
#[path = "markdown_preview.rs"]
mod markdown_preview;
#[path = "regex.rs"]
mod regex;
#[path = "syntax_highlight.rs"]
mod syntax_highlight;

#[derive(Clone)]
struct SteelTemporaryLayoutToken(TemporaryLayoutToken);

impl Custom for SteelTemporaryLayoutToken {}

#[derive(Clone)]
struct SteelExternalDiagnostic(ExternalDiagnostic);

impl Custom for SteelExternalDiagnostic {}

fn fullscreen(cx: &mut Context) -> bool {
    cx.editor.tree.fullscreen_view().is_some()
}

fn fullscreen_enter(cx: &mut Context) -> bool {
    cx.editor.tree.enter_fullscreen(cx.editor.tree.focus)
}

fn fullscreen_leave(cx: &mut Context) -> bool {
    cx.editor.tree.leave_fullscreen()
}

fn fullscreen_toggle(cx: &mut Context) -> bool {
    cx.editor.tree.toggle_fullscreen()
}

fn temporary_layout_enter(
    cx: &mut Context,
    document_id: helix_view::DocumentId,
) -> anyhow::Result<SteelTemporaryLayoutToken> {
    cx.editor
        .enter_temporary_layout(document_id)
        .map(SteelTemporaryLayoutToken)
}

fn temporary_layout_restore(
    cx: &mut Context,
    token: SteelTemporaryLayoutToken,
) -> anyhow::Result<()> {
    cx.editor.restore_temporary_layout(token.0)
}

fn view_resize(cx: &mut Context, axis: String, delta: i32) -> anyhow::Result<i32> {
    let axis = match axis.as_str() {
        "width" => ResizeAxis::Width,
        "height" => ResizeAxis::Height,
        _ => anyhow::bail!("view resize axis must be width or height"),
    };
    Ok(cx.editor.tree.resize_focused(axis, delta))
}

fn view_equalize(cx: &mut Context) -> bool {
    cx.editor.tree.equalize_focused()
}

fn register_view_layout(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/view-layout");
    module
        .register_fn_with_ctx(CTX, "view-fullscreen?", fullscreen)
        .register_fn_with_ctx(CTX, "view-fullscreen-enter!", fullscreen_enter)
        .register_fn_with_ctx(CTX, "view-fullscreen-leave!", fullscreen_leave)
        .register_fn_with_ctx(CTX, "view-fullscreen-toggle!", fullscreen_toggle)
        .register_fn_with_ctx(CTX, "temporary-layout-enter!", temporary_layout_enter)
        .register_fn_with_ctx(CTX, "temporary-layout-restore!", temporary_layout_restore)
        .register_fn_with_ctx(CTX, "view-resize!", view_resize)
        .register_fn_with_ctx(CTX, "view-equalize!", view_equalize);

    let source = include_str!("view-layout.scm");
    if generate_sources {
        generate_module("view-layout.scm", source);
        configure_lsp_builtins("view-layout", &module);
    }
    engine.register_steel_module("helix/view-layout.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn external_diagnostic(
    path: String,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
    severity: String,
    message: String,
    code: Option<String>,
    source: Option<String>,
) -> anyhow::Result<SteelExternalDiagnostic> {
    let severity = match severity.as_str() {
        "hint" => Severity::Hint,
        "info" => Severity::Info,
        "warning" => Severity::Warning,
        "error" => Severity::Error,
        _ => anyhow::bail!("diagnostic severity must be hint, info, warning, or error"),
    };
    Ok(SteelExternalDiagnostic(ExternalDiagnostic {
        path: PathBuf::from(path),
        start_line,
        start_column,
        end_line,
        end_column,
        severity,
        message,
        code,
        source,
    }))
}

fn external_diagnostics_publish(
    cx: &mut Context,
    namespace: String,
    diagnostics: Vec<SteelExternalDiagnostic>,
) -> anyhow::Result<()> {
    if namespace.trim().is_empty() {
        anyhow::bail!("external diagnostic namespace cannot be empty");
    }
    cx.editor.publish_external_diagnostics(
        Arc::<str>::from(namespace),
        diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.0)
            .collect(),
    )
}

fn external_diagnostics_clear(cx: &mut Context, namespace: String) -> anyhow::Result<()> {
    if namespace.trim().is_empty() {
        anyhow::bail!("external diagnostic namespace cannot be empty");
    }
    cx.editor.clear_external_diagnostics(&namespace);
    Ok(())
}

fn register_external_diagnostics(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/external-diagnostics");
    module
        .register_fn("external-diagnostic", external_diagnostic)
        .register_fn_with_ctx(
            CTX,
            "external-diagnostics-publish!",
            external_diagnostics_publish,
        )
        .register_fn_with_ctx(
            CTX,
            "external-diagnostics-clear!",
            external_diagnostics_clear,
        );

    let source = include_str!("external-diagnostics.scm");
    if generate_sources {
        generate_module("external-diagnostics.scm", source);
        configure_lsp_builtins("external-diagnostics", &module);
    }
    engine.register_steel_module(
        "helix/external-diagnostics.scm".to_string(),
        source.to_string(),
    );
    engine.register_module(module);
}

fn register_diagnostic_snapshot(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/diagnostics");
    diagnostic_snapshot::register(&mut module);

    let source = include_str!("diagnostics.scm");
    if generate_sources {
        generate_module("diagnostics.scm", source);
        configure_lsp_builtins("diagnostics", &module);
    }
    engine.register_steel_module("helix/diagnostics.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn text_display_width(value: String) -> usize {
    UnicodeWidthStr::width(value.as_str())
}

fn register_text_display(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/text-display");
    module.register_fn("text-display-width", text_display_width);

    let source = include_str!("text-display.scm");
    if generate_sources {
        generate_module("text-display.scm", source);
        configure_lsp_builtins("text-display", &module);
    }
    engine.register_steel_module("helix/text-display.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn register_syntax_highlight(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/syntax-highlight");
    syntax_highlight::register(&mut module);

    let source = include_str!("syntax-highlight.scm");
    if generate_sources {
        generate_module("syntax-highlight.scm", source);
        configure_lsp_builtins("syntax-highlight", &module);
    }
    engine.register_steel_module("helix/syntax-highlight.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn register_regex(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/regex");
    regex::register(&mut module);

    let source = include_str!("regex.scm");
    if generate_sources {
        generate_module("regex.scm", source);
        configure_lsp_builtins("regex", &module);
    }
    engine.register_steel_module("helix/regex.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn register_json(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/json");
    json::register(&mut module);

    let source = include_str!("json.scm");
    if generate_sources {
        generate_module("json.scm", source);
        configure_lsp_builtins("json", &module);
    }
    engine.register_steel_module("helix/json.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn register_binary(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/binary");
    binary::register(&mut module);

    let source = include_str!("binary.scm");
    if generate_sources {
        generate_module("binary.scm", source);
        configure_lsp_builtins("binary", &module);
    }
    engine.register_steel_module("helix/binary.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn register_archive(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/archive");
    archive::register(&mut module);

    let source = include_str!("archive.scm");
    if generate_sources {
        generate_module("archive.scm", source);
        configure_lsp_builtins("archive", &module);
    }
    engine.register_steel_module("helix/archive.scm".to_string(), source.to_string());
    engine.register_module(module);
}

fn register_markdown_preview(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/markdown-preview");
    markdown_preview::register(&mut module);

    let source = include_str!("markdown-preview.scm");
    if generate_sources {
        generate_module("markdown-preview.scm", source);
        configure_lsp_builtins("markdown-preview", &module);
    }
    engine.register_steel_module("helix/markdown-preview.scm".to_string(), source.to_string());
    engine.register_module(module);
}

pub(super) fn register_builtin(engine: &mut Engine, generate_sources: bool) {
    register_view_layout(engine, generate_sources);
    register_external_diagnostics(engine, generate_sources);
    register_diagnostic_snapshot(engine, generate_sources);
    register_text_display(engine, generate_sources);
    register_syntax_highlight(engine, generate_sources);
    register_regex(engine, generate_sources);
    register_json(engine, generate_sources);
    register_binary(engine, generate_sources);
    register_archive(engine, generate_sources);
    register_markdown_preview(engine, generate_sources);
}

#[cfg(test)]
mod tests {
    use super::text_display_width;

    #[test]
    fn text_display_width_uses_terminal_cells() {
        assert_eq!(text_display_width("abc".to_string()), 3);
        assert_eq!(text_display_width("a界b".to_string()), 4);
        assert_eq!(text_display_width("e\u{301}".to_string()), 1);
    }
}
