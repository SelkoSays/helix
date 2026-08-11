(require-builtin helix/core/binary as helix.)

(provide binary-file-open
         binary-file-close!
         binary-file-path
         binary-file-size
         binary-file-stale?
         binary-file-read
         binary-search-token
         binary-search-cancel!
         binary-file-search)

;;@doc
;; Open one existing local regular file as an opaque, read-only binary handle.
(define binary-file-open helix.binary-file-open)

;;@doc
;; Close a binary handle. Repeated closure returns false.
(define binary-file-close! helix.binary-file-close!)

;;@doc
;; Return the canonical UTF-8 path retained by a binary handle.
(define binary-file-path helix.binary-file-path)

;;@doc
;; Return the byte size captured when a binary handle was opened.
(define binary-file-size helix.binary-file-size)

;;@doc
;; Return true when the handle is closed or the file changed on disk.
(define binary-file-stale? helix.binary-file-stale?)

;;@doc
;; Read at most one MiB from an exact byte offset into a bytevector.
(define binary-file-read helix.binary-file-read)

;;@doc
;; Create a cancellation token for one asynchronous binary search.
(define binary-search-token helix.binary-search-token)

;;@doc
;; Cancel a binary search token. Repeated cancellation returns false.
(define binary-search-cancel! helix.binary-search-cancel!)

;;@doc
;; Search a half-open byte range asynchronously and call back with a structured
;; status/offset pair. Direction is `forward` or `reverse`.
(define (binary-file-search handle token pattern start end direction callback)
  (helix.binary-file-search
    handle token (bytes->list pattern) start end
    (if (symbol? direction) (symbol->string direction) direction)
    callback))
