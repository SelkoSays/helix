use super::{configure_lsp_builtins, generate_module, Custom, Engine, RegisterFn, CTX};
use crate::commands::engine::steel::Context;
use helix_core::diagnostic::Severity;
use helix_view::editor::ExternalDiagnostic;
use helix_view::tree::TemporaryLayoutToken;
use std::{path::PathBuf, sync::Arc};
use steel::steel_vm::builtin::BuiltInModule;

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

fn register_view_layout(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/view-layout");
    module
        .register_fn_with_ctx(CTX, "view-fullscreen?", fullscreen)
        .register_fn_with_ctx(CTX, "view-fullscreen-enter!", fullscreen_enter)
        .register_fn_with_ctx(CTX, "view-fullscreen-leave!", fullscreen_leave)
        .register_fn_with_ctx(CTX, "view-fullscreen-toggle!", fullscreen_toggle)
        .register_fn_with_ctx(CTX, "temporary-layout-enter!", temporary_layout_enter)
        .register_fn_with_ctx(CTX, "temporary-layout-restore!", temporary_layout_restore);

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

pub(super) fn register_builtin(engine: &mut Engine, generate_sources: bool) {
    register_view_layout(engine, generate_sources);
    register_external_diagnostics(engine, generate_sources);
}
