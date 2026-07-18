(require-builtin helix/core/text-display as helix.)

(provide text-display-width)

;;@doc
;; Return the terminal-cell width of a string using Helix's Unicode width
;; implementation. This is distinct from Steel's character-counting
;; string-length.
(define text-display-width helix.text-display-width)
