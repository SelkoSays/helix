(require-builtin helix/core/lazy as lazy.)

(provide register-lazy-plugin!
         register-async-lazy-plugin!
         lazy-plugin-command-doc)

;;@doc
;; Register commands whose modules and initializers run on first invocation.
(define register-lazy-plugin! lazy.#%register-lazy-plugin!)

;;@doc
;; Register lazy commands and queue compilation after init.scm completes.
;; Evaluation and initialization still happen on first invocation.
(define register-async-lazy-plugin! lazy.#%register-async-lazy-plugin!)

(define lazy-plugin-command-doc lazy.lazy-plugin-command-doc)
