; Derived from the highlights of cathaysia/tree-sitter-jinja
; (https://github.com/cathaysia/tree-sitter-jinja), tree-sitter-jinja/queries/highlights.scm at
; tag v0.13.1, commit f46e979278123e3b6949381896f3917f378af068, under the Apache License,
; Version 2.0 (https://www.apache.org/licenses/LICENSE-2.0).
;
; Changed for cr: the captures take Helix's names, an `elif` branch captures only its `elif`
; keyword rather than its whole body, a raw block's body is not captured, so its text keeps the
; language of the file the template produces, and every keyword is captured only under the
; statement or expression that holds it.
; The bare `"in"` of the loop keywords is gone: a loop's `in` is a `binary_operator`, which keeps
; its capture, and the bare token occurs only in a test such as `is in`, captured below.

[
  "{{"
  "{{-"
  "{{+"
  "+}}"
  "-}}"
  "}}"
  "{%"
  "{%-"
  "{%+"
  "+%}"
  "-%}"
  "%}"
] @keyword.directive

(string_literal) @string

(number_literal) @constant.numeric.integer

(float_literal) @constant.numeric.float

(boolean_literal) @constant.builtin.boolean

(null_literal) @constant.builtin

(builtin_test
  "defined" @constant.builtin)

(comment) @comment

[
  ","
  "."
  ":"
] @punctuation.delimiter

[
  (attribute_ignore)
  (attribute_context)
] @attribute

(for_statement
  "recursive" @attribute)

[
  "("
  ")"
  "["
  "]"
  "<"
  ">"
] @punctuation.bracket

(binary_operator) @operator

(block_statement
  [
    "block"
    "required"
  ] @keyword)

(with_statement
  "with" @keyword)

(attribute_context
  "with" @keyword)

(filter_statement
  "filter" @keyword)

(macro_statement
  "macro" @keyword)

(set_statement
  "set" @keyword)

(set_block_statement
  "set" @keyword)

(trans_statement
  "trans" @keyword)

(pluralize_statement
  "pluralize" @keyword)

(autoescape_statement
  "autoescape" @keyword)

[
  (endtrans_statement)
  (endblock_statement)
  (endwith_statement)
  (endfilter_statement)
  (endmacro_statement)
  (endcall_statement)
  (endset_statement)
  (endautoescape_statement)
] @keyword

(do_statement
  "do" @keyword)

(include_statement
  "include" @keyword.control.import)

(import_statement
  "import" @keyword.control.import)

(import_from
  "from" @keyword.control.import)

(extends_statement
  "extends" @keyword.control.import)

(import_as
  "as" @keyword.control.import)

(import_statement
  (identifier) @variable)

(import_as
  (identifier) @variable)

[
  (else_statement)
  (endif_statement)
] @keyword.control.conditional

(if_statement
  "if" @keyword.control.conditional)

(ternary_expression
  "if" @keyword.control.conditional)

(elif_statement
  "elif" @keyword.control.conditional)

(endfor_statement) @keyword.control.repeat

(for_statement
  "for" @keyword.control.repeat)

(statement
  [
    "continue"
    "break"
  ] @keyword.control.repeat)

(call_statement
  "call" @function)

(function_call
  (identifier) @function)

(arg
  (identifier) @variable.parameter)

(arg
  (expression
    (binary_expression
      (unary_expression
        (primary_expression
          (identifier) @variable.parameter)))))

(primary_expression
  "."
  (identifier) @variable.other.member)

(assignment_expression
  "."
  (identifier)+ @variable.other.member)

(inline_trans
  "_" @function.builtin)

(statement
  "debug" @function.builtin)

(raw_start) @keyword

(raw_end) @keyword

(builtin_test
  [
    "boolean"
    "even"
    "in"
    "mapping"
    "sequence"
    "callable"
    "integer"
    "ne"
    "string"
    "defined"
    "filter"
    "iterable"
    "none"
    "test"
    "divisibleby"
    "float"
    "le"
    "number"
    "eq"
    "ge"
    "lower"
    "odd"
    "undefined"
    "escaped"
    "gt"
    "lt"
    "sameas"
    "upper"
  ] @keyword.operator)
