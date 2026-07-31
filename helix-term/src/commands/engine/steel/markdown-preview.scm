(require-builtin helix/core/markdown-preview as helix.)

(provide markdown-render
         markdown-render-text
         markdown-render-width
         markdown-render-source-unchanged?
         markdown-render-styles
         markdown-render-headings
         markdown-render-links
         markdown-render-code-blocks
         markdown-render-media
         markdown-render-mappings
         markdown-render-anchor-output
         markdown-render-source-for-output
         markdown-render-output-for-source
         markdown-render-apply-focused!
         markdown-render-clear-focused!
         markdown-preview-open-target!)

;;@doc
;; Render Markdown into an opaque native result. `source-path` is a string or
;; `#false`; relative targets remain unresolved when it is false. Width is in
;; terminal cells.
(define markdown-render helix.markdown-render)

;;@doc
;; Return the plain preview-buffer text held by a render.
(define markdown-render-text helix.markdown-render-text)

(define markdown-render-width helix.markdown-render-width)
(define markdown-render-source-unchanged? helix.markdown-render-source-unchanged?)

;; Each style row is `(start end scope-or-style)` in character indices.
(define markdown-render-styles helix.markdown-render-styles)

;; Heading rows are `(level title anchor output source-start source-end)`.
(define markdown-render-headings helix.markdown-render-headings)

;; Link rows are `(label destination resolved? output-start output-end
;; source-start source-end)`.
(define markdown-render-links helix.markdown-render-links)

;; Code rows are `(language exact-text output-start output-end source-start
;; source-end)`.
(define markdown-render-code-blocks helix.markdown-render-code-blocks)

;; Media rows are `(alt destination resolved? remote? output-start output-end
;; source-start source-end)`.
(define markdown-render-media helix.markdown-render-media)

;; Mapping rows are `(output-start output-end source-start source-end node-id)`.
(define markdown-render-mappings helix.markdown-render-mappings)
(define markdown-render-anchor-output helix.markdown-render-anchor-output)
(define markdown-render-source-for-output helix.markdown-render-source-for-output)
(define markdown-render-output-for-source helix.markdown-render-output-for-source)

;; Apply the render's theme and concrete styles only when the focused document
;; is byte-for-byte the render's output text.
(define markdown-render-apply-focused! helix.markdown-render-apply-focused!)
(define markdown-render-clear-focused! helix.markdown-render-clear-focused!)

;; Open HTTP(S)/mailto externally or an absolute local target through Helix.
;; Internal anchors return false. Unknown schemes are rejected.
(define markdown-preview-open-target! helix.markdown-preview-open-target!)
