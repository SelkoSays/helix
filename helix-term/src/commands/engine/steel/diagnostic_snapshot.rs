use helix_core::{diagnostic::DiagnosticProvider, Uri};
use helix_lsp::{lsp, OffsetEncoding};
use steel::rvals::{AsRefSteelVal, Custom};
use steel::{steel_vm::builtin::BuiltInModule, SteelVal};

use crate::commands::lsp::jump_to_plugin_location;

use super::{Context, RegisterFn, CTX};

#[derive(Clone)]
pub(super) struct SteelDiagnosticSnapshot {
    uri: Uri,
    path: String,
    range: lsp::Range,
    offset_encoding: OffsetEncoding,
    severity: String,
    message: String,
    code: Option<String>,
    source: Option<String>,
    provider_kind: String,
    provider_id: String,
    provider_namespace: Option<String>,
}

impl Custom for SteelDiagnosticSnapshot {}

#[derive(Clone)]
pub(super) struct SteelDiagnosticBatch {
    items: Vec<SteelDiagnosticSnapshot>,
    omitted: usize,
}

impl Custom for SteelDiagnosticBatch {}

fn severity(value: Option<lsp::DiagnosticSeverity>) -> String {
    match value {
        Some(lsp::DiagnosticSeverity::ERROR) => "error",
        Some(lsp::DiagnosticSeverity::WARNING) | None => "warning",
        Some(lsp::DiagnosticSeverity::INFORMATION) => "info",
        Some(lsp::DiagnosticSeverity::HINT) => "hint",
        Some(_) => "warning",
    }
    .to_string()
}

fn code(value: &Option<lsp::NumberOrString>) -> Option<String> {
    value.as_ref().map(|code| match code {
        lsp::NumberOrString::Number(value) => value.to_string(),
        lsp::NumberOrString::String(value) => value.clone(),
    })
}

fn snapshot(cx: &mut Context) -> SteelDiagnosticBatch {
    let mut items = Vec::new();
    let mut omitted = 0;

    for (uri, diagnostics) in &cx.editor.diagnostics {
        let Some(path) = uri.as_path().and_then(|path| path.to_str()) else {
            omitted += diagnostics.len();
            continue;
        };

        for (diagnostic, provider) in diagnostics {
            let (offset_encoding, provider_kind, provider_id, provider_namespace) = match provider {
                DiagnosticProvider::External { namespace } => (
                    OffsetEncoding::Utf8,
                    "external".to_string(),
                    namespace.to_string(),
                    Some(namespace.to_string()),
                ),
                DiagnosticProvider::Lsp {
                    server_id,
                    identifier,
                } => {
                    let Some(server) = cx.editor.language_server_by_id(*server_id) else {
                        omitted += 1;
                        continue;
                    };
                    let id = match identifier {
                        Some(identifier) => format!("{server_id}:{identifier}"),
                        None => server_id.to_string(),
                    };
                    (
                        server.offset_encoding(),
                        "lsp".to_string(),
                        id,
                        identifier.as_ref().map(|value| value.to_string()),
                    )
                }
            };

            items.push(SteelDiagnosticSnapshot {
                uri: uri.clone(),
                path: path.to_string(),
                range: diagnostic.range,
                offset_encoding,
                severity: severity(diagnostic.severity),
                message: diagnostic.message.clone(),
                code: code(&diagnostic.code),
                source: diagnostic.source.clone(),
                provider_kind,
                provider_id,
                provider_namespace,
            });
        }
    }

    SteelDiagnosticBatch { items, omitted }
}

fn is_batch(value: SteelVal) -> bool {
    SteelDiagnosticBatch::as_ref(&value).is_ok()
}

fn is_snapshot(value: SteelVal) -> bool {
    SteelDiagnosticSnapshot::as_ref(&value).is_ok()
}

fn batch_items(batch: &SteelDiagnosticBatch) -> Vec<SteelDiagnosticSnapshot> {
    batch.items.clone()
}

fn batch_omitted(batch: &SteelDiagnosticBatch) -> usize {
    batch.omitted
}

fn path(snapshot: &SteelDiagnosticSnapshot) -> String {
    snapshot.path.clone()
}

fn start_line(snapshot: &SteelDiagnosticSnapshot) -> usize {
    snapshot.range.start.line as usize + 1
}

fn start_column(snapshot: &SteelDiagnosticSnapshot) -> usize {
    snapshot.range.start.character as usize + 1
}

fn end_line(snapshot: &SteelDiagnosticSnapshot) -> usize {
    snapshot.range.end.line as usize + 1
}

fn end_column(snapshot: &SteelDiagnosticSnapshot) -> usize {
    snapshot.range.end.character as usize + 1
}

fn snapshot_severity(snapshot: &SteelDiagnosticSnapshot) -> String {
    snapshot.severity.clone()
}

fn message(snapshot: &SteelDiagnosticSnapshot) -> String {
    snapshot.message.clone()
}

fn snapshot_code(snapshot: &SteelDiagnosticSnapshot) -> Option<String> {
    snapshot.code.clone()
}

fn source(snapshot: &SteelDiagnosticSnapshot) -> Option<String> {
    snapshot.source.clone()
}

fn provider_kind(snapshot: &SteelDiagnosticSnapshot) -> String {
    snapshot.provider_kind.clone()
}

fn provider_id(snapshot: &SteelDiagnosticSnapshot) -> String {
    snapshot.provider_id.clone()
}

fn provider_namespace(snapshot: &SteelDiagnosticSnapshot) -> Option<String> {
    snapshot.provider_namespace.clone()
}

fn open(cx: &mut Context, snapshot: SteelDiagnosticSnapshot, action: super::super::Action) {
    jump_to_plugin_location(
        cx.editor,
        snapshot.uri.clone(),
        snapshot.range,
        snapshot.offset_encoding,
        action,
    );
}

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn("diagnostic-batch?", is_batch)
        .register_fn("diagnostic-snapshot?", is_snapshot)
        .register_fn_with_ctx(CTX, "diagnostics-snapshot", snapshot)
        .register_fn("diagnostic-batch-items", batch_items)
        .register_fn("diagnostic-batch-omitted", batch_omitted)
        .register_fn("diagnostic-snapshot-path", path)
        .register_fn("diagnostic-snapshot-start-line", start_line)
        .register_fn("diagnostic-snapshot-start-column", start_column)
        .register_fn("diagnostic-snapshot-end-line", end_line)
        .register_fn("diagnostic-snapshot-end-column", end_column)
        .register_fn("diagnostic-snapshot-severity", snapshot_severity)
        .register_fn("diagnostic-snapshot-message", message)
        .register_fn("diagnostic-snapshot-code", snapshot_code)
        .register_fn("diagnostic-snapshot-source", source)
        .register_fn("diagnostic-snapshot-provider-kind", provider_kind)
        .register_fn("diagnostic-snapshot-provider-id", provider_id)
        .register_fn("diagnostic-snapshot-provider-namespace", provider_namespace)
        .register_fn_with_ctx(CTX, "diagnostic-snapshot-open!", open);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_defaults_to_warning() {
        assert_eq!(severity(None), "warning");
        assert_eq!(severity(Some(lsp::DiagnosticSeverity::ERROR)), "error");
    }

    #[test]
    fn codes_are_losslessly_displayable() {
        assert_eq!(
            code(&Some(lsp::NumberOrString::Number(42))),
            Some("42".into())
        );
        assert_eq!(
            code(&Some(lsp::NumberOrString::String("E042".into()))),
            Some("E042".into())
        );
    }
}
