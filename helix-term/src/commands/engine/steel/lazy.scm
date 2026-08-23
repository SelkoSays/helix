(require-builtin helix/core/lazy as lazy.)

(provide register-lazy-plugin!
         register-async-lazy-plugin!
         register-logical-lazy-plugin!
         discover-lazy-commands
         register-discovered-lazy-plugin!
         register-discovered-logical-lazy-plugin!
         register-logical-plugin!
         mark-logical-plugin-eager!
         logical-plugin-registered?
         logical-plugin-registered-ids
         logical-plugin-strategy
         logical-plugin-materialized?
         lazy-plugin-command-doc)

;;@doc
;; Register commands whose modules and initializers run on first invocation.
(define register-lazy-plugin! lazy.#%register-lazy-plugin!)

;;@doc
;; Register lazy commands and queue compilation after init.scm completes.
;; Evaluation and initialization still happen on first invocation.
(define register-async-lazy-plugin! lazy.#%register-async-lazy-plugin!)

;;@doc
;; Register a logical lazy manifest using precomputed command documentation.
;; This avoids runtime source discovery while retaining native validation.
(define register-logical-lazy-plugin! lazy.#%register-logical-lazy-plugin!)

;;@doc
;; Read annotated command names and documentation from Scheme source without
;; compiling or evaluating the source.
(define discover-lazy-commands lazy.#%discover-lazy-commands)

;;@doc
;; Register commands discovered from annotated Scheme source for lazy activation.
(define (register-discovered-lazy-plugin! name modules initializers
                                          #:sources [sources modules])
  (lazy.#%register-discovered-lazy-plugin! name modules initializers sources))

;;@doc
;; Register a lazy manifest under a separately cataloged logical plugin ID.
(define (register-discovered-logical-lazy-plugin!
          logical-name name modules initializers #:sources [sources modules])
  (lazy.#%register-discovered-logical-lazy-plugin!
    logical-name name modules initializers sources))

;; Internal catalog and state bridge used by the unified plugin loader.
(define register-logical-plugin! lazy.#%register-logical-plugin!)
(define mark-logical-plugin-eager! lazy.#%mark-logical-plugin-eager!)
(define logical-plugin-registered? lazy.logical-plugin-registered?)
(define logical-plugin-registered-ids lazy.logical-plugin-registered-ids)
(define logical-plugin-strategy lazy.logical-plugin-strategy)
(define logical-plugin-materialized? lazy.logical-plugin-materialized?)

(define lazy-plugin-command-doc lazy.lazy-plugin-command-doc)
