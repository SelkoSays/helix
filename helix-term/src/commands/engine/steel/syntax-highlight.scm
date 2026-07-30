(require-builtin helix/core/syntax-highlight as helix.)

(provide syntax-highlight-language-for-path
         syntax-highlight-language-known?
         syntax-highlight-byte-budget
         syntax-highlight-spans)

;;@doc
;; Return the language name Helix would use for `path`, or #false when no
;; language matches. Only the file name is consulted; the file need not exist.
(define syntax-highlight-language-for-path helix.syntax-highlight-language-for-path)

;;@doc
;; Whether `language` names a language this Helix build knows about.
(define syntax-highlight-language-known? helix.syntax-highlight-language-known?)

;;@doc
;; The size in bytes above which highlighting declines, so a caller can decide
;; before asking rather than being surprised by an empty result.
(define syntax-highlight-byte-budget helix.syntax-highlight-byte-budget)

;;@doc
;; Highlight `text` as `language`, returning a list of `(start end style)`.
;;
;; `start` and `end` are character indices, not byte offsets, so they can be
;; used directly with `substring`. Spans are sorted and non-overlapping, and a
;; region with no highlight produces no span at all — a style is a patch over
;; whatever base style the caller renders with.
;;
;; An unknown language, a language with no highlight query, unparsable text, or
;; text over the byte budget all return the empty list rather than raising.
(define syntax-highlight-spans helix.syntax-highlight-spans)
