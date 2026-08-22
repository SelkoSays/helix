(require-builtin helix/core/archive as helix.)

(provide archive-cancel-token
         archive-cancel!
         archive-open-async
         archive-close!
         archive-path
         archive-format
         archive-fingerprint
         archive-entry-count
         archive-closed?
         archive-stale?
         archive-entries-page
         archive-entry-read-async)

;;@doc
;; Create a cancellation token for one asynchronous archive operation.
(define archive-cancel-token helix.archive-cancel-token)

;;@doc
;; Cancel an archive operation. Repeated cancellation returns false.
(define archive-cancel! helix.archive-cancel!)

;;@doc
;; Open and index a supported local archive asynchronously. The callback
;; receives `(ok handle)`, `(cancelled #false)`, `(stale #false)`, or
;; `(error message)`. Indexing permits at most 100,000 entries, 32 KiB paths,
;; 32 MiB retained metadata, and a 512 MiB decompressed TAR scan.
(define archive-open-async helix.archive-open-async)

;;@doc
;; Close an archive handle. Repeated closure returns false.
(define archive-close! helix.archive-close!)

;;@doc
;; Return the canonical UTF-8 source path retained by an archive handle.
(define archive-path helix.archive-path)

;;@doc
;; Return the detected archive format symbol.
(define (archive-format handle)
  (string->symbol (helix.archive-format handle)))

;;@doc
;; Return the source `(identity size)` fingerprint captured during indexing.
(define archive-fingerprint helix.archive-fingerprint)

;;@doc
;; Return the number of indexed members, including duplicate paths.
(define archive-entry-count helix.archive-entry-count)

;;@doc
;; Return true after an archive handle has been explicitly closed.
(define archive-closed? helix.archive-closed?)

;;@doc
;; Return true when an archive handle is closed or its source changed.
(define archive-stale? helix.archive-stale?)

;;@doc
;; Return a bounded metadata page. Each row contains stable index, lossy path,
;; type, uncompressed/compressed sizes, mode, timestamp, link target,
;; compression method, and encrypted flag. One page is limited to 10,000 rows.
(define archive-entries-page helix.archive-entries-page)

;;@doc
;; Read one bounded uncompressed entry range asynchronously. The callback
;; receives `(ok bytevector truncated?)` or a cancellation/stale/error status.
;; Offsets are limited to 64 MiB and each read to one MiB.
(define archive-entry-read-async helix.archive-entry-read-async)
