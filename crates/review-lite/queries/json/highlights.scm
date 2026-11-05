; Copied from Zed's crates/grammars/src/json/highlights.scm at commit
; c7bea343838f42b4c14151393e41f0bbe033c95e (GPL-3.0-or-later).

(comment) @comment

(string) @string

(escape_sequence) @string.escape

(pair
  key: (string) @property.json_key)

(number) @number

[
  (true)
  (false)
] @boolean

(null) @constant.builtin

[
  ","
  ":"
] @punctuation.delimiter

[
  "{"
  "}"
  "["
  "]"
] @punctuation.bracket
