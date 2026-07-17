use super::{Diagnostics, Editor};
use crate::Document;
use helix_core::{
    diagnostic::{Diagnostic, DiagnosticProvider},
    syntax::config::LanguageConfiguration,
    Rope, Uri,
};
use helix_lsp::lsp;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// A one-based diagnostic location published by an editor extension.
#[derive(Debug, Clone)]
pub struct ExternalDiagnostic {
    pub path: PathBuf,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub severity: helix_core::diagnostic::Severity,
    pub message: String,
    pub code: Option<String>,
    pub source: Option<String>,
}

impl ExternalDiagnostic {
    fn into_lsp(self) -> anyhow::Result<(Uri, lsp::Diagnostic)> {
        if self.start_line == 0
            || self.start_column == 0
            || self.end_line == 0
            || self.end_column == 0
        {
            anyhow::bail!("external diagnostic positions are one-based and must be positive");
        }
        if (self.end_line, self.end_column) < (self.start_line, self.start_column) {
            anyhow::bail!("external diagnostic end precedes its start");
        }

        let path = helix_stdx::path::canonicalize(self.path);
        let uri = Uri::from(path);
        let severity = match self.severity {
            helix_core::diagnostic::Severity::Hint => lsp::DiagnosticSeverity::HINT,
            helix_core::diagnostic::Severity::Info => lsp::DiagnosticSeverity::INFORMATION,
            helix_core::diagnostic::Severity::Warning => lsp::DiagnosticSeverity::WARNING,
            helix_core::diagnostic::Severity::Error => lsp::DiagnosticSeverity::ERROR,
        };
        let range = lsp::Range::new(
            lsp::Position::new(
                (self.start_line - 1).try_into()?,
                (self.start_column - 1).try_into()?,
            ),
            lsp::Position::new(
                (self.end_line - 1).try_into()?,
                (self.end_column - 1).try_into()?,
            ),
        );
        Ok((
            uri,
            lsp::Diagnostic::new(
                range,
                Some(severity),
                self.code.map(lsp::NumberOrString::String),
                self.source,
                self.message,
                None,
                None,
            ),
        ))
    }
}

pub(super) fn to_core_diagnostic(
    text: &Rope,
    language_config: Option<&LanguageConfiguration>,
    diagnostic: &lsp::Diagnostic,
    provider: DiagnosticProvider,
) -> Option<Diagnostic> {
    let mut diagnostic = diagnostic.clone();
    if helix_lsp::util::lsp_pos_to_pos(text, diagnostic.range.end, helix_lsp::OffsetEncoding::Utf8)
        .is_none()
    {
        diagnostic.range.end = helix_lsp::util::pos_to_lsp_pos(
            text,
            text.len_chars(),
            helix_lsp::OffsetEncoding::Utf8,
        );
    }
    Document::lsp_diagnostic_to_diagnostic(
        text,
        language_config,
        &diagnostic,
        provider,
        helix_lsp::OffsetEncoding::Utf8,
    )
}

fn replace_namespace(
    diagnostics: &mut Diagnostics,
    provider: &DiagnosticProvider,
    grouped: BTreeMap<Uri, Vec<lsp::Diagnostic>>,
) {
    for existing in diagnostics.values_mut() {
        existing.retain(|(_, candidate)| candidate != provider);
    }
    diagnostics.retain(|_, values| !values.is_empty());
    for (uri, values) in grouped {
        diagnostics.entry(uri).or_default().extend(
            values
                .into_iter()
                .map(|diagnostic| (diagnostic, provider.clone())),
        );
    }
}

fn clear_namespace(diagnostics: &mut Diagnostics, namespace: &str) {
    for existing in diagnostics.values_mut() {
        existing.retain(|(_, provider)| {
            provider
                .external_namespace()
                .is_none_or(|candidate| candidate.as_ref() != namespace)
        });
    }
    diagnostics.retain(|_, values| !values.is_empty());
}

impl Editor {
    /// Replace all diagnostics in an external namespace without touching LSPs.
    pub fn publish_external_diagnostics(
        &mut self,
        namespace: Arc<str>,
        diagnostics: Vec<ExternalDiagnostic>,
    ) -> anyhow::Result<()> {
        if namespace.trim().is_empty() {
            anyhow::bail!("external diagnostic namespace cannot be empty");
        }
        let provider = DiagnosticProvider::External {
            namespace: namespace.clone(),
        };
        let mut grouped: BTreeMap<Uri, Vec<lsp::Diagnostic>> = BTreeMap::new();
        for diagnostic in diagnostics {
            let (uri, diagnostic) = diagnostic.into_lsp()?;
            grouped.entry(uri).or_default().push(diagnostic);
        }

        replace_namespace(&mut self.diagnostics, &provider, grouped);
        self.refresh_all_document_diagnostics();
        self.needs_redraw = true;
        Ok(())
    }

    /// Clear one external namespace without touching LSPs or other extensions.
    pub fn clear_external_diagnostics(&mut self, namespace: &str) {
        clear_namespace(&mut self.diagnostics, namespace);
        self.refresh_all_document_diagnostics();
        self.needs_redraw = true;
    }

    fn refresh_all_document_diagnostics(&mut self) {
        let language_servers = &self.language_servers;
        let diagnostics = &self.diagnostics;
        for document in self.documents.values_mut() {
            let converted = Editor::doc_diagnostics(language_servers, diagnostics, document)
                .collect::<Vec<_>>();
            document.replace_diagnostics(converted, &[], None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::diagnostic::LanguageServerId;

    fn raw(message: &str) -> lsp::Diagnostic {
        lsp::Diagnostic::new_simple(
            lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 1)),
            message.into(),
        )
    }

    #[test]
    fn rejects_zero_and_reversed_positions() {
        let make = |start_line, start_column, end_line, end_column| ExternalDiagnostic {
            path: PathBuf::from("test.rs"),
            start_line,
            start_column,
            end_line,
            end_column,
            severity: helix_core::diagnostic::Severity::Error,
            message: "problem".into(),
            code: None,
            source: None,
        };
        assert!(make(0, 1, 1, 1).into_lsp().is_err());
        assert!(make(2, 1, 1, 1).into_lsp().is_err());
        assert!(make(1, 1, 1, 2).into_lsp().is_ok());
    }

    #[test]
    fn replacement_and_clear_preserve_lsp_and_other_namespaces() {
        let uri = Uri::from(helix_stdx::path::canonicalize("test.rs"));
        let lsp_provider = DiagnosticProvider::Lsp {
            server_id: LanguageServerId::default(),
            identifier: None,
        };
        let first = DiagnosticProvider::External {
            namespace: Arc::from("first"),
        };
        let second = DiagnosticProvider::External {
            namespace: Arc::from("second"),
        };
        let mut diagnostics = BTreeMap::from([(
            uri.clone(),
            vec![
                (raw("lsp"), lsp_provider.clone()),
                (raw("old first"), first.clone()),
                (raw("second"), second.clone()),
            ],
        )]);

        replace_namespace(
            &mut diagnostics,
            &first,
            BTreeMap::from([(uri.clone(), vec![raw("new first")])]),
        );
        let values = &diagnostics[&uri];
        assert_eq!(values.len(), 3);
        assert!(
            values
                .iter()
                .any(|(diagnostic, provider)| diagnostic.message == "lsp"
                    && provider == &lsp_provider)
        );
        assert!(values
            .iter()
            .any(|(diagnostic, provider)| diagnostic.message == "second" && provider == &second));
        assert!(values
            .iter()
            .any(|(diagnostic, provider)| diagnostic.message == "new first" && provider == &first));

        clear_namespace(&mut diagnostics, "first");
        let values = &diagnostics[&uri];
        assert_eq!(values.len(), 2);
        assert!(values.iter().all(|(_, provider)| provider != &first));
    }

    #[test]
    fn clamps_an_external_end_position_to_document_end() {
        let text = Rope::from("abc\n");
        let diagnostic = lsp::Diagnostic::new_simple(
            lsp::Range::new(lsp::Position::new(0, 1), lsp::Position::new(99, 99)),
            "problem".into(),
        );
        let converted = to_core_diagnostic(
            &text,
            None,
            &diagnostic,
            DiagnosticProvider::External {
                namespace: Arc::from("test"),
            },
        )
        .unwrap();
        assert_eq!(converted.range.start, 1);
        assert_eq!(converted.range.end, text.len_chars());
    }
}
