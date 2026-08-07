(require-builtin helix/core/json as helix.)

(provide json-parse
         json-parse-lines)

;;@doc
;; Parse one JSON value into ordinary Steel values. The optional byte limit
;; defaults to eight MiB and is checked before parsing.
(define (json-parse input #:max-bytes [max-bytes (* 8 1024 1024)])
  (helix.json-parse-bounded input max-bytes))

;;@doc
;; Parse newline-delimited JSON into a list. Blank lines are ignored. Errors
;; name the one-based input line. Limits default to eight MiB and 100000 items.
(define (json-parse-lines input
                          #:max-bytes [max-bytes (* 8 1024 1024)]
                          #:max-items [max-items 100000])
  (helix.json-parse-lines-bounded input max-bytes max-items))
