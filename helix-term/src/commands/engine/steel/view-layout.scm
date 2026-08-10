(require-builtin helix/core/view-layout as helix.)

(provide view-fullscreen?
         view-fullscreen-enter!
         view-fullscreen-leave!
         view-fullscreen-toggle!
         temporary-layout-enter!
         temporary-layout-restore!
         view-resize!
         view-equalize!)

;;@doc
;; Return whether an editor view is currently rendered fullscreen.
(define view-fullscreen? helix.view-fullscreen?)

;;@doc
;; Render the focused view fullscreen without closing sibling views.
(define view-fullscreen-enter! helix.view-fullscreen-enter!)

;;@doc
;; Leave fullscreen and restore the split layout.
(define view-fullscreen-leave! helix.view-fullscreen-leave!)

;;@doc
;; Toggle fullscreen rendering for the focused view.
(define view-fullscreen-toggle! helix.view-fullscreen-toggle!)

;;@doc
;; Suspend the current split layout, display the requested DocumentId, and
;; return an opaque restoration token.
(define temporary-layout-enter! helix.temporary-layout-enter!)

;;@doc
;; Restore the most recently suspended layout identified by token.
(define temporary-layout-restore! helix.temporary-layout-restore!)

;;@doc
;; Grow or shrink the focused split branch along width or height by a signed
;; terminal-cell delta. Return the signed number of cells actually moved.
(define view-resize! helix.view-resize!)

;;@doc
;; Restore equal proportions in the focused view's immediate split group.
(define view-equalize! helix.view-equalize!)
