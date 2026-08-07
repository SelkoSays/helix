(require-builtin helix/core/diagnostics as helix.)

(provide DiagnosticBatch?
         DiagnosticSnapshot?
         diagnostics-snapshot
         DiagnosticBatch-items
         DiagnosticBatch-omitted
         DiagnosticSnapshot-path
         DiagnosticSnapshot-start-line
         DiagnosticSnapshot-start-column
         DiagnosticSnapshot-end-line
         DiagnosticSnapshot-end-column
         DiagnosticSnapshot-severity
         DiagnosticSnapshot-message
         DiagnosticSnapshot-code
         DiagnosticSnapshot-source
         DiagnosticSnapshot-provider-kind
         DiagnosticSnapshot-provider-id
         DiagnosticSnapshot-provider-namespace
         diagnostic-snapshot-open!)

(define DiagnosticBatch? helix.diagnostic-batch?)
(define DiagnosticSnapshot? helix.diagnostic-snapshot?)
(define diagnostics-snapshot helix.diagnostics-snapshot)
(define DiagnosticBatch-items helix.diagnostic-batch-items)
(define DiagnosticBatch-omitted helix.diagnostic-batch-omitted)
(define DiagnosticSnapshot-path helix.diagnostic-snapshot-path)
(define DiagnosticSnapshot-start-line helix.diagnostic-snapshot-start-line)
(define DiagnosticSnapshot-start-column helix.diagnostic-snapshot-start-column)
(define DiagnosticSnapshot-end-line helix.diagnostic-snapshot-end-line)
(define DiagnosticSnapshot-end-column helix.diagnostic-snapshot-end-column)
(define DiagnosticSnapshot-severity helix.diagnostic-snapshot-severity)
(define DiagnosticSnapshot-message helix.diagnostic-snapshot-message)
(define DiagnosticSnapshot-code helix.diagnostic-snapshot-code)
(define DiagnosticSnapshot-source helix.diagnostic-snapshot-source)
(define DiagnosticSnapshot-provider-kind helix.diagnostic-snapshot-provider-kind)
(define DiagnosticSnapshot-provider-id helix.diagnostic-snapshot-provider-id)
(define DiagnosticSnapshot-provider-namespace
  helix.diagnostic-snapshot-provider-namespace)
(define diagnostic-snapshot-open! helix.diagnostic-snapshot-open!)
