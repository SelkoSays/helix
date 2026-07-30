(require-builtin helix/core/regex as helix.)

(provide regex?
         regex-compile
         regex-pattern
         regex-full-match?
         regex-find
         regex-find-all
         regex-replace-all
         regex-expand-match)

;;@doc
;; Whether a value is a compiled regular expression.
(define regex? helix.regex?)

;;@doc
;; Compile a pattern, returning #false when it is invalid.
;;
;; Compilation never raises, because patterns arrive from live user typing and
;; a half-typed pattern is a normal intermediate state.
(define regex-compile helix.regex-compile)

;;@doc
;; The pattern text a regular expression was compiled from.
(define regex-pattern helix.regex-pattern)

;;@doc
;; Whether the whole string matches. This is not a substring search: the
;; pattern is anchored at both ends before testing.
(define regex-full-match? helix.regex-full-match?)

;;@doc
;; The first match as a `(start end)` character range, or #false.
(define regex-find helix.regex-find)

;;@doc
;; Every non-overlapping match as a list of `(start end)` character ranges.
(define regex-find-all helix.regex-find-all)

;;@doc
;; Replace every match, expanding `$1` and `${name}` capture references in the
;; replacement.
(define regex-replace-all helix.regex-replace-all)

;;@doc
;; Expand the replacement for the exact `(start end)` character range in
;; `text`, or return #false when that range is not the next regex match.
(define regex-expand-match helix.regex-expand-match)
