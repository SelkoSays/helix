(require-builtin helix/core/lazy as lazy.)

(provide register-lazy-plugin!
         register-async-lazy-plugin!
         discover-lazy-commands
         register-discovered-lazy-plugin!
         lazy-plugin-command-doc)

;;@doc
;; Register commands whose modules and initializers run on first invocation.
(define register-lazy-plugin! lazy.#%register-lazy-plugin!)

;;@doc
;; Register lazy commands and queue compilation after init.scm completes.
;; Evaluation and initialization still happen on first invocation.
(define register-async-lazy-plugin! lazy.#%register-async-lazy-plugin!)

;;@doc
;; Read annotated command names and documentation from Scheme source without
;; compiling or evaluating the source.
(define discover-lazy-commands lazy.#%discover-lazy-commands)

;;@doc
;; Register commands discovered from annotated Scheme source for lazy activation.
(define (register-discovered-lazy-plugin! name modules initializers
                                          #:sources [sources modules])
  (lazy.#%register-discovered-lazy-plugin! name modules initializers sources))

(define lazy-plugin-command-doc lazy.lazy-plugin-command-doc)
