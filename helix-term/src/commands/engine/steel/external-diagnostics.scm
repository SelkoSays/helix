(require-builtin helix/core/external-diagnostics as helix.)

(provide external-diagnostic
         external-diagnostics-publish!
         external-diagnostics-clear!)

;;@doc
;; Construct a one-based external diagnostic. Severity is one of
;; "hint", "info", "warning", or "error". Code and source may be #false.
(define external-diagnostic helix.external-diagnostic)

;;@doc
;; Replace every diagnostic in namespace with the supplied diagnostic list.
(define external-diagnostics-publish! helix.external-diagnostics-publish!)

;;@doc
;; Clear one external diagnostic namespace without affecting LSP diagnostics.
(define external-diagnostics-clear! helix.external-diagnostics-clear!)
