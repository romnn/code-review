; Derived from the injections of cathaysia/tree-sitter-jinja
; (https://github.com/cathaysia/tree-sitter-jinja), tree-sitter-jinja/queries/injections.scm at
; tag v0.13.1, commit f46e979278123e3b6949381896f3917f378af068, under the Apache License,
; Version 2.0 (https://www.apache.org/licenses/LICENSE-2.0).
;
; Changed for cr: the template's literal text, raw blocks' bodies included, is HTML unless the
; file's name says otherwise, as in Helix's other template languages, and the anonymous tokens
; inside it are part of it.
; cr's fork of the grammar has no line statements, so there is no `jinja_inline` to inject.

([
  (content)
  (raw_body)
] @injection.content
  (#set! injection.language "html")
  (#set! injection.combined)
  (#set! injection.include-unnamed-children))
