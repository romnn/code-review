# Plan

A fast, native desktop tool for reviewing large, often AI-written diffs and GitHub pull requests.
It remembers what you have already reviewed down to the hunk, across pushes and force-pushes,
walks you through a change in a sensible order, and keeps reformatting and moved code from
drowning the real edits. See [prior-work.md](prior-work.md) for the survey this plan builds on.

We build the review engine once and put two desktop front ends on it, then keep the one that is
easier to maintain and grow:

- **lite**: a standalone app on `gpui` and `gpui-component`, copying the GPL Zed code it needs
  (LSP client and similar).
- **zed**: a fork of Zed with a review crate added, inheriting Zed's editor, multibuffer, LSP for
  every language, highlighting and vim mode.

No web stack and no JavaScript: everything is Rust.

## Goals

1. **Seen state per hunk that survives pushes.** After a push, rebase or force-push, only new or
   changed hunks are unreviewed; changed hunks show what changed since you last reviewed them.
2. **Format-independent diff.** Changes that provably only touch whitespace, indentation or line
   wrapping are classified as formatting-only and hidden behind a visible count; real edits inside
   reformatted code are highlighted.
3. **Move detection.** Code moved within or across files is shown as one line, "moved from
   `a.rs:120-160`"; moves with edits show their edits.
4. **Guided walkthrough.** An ordered list of steps, each showing its own hunks and why it matters.
   Walkthroughs are validated against every revision: each review unit is covered exactly once.
5. **Private annotations.** Accept, flag or annotate hunks; notes follow their hunk across
   revisions and fall back to the file when the hunk disappears.
6. **Agent round trip.** Export accepted, flagged and annotated hunks as numbered Markdown or JSON
   to hand back to Claude or Codex; agents can also drive the CLI.
7. **Code browsing with LSP** at the reviewed revision: go to definition, find references, hover.
8. **Fast on huge diffs and live on pushes.** Virtualized rendering, analysis off the UI thread, and
   a banner on new pushes instead of a reload.

The overriding safety rule: **the tool must never make a reviewer believe they reviewed code they
did not.** Whenever identity, equivalence or a move is ambiguous, it reopens the code or shows it
in full rather than guessing.

## Non-goals for the MVP

- Posting comments to GitHub (export only; posting comes after the MVP).
- Multi-user or server features; review state is local to one reviewer.
- Structural (AST) diff rendering. Tree-sitter is used only to prove formatting equivalence.
- AI walkthrough generation inside the app (the CLI emits a prompt and imports the result).

## Concepts

- **Review**: what is being reviewed, identified by a stable key. Either a GitHub PR
  (`github.com/owner/repo#123@<git-common-dir>`) or a local range in a repository
  (`<repo>:<base-ref>..<head-ref>`, or the working tree against a ref for reviewing an agent's
  uncommitted turn). A PR review is keyed per clone because its snapshots are pinned in that
  clone's object store: opening the same PR from another clone starts a separate review instead
  of reading history whose objects that clone lacks. Worktrees of one clone share its common git
  directory and therefore its review.
- **Snapshot**: an immutable commit. Either a real commit, or a synthetic commit capturing the
  working tree (see "Snapshots of uncommitted work").
- **Revision**: one observed version of a review, identified by its `(base, head)` snapshot IDs,
  where `base` is the merge base of the target and the head. A new revision is recorded whenever
  either changes. Revisions are numbered from 1 in observation order and never forgotten; they form
  a linear chain. An observation is deduplicated only against the **current** revision, the last
  one: a snapshot pair seen earlier, such as after a force-push back to an earlier head, becomes a
  new revision (its content-addressed analysis is reused), so A, B, then A again are revisions 1,
  2 and 3. Every command and front end defaults to the current revision.
- **Review unit**: the atomic unit of review state, local to one revision. Either a **chunk** (a
  maximal run of changed lines in one file with zero context lines) or a **file unit** for a change
  that is not a line change: mode change, type change, submodule update, binary or oversized
  content, empty-file addition or deletion, or a rename. File units are produced independently of
  chunks, so a file whose text and mode both change has both, and a type change (for example a
  symlink replaced by a regular file) keeps chunks for every side that is a regular text file,
  diffed against an empty other side. File units show the values that changed: modes, symlink
  targets (escaped losslessly, with `\xNN` for bytes that are not UTF-8) and submodule commits.
  Every changed tree entry yields at least one review unit, so progress and walkthrough coverage
  cannot miss a change.
- **Unit and move IDs**: `u<revision>-<index>` and `m<revision>-<index>`, such as `u3-0007` and
  `m3-0002`, local to one revision. Indices are zero-based and written with at least four digits,
  so the first unit and the first move of revision 1 are `u1-0000` and `m1-0000`.
- **Hunk**: the display unit. Units within a few lines of each other are shown together, like a
  unified-diff hunk. A hunk is reviewed when all its units are.
- **Correspondence**: a mapping between the units of two revisions, computed from positions in
  both snapshots and confirmed by content (see "Carrying state across revisions"). Review state
  moves between revisions only along an unambiguous correspondence.
- **Mark**: a reviewer's verdict on a unit at a revision: `seen`, `accepted`, `flagged`, or
  `unseen`, which explicitly requires review again and stops inheritance from earlier marks. A mark
  records the unit's full evidence (both sides of the change, paths, modes, object IDs) and the
  revision it was made on. Verdicts, including move acknowledgements, go only on units of the
  current revision, checked under the lock; with revisions in observation order, review state then
  always flows forward in time and an older verdict can never overtake a newer one. Notes may go on
  any revision.
- **Note**: free text attached to a unit at a revision, carried along correspondences.
- **Walkthrough**: ordered steps over the units of the revision it was written for, carried to
  later revisions through correspondences.

## Engine design

### Git access

Shell out to the `git` CLI rather than linking a git library: it supports every repository layout
users have (partial clones, worktrees, alternates), matches what `git diff` shows, and keeps the
engine's dependency tree small enough to embed in Zed without version conflicts. The git layer
produces verified immutable inputs; everything after it is pure analysis over object IDs.

- A long-lived `git cat-file --batch` process reads blobs.
- `git diff-tree -r -z --raw -M <base> <head>` lists changed entries with modes, object IDs and
  rename detection.
- GitHub PR metadata (target branch, head) comes from `gh pr view <n> --json ...`.
- Fetches go into operation-specific refs (`refs/code-review/tmp/<id>`), never `FETCH_HEAD`, and
  snapshot identity is taken from the fetched objects, not from an earlier observation. Every fetch
  passes `--no-write-fetch-head --refmap=` so a remote's configured refspecs never update
  remote-tracking refs; nothing outside `refs/code-review/*` changes.
- Every snapshot is pinned under `refs/code-review/<review>/snap/<object-id>`, named by its own
  object ID, so pinning is idempotent: a ref that already exists holds the same object by
  construction. A force-push or GC cannot lose a pinned snapshot, and a crash after pinning leaves
  only a harmless extra pin, reconciled against the event log later.
- `git merge-base --all` computes the base. Several merge bases are reported to the user and the
  first is used. A shallow clone without the merge base is deepened with `--deepen` from the PR's
  remote (or `origin` for local reviews), wanting the target and the clone's shallow boundary
  commits but never the head, which may be a synthetic working-tree capture or an unpublished
  commit; wants the remote does not know are dropped one by one. Deepening has the same isolation
  as every other fetch; when it cannot find a merge base, the review fails with an explicit error.
- Partial clones: analysis reads every blob it needs, which makes git fetch missing objects, and
  the analysis cache stores the review evidence (the text of each unit), so past reviews stay
  reproducible offline. Browsing an old revision needs its objects and may need the network.

### Snapshots of uncommitted work

To review an agent's uncommitted turn, capture the working tree without touching the user's index:
with a temporary `GIT_INDEX_FILE` holding a copy of the user's index (which keeps git's stat cache
and sparse-checkout entries; `git read-tree HEAD` when there is no index), `git add -A` (tracked
and untracked files, respecting `.gitignore`), `git write-tree` and `git commit-tree` with a fixed
identity and date produce a synthetic snapshot commit, pinned like any other. Staged and unstaged changes are captured together. Two captures of
different contents are two revisions; a capture identical to the current revision deduplicates.

### Line diff and units

For each changed entry, diff old and new blobs line by line with imara-diff (Histogram). Each
change region with zero context becomes a chunk. Path, mode, type and opaque-content changes become
file units (see "Review unit"), identified by their old and new paths, modes, types and object IDs;
an entry can have both chunks and file units.

### Format-independent classification

Formatting-only is a claim that two pieces of code are equivalent, so it is only made when it can
be proven, and is never used as identity. Three review rounds found new holes in the previous
canonical form, which normalized every gap between tree nodes and then patched exceptions (a
blacklist). This section specifies its replacement, a conservative canonical form in which every
normalization is positively justified. It is implemented in `review-core/src/syntax.rs` as
`ANALYSIS_FORMAT` 9. Every syntax token is kept: the form normalizes whitespace only, never
punctuation.

#### What equivalent means

Two sources are equivalent when every conforming implementation of the language accepts both or
neither and gives them the same meaning, except for source positions: line and column numbers
(`line!()`, `column!()`, `Location::caller`, proc-macro span positions, Python tracebacks and
`co_firstlineno`, stack traces, debug info) and source-text reflection (`inspect.getsource`,
`Function.prototype.toString`) may differ. Every reformatting changes those, so no formatter
preserves them. Documentation tools are out of scope too, except that comments stay verbatim.

The carve-out has a plain limit: code that branches on its own source positions or reflected
source text, such as a test asserting `line!()` or a function printing its own source, can
behave differently after a reformatting the proof accepts. That is accepted and documented, not
detected.

#### The proof

- Supported languages are parsed with tree-sitter on the **whole** old and new blobs. A parse with
  any `ERROR` or `MISSING` node proves nothing. Grammar acceptance is evidence that the structure
  matches, never proof that the source is valid: every lexical rule the proof relies on comes from
  the language's specification, not from the grammar.
- A **file** is formatting-only when the canonical forms (below) of both sides are equal.
- A **chunk** of a file with real changes, whose tokens are unchanged (the `whitespace_only`
  hint), is formatting-only when two hybrids keep the canonical forms: the old file with only
  this chunk applied must canonicalize like the old file, and the new file with only this chunk
  reverted like the new file. Requiring both proves the chunk a pure reformat in the old file and
  beside every other change in the new one; another chunk that opens a string around it breaks
  the second comparison. Both proofs use the same canonical form. A parse error in any of the four
  trees leaves the chunk unproven. Each candidate parses two hybrids about the file's size, and past
  64 MiB of hybrids per file the remaining candidates stay unproven.
- `Unit::formatting_only` carries either proof, `FileChange::formatting_only` the whole-file one.
  Changing the canonical form bumps `ANALYSIS_FORMAT`.
- Unproven chunks keep a token-level diff that highlights what really changed. Nothing is hidden.

#### The canonical form

The canonical form is lossless by default: every byte of the source belongs to exactly one item,
and only items positively classified as trivia are normalized.

1. **Items.** The serializer walks the tree in source order and emits node kinds, field names and
   nesting as today, and for the bytes:
   - a **token**: a leaf whose kind the language's lexical table (below) classifies as an
     identifier, keyword, punctuator or operator, recorded with its exact text;
   - an **opaque literal**: a node of a kind the table lists as literal (strings and their
     interpolations, characters, numbers, regular expressions, template literals, JSX text, TOML
     quoted keys and dates, comments), recorded with its exact bytes, hidden children included;
   - a **prefix**: the bytes before the first item, recorded exactly (a JavaScript or Rust
     hashbang must start at byte 0);
   - a **gap**: the bytes between two adjacent items.
2. **Unclassified means significant.** A leaf kind the table does not classify, and every gap next
   to one, is recorded byte for byte. A gap inside an opaque literal does not exist: it is part of
   the literal. This is what makes hidden grammar content, such as the whitespace inside a TOML
   quoted key, impossible to drop: the quote leaves are not tokens, so the gap between them is
   exact.
3. **Trivia alphabet.** Each language lists the bytes that are whitespace between its tokens. A
   gap is trivia only if it lies between two classified items and holds only bytes from that list;
   any other byte (a vertical tab in JSON, a no-break space in Python) makes it exact.
4. **Line breaks and touching.** A trivia gap records two things. Whether it holds a line break
   (how many and how much horizontal whitespace are never recorded), unless the gap is a **wrap
   site** of the language (below). And whether it is empty, so the two items touch, unless the
   language shows that touching cannot matter there (*separable* gaps, below). Touching is kept
   by default because some languages read touching tokens as one lexeme or forbid them: Rust's
   `1custom` is one suffixed literal to a macro, and TypeScript rejects `1as number`. Gaps next
   to a comment are never wrap sites: a line comment ends at a break, JavaScript's `-->` comment
   must start a line, and Go and JavaScript treat a comment containing a break as a break. Next
   to a comment, whether there is a break and whether there are two or more (a blank line) is
   recorded. The gap before a Go directive comment (`//line`, `/*line`, `//go:`, `//export`,
   `//extern`, `// +build`) is exact, since a directive's place on its line is part of its
   meaning; a cgo preamble's distance to `import "C"` is kept by the comment rule. Python's
   encoding declaration counts only on line 1 or 2, which the exact prefix and the comment rule
   keep.
   After an unclassified item, the rest of the file is exact even when it is empty.
5. **Indentation.** Python records the exact indentation bytes of every logical line (below).
6. **Completeness check.** For each pinned grammar, a test enumerates every terminal kind the
   grammar's `node-types.json` lists and fails unless the lexical table classifies it as token,
   literal, comment, structure or deliberately unclassified. A grammar upgrade that adds a kind
   turns the test red instead of silently normalizing it.
7. **Linear cost.** The context a gap is judged in (the node whose children it separates, the
   enclosing JSX name, the parent kind) is carried down the walk, so no gap looks up through
   its ancestors; a 25,000-term left-deep expression serializes in a fraction of a second.

Separable gaps, where touching is not recorded, are only three classes. The grammars split
sequences that the real lexers merge or reject (Go reads `a<-b` as a receive, JavaScript reads
`<!--` as a comment, Rust rejects `0x1.0` and reserves `foo#bar`), so touching next to other
punctuation is never assumed harmless:

- **A bracket `( ) [ ] { }` on either side.** A bracket is a one-character token in every
  supported language, and no longer token contains one outside a literal, which is opaque, so
  whitespace next to it can neither join it to a neighbour nor split a neighbour. Rust
  (Reference, "Delimiters"): brackets reach a macro as a `Group` without spacing; punctuation
  before a bracket still keeps its touching, since its own `Spacing` reaches the macro. Python
  (Language Reference, "Delimiters"), JavaScript and TypeScript (ECMA-262, "Punctuators"; a
  template's `${` and `}` belong to the opaque template literal) and Go (specification,
  "Operators and punctuation"): brackets are one-character tokens. TOML 1.0.0: whitespace inside
  arrays and inline tables and around a header's key "is ignored"; the `[[` and `]]` of array
  tables are tokens of their own and keep touching.
- **A chain link in Rust, JavaScript and TypeScript:** a `.` (and `?.` in JavaScript and
  TypeScript) right after an identifier, `self`, `Self`, `super`, `crate` or `this`, where
  rustfmt and prettier break chains. An identifier is a maximal token, so a following `.`
  cannot join it. Rust (Reference, "Identifiers", "Reserved prefixes"): the only characters
  that make a touching identifier lex differently are `#`, `"` and `'`, and `proc_macro::Ident`
  carries no spacing. ECMA-262 ("Punctuators", "Names and Keywords"): after an
  `IdentifierName`, `.` is always the punctuator, and `?.` is the `OptionalChainingPunctuator`
  unless a digit follows, which the unchanged tokens show; only a numeric literal followed by
  `.` lexes differently, and literals are excluded. Python and Go do not get it: black wraps
  inside brackets, and gofmt keeps the dot at the end of the line.
- **JSON, every gap** (RFC 8259, section 2): whitespace is insignificant around the structural
  characters, every gap is next to one, and JSON has no comments or operators that touching
  could form.

The cost: spacing normalization next to operators and commas (`f(a,b)` to `f(a, b)`, `x=y+1` to
`x = y + 1`) stays unproven, and so does a `.` after a literal or other punctuation.

#### Per language

Each wrap site carries a one-line justification from the specification; a language or construct
without one stays unproven.

**Rust** (The Rust Reference, "Whitespace" and "Tokens").
- Alphabet: Pattern_White_Space (U+0009–U+000D, U+0020, U+0085, U+200E, U+200F, U+2028,
  U+2029); line break: LF.
- Wrap sites: every gap between two non-comment tokens. *The Reference: "all forms of
  whitespace serve only to separate tokens in the grammar, and have no semantic significance."*
- Touching: whether a punctuation token (any non-delimiter punctuator, with the `'` of lifetimes
  and labels and the `$` of metavariables counted as punctuation) touches the **next token of
  any kind** is recorded everywhere, inside and outside macros. *`proc_macro::Punct::spacing` is
  `Joint` when the next character is punctuation, and attribute macros receive whole items, so
  any punctuation may reach a macro; `stringify!` prints spacing from the same information.*
  This one rule replaces today's special cases for lifetimes, metavariables, `$x:frag` bindings
  and tuple-index chains. Delimiters `()[]{}` are groups, not punctuation, so `vec![\n1, 2\n]`
  still reflows. rustfmt leaves adjacency alone when it rewraps formatted code, but changes it
  when it normalizes unformatted code (`b+-1` to `b + -1`, `r=&&x` to `r = &&x`); those
  formatting passes stay unproven.
- Literals: strings, raw strings, byte strings, C strings, characters, numbers, comments (doc
  comments included), the hashbang.

**Go** (The Go Programming Language Specification, "Semicolons", "Tokens").
- Alphabet: space, tab, CR, LF; line break: LF.
- Wrap sites:
  - a gap whose preceding token is **not** an identifier, a basic literal, one of the keywords
    `break`, `continue`, `fallthrough`, `return`, or one of `++ -- ) ] }`. *The lexer inserts a
    semicolon after exactly those tokens when a line ends, so a line break anywhere else
    produces the same token stream.* Identifiers are lexical: `nil`, `true`, `false` and `iota`
    are identifiers whatever node kind the grammar gives them, and so trigger insertion;
  - a gap before the `}` closing a block, a struct's field list, an interface, a switch or a
    select. *"A semicolon may be omitted before a closing `}`," so one inserted there changes
    nothing.* A composite literal's `}` is not one of these.

  This keeps breaks after a comma, an opening delimiter or a binary operator and before a block's
  closing brace, and rejects `g(1\n+ 2)`, `func f(x\nint)` and `[]int{1\n}`.
- Touching: recorded except next to a bracket (separable gaps above); a break after an
  identifier inserts a semicolon in Go, so chains get no exception here.
- Literals: interpreted and raw strings and runes, all opaque, and numbers; comments.

**Python** (The Python Language Reference, "Lexical analysis").
- Alphabet: space, tab, form feed; line breaks: LF, CR, CRLF.
- Wrap sites: gaps inside parentheses, brackets and braces. *Implicit line joining: "Expressions
  in parentheses, square brackets or curly braces can be split over more than one physical line
  without using backslashes."* Outside brackets a break is a NEWLINE token and stays recorded;
  blank lines are not (*"A logical line that contains only spaces, tabs, formfeeds and possibly
  a comment, is ignored"*).
- Indentation: the exact leading bytes of every logical line outside brackets. An explicit line
  continuation (a backslash and a line break between tokens) is unclassified, so it and the gaps
  on both sides, the next line's indentation included, are exact.
- Literals: strings of every prefix, with f-string and t-string replacement fields (`f"{x=}"`
  prints its own source text), implicit concatenations element by element, numbers, comments.

**JavaScript, TypeScript and TSX** (ECMA-262 16th edition, 12.10 "Automatic Semicolon
Insertion"; TypeScript has no specification, so its rules come from the TypeScript 6.0 parser,
`parser.ts`).
- Alphabet: tab, VT, FF, space, U+00A0, U+FEFF, other Zs; line breaks: LF, CR, U+2028, U+2029.
- Wrap sites, and nowhere else (so `interface\nI`, `throw\nx`, `x\nas T` stay recorded):
  - after `(`, `[`, `{`, `,`, `;` — *no restricted production has [no LineTerminator here]
    right after a punctuator, and the token after it is parsed the same way on either line;*
  - before `)`, `]`, `}` — *ASI before a closing punctuator inserts nothing a closing punctuator
    does not already end;*
  - after a binary or assignment operator except `<`, `>`, `!`, `in`, `instanceof`, `as`,
    `satisfies` — *a binary operator needs a right operand, so ASI cannot fire after it; `<` and
    `>` are excluded because TypeScript type arguments make line breaks around them
    significant;*
  - after `=>` — *only the break before `=>` is restricted;*
  - after the spread `...` — *no restricted production follows it;*
  - before `.` and `?.` when the previous token is an identifier, `this`, `super`, `)` or `]` —
    *MemberExpression and OptionalChain have no restriction before the dot, and the dot cannot
    start a statement;*
  - before `?` and `:` of a conditional expression or conditional type, and before `|` and `&`
    of a union or intersection type — *no restriction precedes them in ECMA-262 or in TypeScript
    6.0's parser;*
  - inside a JSX opening or self-closing tag, between its name, attributes and closing `>` or
    `/>` — *JSX attributes are separated by JavaScript whitespace.*
- Touching: recorded except next to a bracket and between an identifier and a following `.`
  or `?.` (separable gaps above); inside JSX element names every gap is exact.
- Literals: strings, template literals with their substitutions, regular expressions, numbers,
  comments (including `<!--` and `-->` HTML-like comments), JSX text, the hashbang.

**JSON** (RFC 8259, section 2).
- Alphabet: space, tab, LF, CR; wrap sites: every gap. *"Insignificant whitespace is allowed
  before or after any of the six structural characters."*
- Literals: strings and numbers; `true`, `false` and `null` are tokens. A comment is not JSON: a
  file with one stays unproven.

**TOML** (TOML 1.0.0).
- Alphabet: space, tab; line breaks: LF, CRLF.
- Wrap sites: only the direct gaps between an array's own children — `[`, its values, its commas
  and `]` — never a gap deeper inside a value. *"Arrays can span
  multiple lines … any number of newlines and comments may precede values, commas, and the
  closing bracket."* Everywhere else a break ends a key/value pair and stays recorded; inline
  tables are single-line in TOML 1.0.
- Literals: basic, literal and multi-line strings, quoted keys, bare keys, numbers, booleans,
  dates and times (which can contain a space), comments.

#### Commas and other punctuation

Every syntax token is kept, so a reflow that adds or removes a comma, a semicolon or a parenthesis
is not formatting-only. An earlier draft dropped optional trailing commas; review showed that
macros and TypeScript observe them. Comma normalization may come later, one grammar production at
a time and after measuring how much coverage it buys, and each production must pass these negative
fixtures first:

- Rust: `S { ..s, }` is invalid while `S { ..s }` is valid; every item an attribute or derive
  macro can see (the annotated item and everything inside it, including conditional attributes)
  observes its commas.
- Python: `f(x for x in xs,)` is invalid; a one-element tuple needs its comma, and in a subscript
  `a[1,]` is a tuple.
- JavaScript: a lone comma in an array is an elision (`[,]` has one element).
- Go: a trailing comma disambiguates type parameters (`[T any,]`).
- TOML: inline tables allow no trailing comma.
- TypeScript: type arguments forbid one (`f<string,>()`, diagnostic 1009), and in TSX, `.mts`
  and `.cts` the comma in `<T,>` decides whether a generic arrow function parses.

#### Coverage

What stays proven. Rows marked *tested* have a test in `syntax.rs`; the others are expectations
from the rules, without measurement on real formatter output.

| Formatter | Reflow | Proven |
|---|---|---|
| rustfmt | Block expansion: `for entry in &entries[..take] { self.apply(entry); }` over three lines | yes, tested |
| rustfmt | Chains broken before `.`, after `)` or an identifier, binary operators wrapped | yes, tested |
| rustfmt | Re-indentation | yes, expected |
| rustfmt | Breaking a list vertically when that adds a trailing comma | no |
| rustfmt | Breaking a list vertically without a comma change | yes, expected |
| rustfmt | Spacing normalization of unformatted code (`b+-1`, `x=&&y`, `foo(a,b)`) | no: punctuation's spacing reaches macros |
| rustfmt | Import sorting, brace removal in `use`, match arm commas | no |
| prettier | Arguments, objects, arrays broken vertically when that adds a trailing comma | no |
| prettier | Chains broken before `.` or `?.`, wraps after binary operators and `=>`, ternaries with leading `?` and `:` | yes, tested |
| prettier | Union types reflowed with a leading `\|` | no: it adds a token |
| prettier | JSX attributes one per line | yes, expected |
| prettier | Quote normalization, added semicolons or parentheses, JSX text re-wrapping | no |
| gofmt | Indentation, alignment of fields and trailing comments, block expansion | yes, tested |
| gofmt | Lists broken after commas without a comma change | yes, tested |
| gofmt | `gofmt -s` simplifications, import sorting | no |
| black | Wrapping inside brackets without a comma change, blank lines | yes, tested |
| black, prettier, gofmt | Spacing next to operators and commas (`f(a,b)` to `f(a, b)`, `x=y+1`) | no: touching is kept |
| any | Spacing inside brackets (`f( a )` to `f(a)`) | yes, tested |
| black | Magic trailing comma, added parentheses, quote normalization, numeric literal case | no |
| JSON | Pretty-printing | yes, tested |
| TOML | Arrays reflowed without a comma change | yes, expected |
| TOML | Alignment and spacing around `=` | no: touching is kept |

#### Tests

The tests in `syntax.rs` and `analysis/build.rs` pin the rules:

- `every_terminal_kind_is_classified` holds every pinned grammar's terminal kinds to the lexical
  tables. It is an upgrade alarm: it cannot show that a kind is classified correctly.
- `review_counterexamples_are_not_equivalent` holds every counterexample of the reviews, both
  ways round, and `significant_whitespace_chunks_are_not_proven_beside_real_changes` runs three
  of them through the per-chunk proof.
- `spacing_next_to_punctuation_stays_unproven` pins the coverage given up: comma and operator
  spacing, and a `.` after a literal or `#`; `chains_break_after_an_identifier` pins the chain
  link that stays proven.
- `deep_trees_serialize_in_linear_time` serializes 25,000-term left-deep sums in Rust, TSX and
  Go within a generous bound.
- `common_reflows_stay_equivalent` holds the reflows that must stay proven, among them the
  `for entry in &entries[..take] { self.apply(entry); }` block expansion of the demo.
- The older tests kept their verdicts, except two that flipped to not proven: `a + -b` against
  `a +-b` outside macros (Rust touching), and a re-indented explicit Python continuation (exact
  continuation gaps). Trailing-comma cases stay unproven, as before.

| Round | Counterexample | Rejected by |
|---|---|---|
| 2 | Python `# \` comment, then an over-indented `b()` | indentation of every logical line |
| 2 | `throw` + break + `new Error()` | no wrap site after `throw` |
| 2 | `return`, `break`, `continue`, `yield`, `async`, `=>`, postfix `++` with a break | no wrap site there |
| 2 | Macro `++` against `+ +` | Rust touching |
| 2 | Bare CR, U+2028 and U+2029 as breaks; a break inside a comment | line-break alphabets; comment gaps |
| 3 | TypeScript `interface` + break + `I` | no wrap site after `interface` |
| 3 | Macro `+'a` against `+ 'a` | Rust touching, `'` as punctuation |
| 3 | `#[policy] fn f() { a + -b }` against `a +-b` | Rust touching everywhere |
| 3 | Go `g(1\n+ 2)`, `func f(x\nint)`, `[]int{1\n}` | Go's wrap sites follow semicolon insertion |
| 3 | JSON `{"a":<VT>1}`, Python `x = (1<VT>+ 2)` | whitespace alphabets |
| 3 | TOML `" " = 1` against `"  " = 1`, and single-quoted | quoted keys are opaque |
| 3 | A space before a JavaScript hashbang | exact prefix |
| 3 | `const x = 1;\n--> c` against `const x = 1; --> c` | comment gaps are never wrap sites |
| 4 | `#[probe] fn f(x: u8,)` against `fn f(x: u8)` | commas are tokens |
| 4 | `f<string,>()` against `f<string>()`; `<T,>` against `<T>` in TSX | commas are tokens |
| 4 | A comment in JSON | comments refuse a JSON proof |
| 5 | Rust `probe!(1custom)` against `probe!(1 custom)`, and string, raw-string and char suffixes | touching is kept by default |
| 5 | TypeScript `1as number`, JavaScript `1in obj` against the spaced forms | touching is kept by default |
| 5 | Python `x=1\` at the end of the file, with and without a further line break | exact rest after an unclassified item |
| 5 | Go `//line x:notanumber` at the start of a line against one space before it | exact gap before a directive |
| 6 | Go `a< -b`, `a+ +b`, `a- -b`, `a/ *b` against the touching forms | touching kept next to punctuation |
| 6 | JavaScript `(a< !--b)` against `(a<!--b)` | touching kept next to punctuation |
| 6 | Rust `m!(0x1 .0)` against `m!(0x1.0)`, `m!(foo #bar)` against `m!(foo#bar)` | touching kept next to punctuation |

#### Languages

MVP languages: Rust, Python, JavaScript, TypeScript and TSX, Go, JSON and TOML. Grammar versions
are pinned to the ones Zed uses so the engine can link into the Zed fork. Like Zed, JavaScript is
parsed with the TSX grammar. Zed ships no TOML grammar (TOML is an extension there), so
`tree-sitter-toml-ng` has no Zed pin; it has no `links` key and shares Zed's
`tree-sitter-language`. Every language above has a justified wrap-site list, so none is dropped;
TypeScript's list rests on its parser rather than a specification and is the first to drop if
review finds a construct it cannot justify. Unsupported languages and parse errors are never
formatting-only; lines whose only difference is whitespace may be dimmed there, but they stay
visible and unreviewed.

### Move detection

Moves are a presentation over review units, not units of their own:

- Across all files of a revision, removed and added line runs are matched by content, at least 3
  lines and 20 tokens long. A plain move requires exact text in the MVP. Whitespace-trimmed
  matching may only *propose* a moved with edits pairing, whose differences, including indentation,
  stay visible. Recognizing re-indented moves as plain moves waits for the per-chunk formatting
  proof.
- Assignments are one-to-one and non-overlapping: each removed run pairs with at most one added
  run. A block copied to two places shows one move and one plain addition.
- **Moved**: both sides equal after normalization. Shown as one collapsed line, "moved from
  `old.rs:120-160`" at the destination and "moved to `new.rs:40-80`" at the source. Acknowledging
  it marks only the units the move covers completely, on both sides; a unit with changes outside
  the move stays unreviewed until those are reviewed too.
- **Moved with edits**: at least 80 % of the lines equal. Shown expanded by default with the inner
  diff between the two blocks; never part of a noise category.
- Any part of a unit not covered by a move stays visible as an ordinary change.

### Noise classification

Generated and vendored files are collapsed by default: `linguist-generated` or `-diff` in
`.gitattributes`, known lockfiles (`Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`,
`yarn.lock`, `go.sum`, `poetry.lock`, `uv.lock`), snapshots (`*.snap`), and files whose first lines
carry `@generated` or `DO NOT EDIT`. Collapsed units still need a mark; the UI offers one action to
mark a whole category, and every category stays visible as a count.

### Carrying state across revisions

Review state never moves by content hash alone. For each pair of consecutive revisions the engine
computes a correspondence between their units:

1. **Map files**: renames between the two heads and between the two bases, from
   `git diff-tree -M`. A rename is ambiguous when another file has equal similarity for the same
   source or destination; units in ambiguously mapped files have no unchanged correspondence.
2. **Map positions**: line maps from a line diff of each file's old head against its new head, and
   old base against new base. A line diff picks one alignment among equally good ones, so a
   position is trusted only when the unit is distinguishable: its change appears once in the file
   on both revisions, or its surrounding context lines match too.
3. **Candidates**: a unit of the newer revision corresponds to a unit of the older one when the
   older unit's position maps onto it, on the head side for added lines and on the base side for
   removed lines. A deletion-only chunk maps through its insertion point.
4. **Confirm**: the correspondence is **unchanged** when both sides of the change are equal
   (exactly in the MVP; formatting normalization waits for the per-chunk proof), both footprints
   map exactly on the head and the base side, and the unit is distinguishable; **changed** when the
   change differs. An equal change whose position is not trusted is reviewed again.
5. **Ambiguity reopens**: a unit with several candidate predecessors (merges), a predecessor with
   several candidate successors (splits), or no positional candidate at all has no unchanged
   correspondence. Content equality without positional correspondence, such as an identical edit
   appearing elsewhere in the file, never carries a mark.

Correspondences chain across revisions, so each unit knows its **last reviewed predecessor**: the
nearest earlier unit in its chain with a mark, even if intermediate revisions were never reviewed.
A unit then is:

- **reviewed** when every link back to that marked predecessor is unchanged and no explicit
  `unseen` mark lies on the chain;
- **changed since reviewed** otherwise, shown with an interdiff of the two changes: both the removed
  and the added sides, with base-side differences (the target branch moved) distinguished from
  head-side differences (the PR changed);
- **new** when it has no marked predecessor.

Notes follow the same chains. A note whose chain ends, or splits ambiguously, attaches to the file
and is flagged as outdated, keeping its original revision and text as provenance.

### Walkthroughs

A walkthrough is JSON written for one revision:

```json
{
  "version": 1,
  "review": "github.com/owner/repo#123",
  "revision": 3,
  "title": "Add token-based move detection",
  "steps": [
    {
      "title": "New tokenizer",
      "why": "Everything else builds on the token stream",
      "units": ["u3-0007", "u3-0012"]
    }
  ]
}
```

- Unit IDs are revision-local. Import rejects a walkthrough whose review or revision does not match,
  and unknown or duplicate IDs.
- For every revision displayed, the engine derives a fresh partition: each current unit sits in
  exactly one step. Units inherit the step of their corresponding predecessor; stale references are
  dropped with a warning; units without a predecessor in the walkthrough go to a "New since
  walkthrough" step; formatting-only, moved and noise units left unassigned go to a "Noise" step,
  and any other unassigned unit to "Other changes" with a warning.
- Each step renders only its own units; a neighboring unit of another step appears as context, not
  as part of the step.
- `cr walkthrough prompt` prints a prompt with a compact unit catalog (ID, path, line range, short
  excerpt) for an agent to answer; `cr walkthrough import` validates and stores the result.
- Without an imported walkthrough, a deterministic default orders files by directory, puts tests
  after the code they test, and puts noise last.

### Storage

Per review, under `$XDG_DATA_HOME/code-review/reviews/<review-key>/`:

- `events.jsonl`: an append-only event log of revisions observed, marks, notes and walkthrough
  imports.
- `analysis/<base>-<head>.json`: the analysis of a revision (units, evidence, classifications,
  moves), content-addressed by its snapshot IDs, written atomically (temporary file and rename) and
  recomputable from git.
- `review.lock`: serializes state transitions (held exclusively) and log reads (held shared).
- `view/<front-end>.json`: each front end's own view state, such as review-lite's folded folders,
  through `Review::view_state` and `Review::save_view_state`. It is how the review was last shown,
  not review state: written atomically without the lock, never in the event log, and a front end
  that cannot read it starts afresh.

Rules:

- **State transitions are serialized as a whole.** Recording a revision takes `review.lock`,
  rereads the log, compares the snapshot pair with the current revision's, pins both snapshots,
  publishes the analysis, appends the event with the next number, and only then releases the lock.
  The appended event is the commit point: a revision number exists only once its event is in the log, so a crash before
  that leaves nothing to clean up but idempotent pins and a content-addressed analysis file.
  Background work carries the snapshot IDs it was started for, so a stale result can never replace
  a newer one.
- **Appends are complete or absent.** A writer holds the lock across `write_all` of one
  newline-terminated record and `sync_data` before reporting success. On opening the log under the
  lock, a trailing record without a newline (a writer that died) is truncated, because it was never
  acknowledged.
- **Readers tail safely.** A reader holds a shared lock on `review.lock` for each poll, so it never
  reads while a writer truncates or appends: a read racing recovery could otherwise stitch a dead
  writer's partial record to the record that replaces it. A reader advances its offset only past
  complete records and never keeps the bytes of an incomplete trailing line: it rereads from its
  last complete-record boundary every time, because recovery may truncate that tail and another
  writer may append a different record at the same offset. A corrupt record in the interior of the
  log is a hard error, never skipped.

An event log keeps the state human-readable, diffable and easy to sync, and avoids linking SQLite,
which would conflict with Zed's own `libsqlite3-sys`.

### Live updates

For PR reviews, poll every 30 seconds (configurable): `git ls-remote` for the PR head and the target
branch, and `gh pr view` for the target branch name, since a PR can be retargeted. A change of
either side fetches into a temporary ref, verifies the fetched objects, records a revision and
analyzes it in the background. The UI shows a banner such as "New push: 3 new and 2 changed
units" and applies it on request, keeping the cursor on the current unit.

### Code browsing at a revision

Each revision being browsed gets its own detached worktree at its head snapshot, created lazily
under the review's data directory and shared by every client viewing that revision. A client
switching revisions switches its project to the other worktree; worktrees no client uses are
reclaimed. Worktrees are snapshots: the front ends open them read-only, so an editor save cannot
change what is being reviewed.

## Workspace layout

| Path | Crate | License | Purpose |
|---|---|---|---|
| `crates/review-core` | `review-core` | MIT OR Apache-2.0 | Engine: git layer, snapshots, units, formatting proof, moves, noise, correspondence, walkthroughs, event store, export. Synchronous API; callers run it on their own background threads. |
| `crates/review-cli` | `review-cli` (binary `cr`) | MIT OR Apache-2.0 | CLI over the engine, with `--json` output for agents. |
| `crates/review-lite` | `review-lite` | GPL-3.0-or-later | The standalone gpui + gpui-component app. GPL because it may copy Zed code. |
| Zed fork (separate repo) | `code_review` | GPL-3.0-or-later | Review crate inside a Zed fork, depending on `review-core` by path. |

`review-core` must stay embeddable in Zed: no async runtime, no SQLite, no git library, and only
small, common dependencies (imara-diff, serde, serde_json, thiserror, a hash crate, and tree-sitter
with grammar versions matching Zed's).

## CLI (MVP)

| Command | Purpose |
|---|---|
| `cr open [pr-number \| pr-url \| base..head \| --worktree [ref]]` | Start or refresh a review; records a revision if anything changed and prints a summary. Without a target, see below. |
| `cr status` | Counts per file and in total: new, changed since reviewed, reviewed, accepted, flagged, noise. |
| `cr show [--unreviewed] [--step N] [--file PATH]` | Colored text rendering with moves collapsed and formatting-only units hidden behind counts. |
| `cr mark <unit>... seen \| accepted \| flagged \| unseen` | Set review state. |
| `cr note <unit> <text>` | Add a note. |
| `cr export [--format md \| json]` | Numbered list of flagged and annotated units with `file:line`, unit ID and excerpt, for an agent. |
| `cr walkthrough prompt \| import <file> \| show` | Agent-generated walkthroughs. |
| `cr watch` | Poll for new revisions and print what changed. |

Every command accepts `--review <key>`, defaulting to the review for the current repository and
branch, and `--json`.

`cr open` and `review-lite` without a target and without `--worktree` choose one in
`review-core` (`Review::open_for_current_branch`), and say on standard error which rule fired
(review-lite also in its status line when the review opens, after any configuration problem):

1. The review last opened for this repository and branch.
2. Otherwise the branch's open pull request, as `gh pr view` resolves it, keyed exactly as when it
   is opened by number or URL so marks are shared.
   A merged or closed pull request is skipped, because its review would no longer follow the
   branch; reviewing one stays possible by number or URL.
3. Otherwise `<default branch>..<branch>` (`..HEAD` when detached), where the default branch is
   what `origin/HEAD` names, or else the first of `origin/main`, `origin/master`, `main` and
   `master` that exists.
   Any `gh` failure (not installed, not authenticated, no GitHub remote, no pull request) only
   skips rule 2 and is reported as the reason.
   With no default branch, or no commit on `HEAD` beyond it, opening fails and asks for a target
   or `--worktree`.

A remembered review always wins, so a pull request opened after a range review of the same branch
is picked up only by naming it once.

## Front ends

Both front ends first get the same **thin slice**, which tests the risky integration seams before
either becomes a full product:

- Open a review by PR number, PR URL or range; show the revision and its progress.
- Sidebar with walkthrough steps, each with progress.
- Main view: the current step's hunks, virtualized; moved and formatting-only units collapsed to
  one line with a toggle; changed-since-reviewed units show their interdiff.
- Keyboard: next and previous unit, next and previous step, mark seen and advance, accept, flag.
- Scroll a 50,000-line diff smoothly.
- Go to definition from a hunk into the revision's worktree.

Notes, the new-push banner, find-references and the rest of the feature set follow once one front
end is chosen.

### lite

#### Product direction

review-lite works like lazygit, with approving where lazygit stages. The reviewer moves through
files, opens one file's diff, steps from hunk to hunk and approves each with one key; a whole
file or folder is approved from the tree the same way. The look is a bold terminal editor's, but
calm: code is an uninterrupted grid, and everything the tool says about the code sits apart from
it, muted, so it never reads as code.

- **Modes.** Three modes, each its own key context and focus handle, and one at a time:
  - *Files* (the left pane): `j`/`k` move and preview each file's diff, `h`/`l` fold and unfold
    folders, Enter folds a folder or opens a file, `0` opens the diff, Space approves or
    unapproves the file or folder, `!` flags it and `u` resets it. `/` filters the tree as
    lazygit does: typing narrows it to the files whose path contains the query (and their
    folders), Enter keeps the filter and returns to the list, Esc clears it. A filtered folder
    cannot be marked as a whole, since it hides some of its files. Folded folders are saved with
    the review whenever they change and fold again when it reopens, except folders the shown
    revision no longer has; the review opens at the first file that needs the reviewer outside
    them, and unfolds one only when every such file is folded away.
  - *Diff*: a file opens at its first hunk that needs the reviewer. `j`/`k` move between hunks,
    `Ctrl+D`/`Ctrl+U` scroll, Space approves the hunk and moves to the next open one (and, at the
    end of a file, names the next file, which `n` opens), `!` flags, `za` folds, Esc or `h` goes
    back to the files. `/` searches the diff's code as vim does, without leaving the diff:
    matches are highlighted, `n`/`N` step through them (while a search is active `n` steps
    rather than opening the next file), and the selected hunk follows the match, so Space acts
    on the hunk that holds it. Leaving the diff ends the search.
  - *File* (`=`): the whole head version as a read-only buffer at the selected hunk, with the
    changed lines marked in the gutter and syntax everywhere; vim motions (`j`/`k`, `gg`, `G`,
    `Ctrl+D`/`Ctrl+U`, `w`/`b`), `/` with `n`/`N`, `gd` when trusted, and Space approves the hunk
    under the cursor. `=` or Esc goes back to the diff at the same place.
  - `+` widens the diff across the window and `_` brings the split back; `i` opens the pull
    request overview and `?` the keys, both overlays that own the keyboard until closed.
  - `s` shows the diff side by side or unified, `zA` shows or folds approved hunks, `|` cycles
    the diff styles, and `T` or `,` opens the settings list (below).
- **Pickers.** Telescope's two most used, from every mode, as overlays that own the keyboard
  and the pointer: `Ctrl+P` finds a file by a fuzzy match of its path (`nucleo-matcher`, the
  matcher Helix uses), each entry with its change kind and review progress; `Ctrl+G` searches
  file contents as you type, each hit as `path:line` with the line and its match highlighted,
  beside a preview of the lines around it. Both start on the review's changed files; Tab widens
  them to the whole head tree. The query takes typing; Esc gives the list the keys (`j`/`k`,
  `i` or `/` to type again, Esc or `q` closes), as in vim. Enter opens a changed file in the
  diff, at the hunk holding a content hit or, when no hunk shows that line, in the file buffer
  at it; a file outside the review opens read-only in the buffer. Changed files are searched in
  the head texts the window already holds for the diff, which are exactly the text the diff and
  the buffer show, so a hit's line is the line that opens and typing starts no process; the
  whole tree is not in memory, so it is searched with `git grep` on the pinned head snapshot and
  listed with `git ls-tree`, never the worktree. Searches run in the background, a newer query
  cancels the older one (killing its `git grep`), and only the newest result lands. The picker
  is a list, a query and an optional preview, meant to host other searchable lists later.
- **Holding a key.** Moving the tree cursor previews each file's diff. A single press builds it
  at once; while a held key repeats, only the file's header follows the cursor and the diff is
  built once the cursor rests (120 ms), so the cursor and header move every keypress. No
  older file's rows ever show under a newer header, and any mark, mode change or buffer
  completes the preview first, so a key pressed right after a burst acts on the file shown.
  Anything that would rebuild the diff meanwhile (a mark landing, text arriving) keeps the
  header alone, so the file still opens at its place once the cursor rests. Layouts are not
  cached: a layout reads text, highlights and folds that change on their own,
  and building one only when the cursor rests is fast enough.
- **Keys act on what shows.** Every mark resolves its target from the screen in one place:
  in the tree, the file under the cursor must be the file whose diff shows; in the diff, the
  rows must be the selected file's and the selected hunk must be on screen (one scrolled away
  is shown first, and the next press marks it); in the buffer, only a buffer of the selected
  file has hunks. Every way a file comes to show (the tree, `n`, following a move, a picker)
  goes through one function that also reveals it in the tree, clearing a filter that hides it
  and unfolding its folders. A diff search selects the hunk that holds its match, or the
  nearest one for unchanged code.
- **Actions as data.** `app/keymap.rs` is one table of actions: their mode, default keys, group,
  help and hint priority. Dispatch, the bottom bar, the `?` sheet and `[keys]` in the
  configuration all read it. The bottom bar shows what the keys act on (`hunk 2/3 ·
  src/engine.rs`) at the left and every hinted key of the current mode at the right, in rank
  order with `? keys` last at the edge. Keys that cannot act now are dimmed, not removed, and a
  narrow window drops whole hints from the highest rank down, so the bar never shifts as the
  selection changes.
- **Visual grammar.** Code rows keep one gutter: line number, sign, code. Our own rows sit on a
  separate band without a gutter, in muted text: a hunk header is one quiet line with the review
  state's glyph, the scope breadcrumb first with its innermost scope brightest (what the hunk
  changes), at most one tag (`only reformatting`, `whitespace changed · not proven equivalent`,
  `changed since revision 1`), and the lines it covers only when its code is folded away or no
  scope names it, since the gutter numbers the shown lines (unit IDs stay out; `y i` copies
  one); unchanged lines left out are one rule (`15 unchanged lines`, Enter shows more); a note is
  a bounded block with an accent bar; every hunk header is a gray band and every hunk has a gray bar
  along its left edge, both in the accent's blue for the selected hunk, so each hunk's start and
  extent show at a glance. Approved hunks fold to
  their header, and unchanged lines between folded hunks or between one and the file's edge
  show nothing, not even a gap rule or a blank row, so a run of folded hunks reads as a list of
  headers; `zA` (`[diff] approved = "folded"` or `"shown"`) shows approved hunks like any other,
  and fully reviewed files then stay open too. Formatting-only hunks still show, on a neutral background without red and green,
  labeled `only reformatting`; an unproven whitespace change keeps red and green and reads
  `whitespace changed · not proven equivalent`. Tree rows are an indent, a change-kind letter
  colored by kind, and the name colored by review state; only state that needs action or confirms
  done carries a glyph (`✓` done, `◐` partly reviewed, `!` flagged, `↻` changed since reviewed), so
  unreviewed files are just bright names. Change details and counts live in the diff's header. Moves
  are one quiet line naming the other end (Enter follows it); a move with edits also shows its edits
  where the code arrives. The file buffer's header names the scopes around the cursor
  (`src/engine.rs › impl Ledger › fn balance`). On tinted lines, syntax colors too dim to read are
  lifted toward the text color until they reach a 3:1 contrast.
- **Diff styles.** How a diff is colored is independent of the theme: a diff style is a named
  bundle of syntax colors (on or off), what changed lines are drawn on (`none`, `muted`, mixed
  toward the editor background, or `bold`), hunk headers (`bands` across the width or small
  muted `chips`) and a change bar in the gutter. A style is colors only: it never moves a row,
  so cycling styles never shifts the content. Three are built in, in the order `|` cycles them:
  `plain`, lazygit's red and green text without syntax or backgrounds; `bold` (the default),
  delta's syntax colors on its line backgrounds (`#5f1519`, `#14471f`, overridable per theme);
  and `soft`, the Zed prototype's muted look with chips and a change bar. `[styles.<name>]`
  tables add more (one that sets `density` is rejected with an error saying where it belongs),
  `[diff] style` remembers the choice (the older `syntax` reads as `bold`), and the bar's `|`
  hint names the style in use. Changed-token emphasis is off by default
  (`[diff] token_emphasis`).
- **Density.** The rows' spacing is its own setting, `[diff] density`: `compact`, a terminal's
  line height with each hunk right after the code before it; `normal` (the default), a little
  more line height and a blank row before each hunk; `comfortable`, Zed's taller lines with the
  same blank rows. Every row keeps one height, and the air is whole blank rows, so places and
  scrolling stay row arithmetic.
- **Side by side.** `s` toggles `[diff] layout` between `unified` and `side-by-side`. Pairing is a
  projection after the rows are laid out (`rows::present`): a run of removed lines beside the
  added lines right after it in the same unit and hunk, line by line, with the shorter side
  padded; unchanged lines on both sides with each side's own line number. A nested diff pairs
  the same way within its own block and never with the chunk's lines: a move's edits show the
  source's lines numbered as in the source file on the left and the moved lines numbered as in
  the target on the right, and an interdiff the reviewed version on the left and the current
  one on the right. Every line of code shows exactly once, in order on its side; notes, gaps,
  headers and move rows keep their rows across the width, and every row keeps its owners. Each
  half is a fixed viewport, and one text offset moves the code of every row while gutters, the
  middle divider and the tool's own rows stay put, so a hunk's header and a move's link never
  pan out of view; another file starts back at the left edge. When a half
  would show fewer than 40 cells, the diff shows unified and says so, without changing the
  remembered layout. A selection keeps the half it started in and copies that side's lines,
  skipping padding but keeping blank lines. `/` searches each half apart, the base's half before
  the head's in each row, and highlights a match only in its own half.
- **Settings.** `T` or `,` opens a keyboard-first settings list in the files pane's place (over the
  left of the diff when the files pane is hidden or narrower than 260 px), so the diff keeps its
  exact geometry: themes, diff styles, density, layout, token emphasis, approved hunks (folded or
  shown) and font size, with a search over every entry and the cursor's detail in a footer. The list
  and its search field are two key contexts (Tab or a click into the field switches; the field takes
  everything typed). Every entry under the cursor previews over what is applied, through the
  presentation transition, always from where the lists were when the list opened, so a preview that
  had to move a list never moves the place for good; moving off it or closing the list drops the
  preview, Space or Enter applies and writes it to the configuration, and a failed write says the
  look was applied but not saved. Measured on the 60,000-line fixture in a release build, a preview
  that rebuilds the rows takes under 4 ms, so previews need no debounce. The panel keeps the text
  size and row height it opened with, so previewing a density or a font size never moves the panel.
  Opening the settings closes a picker and opening a picker closes the settings, so one overlay owns
  the keys. There are no presets bundling several settings: the individual settings reach every look
  a preset did and say plainly what each one changes, so presets were removed; an older
  configuration's `[presets]` is ignored with one note. A mistake in one `[styles.<name>]` table
  costs only that style; any configuration problem shows in the status line when the review opens,
  not only on stderr. The configuration is edited as TOML (`toml_edit`), keeping comments and
  everything it does not change.
- **Presentation changes.** The layout, the density, the font size, the narrow fallback and
  whether approved hunks fold, and previews of them, go through one transition that notes where
  every list is by row identity, stops scroll animations, drops a selection whose rows changed,
  projects the rows again and puts every list back at its row. Showing or folding approved hunks
  lays the rows out anew, so the diff finds its place by what its top row showed: the same line,
  or else its unit's header. Places kept for other files are row identities too, so a
  density change never lands a returning reviewer on another line.
- **Themes as data.** A theme is a TOML file of semantic roles (`[ui]`, `[review]`, `[kind]`,
  `[diff]`, `[syntax]`). Nine are built in under `crates/review-lite/themes/`, each citing its
  source palette and license: carbonfox (the default), nightfox, nordfox, terafox, duskfox,
  tokyonight-night, One Dark, Catppuccin Mocha and Rosé Pine. Files in
  `$XDG_CONFIG_HOME/code-review/themes/` add themes or replace a built-in one of the same name.
  `[theme] name` picks one, and the settings list previews and applies them. The app is dark
  only and monospace throughout.
- **State model.** What is shown derives from a snapshot (one history, one pinned revision) and
  caches keyed by blob identity, so reloading never pairs new changes with old text. Every row
  names its hunk explicitly or none. A mark captures its exact units and revision when the
  reviewer acts and shows at once as pending; marks are saved one at a time in the background,
  and each completion rebuilds the view from the current selection, so a slow save never acts on
  what is shown now. The engine refuses a mark against a revision that is no longer current. A
  newer revision is announced in the status bar and never switched to by itself: `R` switches to
  it, or fetches when there is none. Language-server requests have deadlines.

#### Implementation

- **Stack.** `gpui-pre` 0.3.9 and `gpui-component` 0.8.0, both Apache-2.0. 0.9.0 and 0.8.1 are
  newer but still inside the repository's 14-day dependency cooldown; move to 0.9.0 (which uses
  `gpui-pre` 0.3.11) once it clears. `gpui-pre-platform` is built with the `wayland` feature only,
  so the Linux build links nothing from X11 and needs only `libxkbcommon-dev`.
- **One tree-sitter.** `gpui-component`'s tree-sitter highlighter is an optional feature and stays
  off, so the dependency graph has exactly one tree-sitter, the one `review-core` pins to Zed's
  revision, and no `links` conflict.
- **Syntax highlighting.** review-lite runs Zed's `highlights.scm` queries (copied under
  `crates/review-lite/queries/` with their source commit; TOML's comes with its grammar) on the
  grammars `review-core` exposes through `Language::grammar`. Each blob is highlighted once, in
  the background after the first frame, and cached by its object ID; added and unchanged lines
  take the head's colors and removed lines the base's.
- **Scope breadcrumbs.** `review_core::scope` finds named scopes (`impl Ledger › fn apply`,
  `class Parser › def parse`, `func (*Ledger) Apply`, `[package]`) for every supported language,
  and `RevisionView::scope` gives a unit's breadcrumb, read from the head, or from the base for a
  deletion-only chunk. Each hunk header shows it.
- **Rendering.** A `uniform_list` of single-line rows for the selected file, laid out from the
  engine's answers: collapse reasons, moves, interdiffs, display hunks and notes. Front ends never
  decide these themselves. `gpui-component`'s editor and its diff view are not used: a uniform
  list of prepared rows is the simplest thing that stays smooth at 50,000 lines. Walkthrough
  steps stay in the engine and the CLI; the app orders by files.
- **Go to definition.** A minimal synchronous stdio JSON-RPC client (`lsp.rs`) speaks
  `initialize`, `didOpen` and `textDocument/definition` to rust-analyzer (and, if installed,
  gopls, pyright and typescript-language-server) rooted at the revision's worktree, on gpui's
  background executor; a reader thread lets every request time out. Zed's `lsp` crate was the
  alternative, but it is async on Zed's executor and pulls in its settings, fs and http layers;
  one request type does not justify that. Build scripts, proc macros and `cargo check` stay off in
  rust-analyzer, but `cargo metadata` and `rustc` still run under the reviewed tree's Cargo
  configuration, which can name any program as a `rustc` wrapper. Running a language server on a
  reviewed head is therefore arbitrary code execution, and it is gated on trust (below). The
  definition opens in the file buffer; `Ctrl+O` goes back.
- **Trust.** No tool that can execute reviewed code starts unless the review is trusted.
  `Review::trust` and `Review::set_trusted` in `review-core` store the decision as a `trust` event
  in the review's log, so every front end shares it. Without a decision, the working tree and a
  range whose head is a local branch are trusted, and pull requests and other ranges, such as a
  remote-tracking ref or a bare commit, are not. A local branch can still hold someone else's
  code, for example one made by `gh pr checkout`; revoke trust for it. In review-lite, `gd` on an
  untrusted review shows a one-line hint, and `Ctrl+T` explains what will run and trusts on a
  second press, or revokes trust and stops the language servers. The demo's head is a local
  branch, so it is trusted.
- **Window chrome.** gpui asks for server-side decorations; GNOME has no `xdg-decoration`, so
  the window falls back to client decorations and review-lite draws its own square title bar
  (`app/chrome.rs`): the title, a drag area, minimize, maximize and close. As in Zed's
  `platform_title_bar` on Linux, a press arms a move and the first motion calls
  `start_window_move`; the armed state lives on the app, not in the frame's closures, because a
  re-render between press and motion would otherwise drop it. A double click toggles maximize and
  a right click opens the window menu. The QA sway forces client decorations so screenshots show
  the same bar.
- **File change kinds.** `FileStatus::change` (`FileChangeSummary`: git's kind, noise,
  formatting-only, moves, entry changes such as binary or mode) and its `FileTone` come from
  `review-core`. The tree shows the kind as a colored letter; the diff header spells out the
  details.
- **Layout.** Fixed row heights and fixed-width columns, so folding, marking and moving the
  cursor never shift anything else, and no pane animates its width; square corners throughout; a
  4 px grid with 12 px side padding; pane headers line up on one hairline. Nothing that holds
  text has a fixed pixel size: columns are sized in cells of the monospace font, whose advance is
  measured at startup (the line-number gutter is five code cells, the sign column one and a half,
  the tree's chevron, kind and state columns one and three quarters each), bars are one line of
  their text plus a grid step, and counts are padded to their widest, so nothing overlaps, clips
  or wraps at any font size from 9 to 40 px (zoom and `--font-size` clamp to that range). Where
  room runs out, long text truncates with an ellipsis (paths from their start) and whole
  lower-priority parts make way: the file header drops its line counts, then its meter. Only the
  title bar and its buttons keep the platform's fixed size, so previewing a font size never moves
  the settings panel. The files pane resizes by dragging its divider (double click resets, limits
  keep it at least 180 px and 16 text cells wide and the diff at least 400 px wide), `Ctrl+B`
  hides it, and the width is remembered in the `[layout]` table of the configuration. Under its
  header, two progress lines show the files and the hunks reviewed: a bar, done of total, and the
  whole percent, rounded down so 100% means all of it; in a narrow pane the bar makes way first,
  then the percent, then the label.
- **Scrolling.** `review-lite --bench-scroll` scrolls every row of every file of the review once,
  file after file, 30 rows per frame, and prints frame-interval and per-frame build-time
  percentiles. On the perf fixture (25 files, 90,000 rows) the app's work per frame is about
  1.4 ms at the median and 3.6 ms at p99 in a release build.

### Demo review

`review_core::demo` materializes a built-in, deterministic sample pull request from the plain files
under `crates/review-core/demo/` into `target/demo/repo`, with its review data in
`target/demo/data`: fixed commit IDs, three revisions (revision 2 is a force-push that carries,
changes and adds units; revision 3 returns one hunk to its revision-1 form), a walkthrough and a
few marks. It covers every feature the front ends render: formatting-only files, a re-wrap with a
real edit, a plain move and a move with edits, a rename with edits, mode, symlink and binary
changes, deletion-only chunks and noise. `--demo` rebuilds it and opens revision 1, and
`--demo-revision N` advances to revision N keeping marks, in `cr` and `review-lite` alike. It is
the shared fixture for both front ends; the Zed fork opens the same review.

### QA harness

`review-lite --qa` reads line commands on standard input (`key`, `action`, `click`, `drag`,
`hover`, `hit`, `file`, `find`, `scroll`, `resize`, `state`, `quit` and more; see `app/qa.rs`)
and answers each with one line once its background work is done and a frame has rendered;
`rapid <command>` answers without waiting, so a script can race input against background work.
`state` prints the visible UI state as JSON. `scripts/qa/run.sh` (`task qa --
<script>`) runs the app inside a headless sway, feeds it a script, checks answers with `expect`
lines and saves `grim` screenshots to `target/qa/`. `scripts/qa/smoke.qa` walks the demo. QA runs
keep their configuration and demo under `target/qa/`, so they never touch the user's settings or
race another front end's demo. The Zed fork reuses the runner with its own QA mode.

### zed

As built for M3 in `~/dev/zed-review` (remote `upstream`, Zed `main` at c7bea34383), crate
`crates/code_review`:

- **Patch to upstream:** five lines in four files: the workspace member and dependency in
  `Cargo.toml`, the dependency in `crates/zed/Cargo.toml`, `code_review::init(cx)` in `main.rs`,
  and `code_review::bind_keys(cx)` at the end of `load_default_keymap` in `zed.rs`. Zed reloads its
  keymap by clearing every binding, so the crate's own `keymap.json` must be re-bound from there;
  that line is also what lets the review keys win over vim's normal mode. `Cargo.lock` gains
  `code_review`, `review-core` and `tree-sitter-toml-ng`; there is one tree-sitter in the graph.
- **Opening:** command-palette actions (`code_review: open demo`, `open demo revision`,
  `review working tree`, `review range`) and, at startup, `CODE_REVIEW_DEMO=1`,
  `CODE_REVIEW_DEMO_REVISION=N`, `CODE_REVIEW_RANGE=a..b` or `CODE_REVIEW_WORKTREE=REF`, because
  Zed's CLI rejects unknown flags. `scripts/zed/launch.sh` wraps them with its own Zed profile
  (dark theme, terminal font, diff colors as theme overrides). The review's head worktree opens as
  a workspace in the first window; every buffer of it is made read-only.
- **Review item:** one `MultiBuffer` per step over the head worktree's project buffers, so
  highlighting, go-to-definition, search, excerpt expansion, sticky scroll and breadcrumbs are
  Zed's. Each file gets a `BufferDiff` whose base text is the head file with only the step's
  shown units reverted, so Zed renders exactly the step's own units as hunks and every other change
  as context. Header blocks carry the badge, collapsed units (formatting-only, plain moves, noise,
  exact moves with edits) are `Replace` blocks with a one-line summary that `enter` expands in
  place, and interdiffs, inner move diffs, descriptions and notes are blocks below. A PR overview
  with the description rendered by Zed's Markdown renderer sits above the step.
- **Panel:** Steps and Files tabs; the file tree comes from `review_core::tree` and marks whole
  files and directories through `Review::mark_files`.
- **State:** engine work runs on the background executor; a mark shows as a pending badge in the
  next frame and the reloaded snapshot replaces it without rebuilding the excerpts. Base texts are
  built off the main thread; the only main-thread work is Zed's excerpt and block update on a step
  change (a few milliseconds on the demo), and every engine call and layout pass logs its time.
- **Snapshot hygiene:** rust-analyzer's default `cargo check` on save writes a `target/` directory
  into the head worktree, which Zed then rescans on its main thread (multi-second hangs) and which
  runs the reviewed code. The launch profile turns off check on save, build scripts and proc
  macros, as review-lite does; the fork should enforce this per review worktree rather than rely
  on the profile.
- **Trust:** the engine's trust decision maps onto Zed's restricted mode (`TrustedWorktrees`):
  trusted reviews trust the head worktree, untrusted ones restrict it, and trusting it in Zed's
  prompt records the trust in the engine.
- **QA:** `CODE_REVIEW_QA=1` speaks the same stdin protocol as review-lite's `--qa`;
  `scripts/qa/zed-smoke.qa` walks the demo, and `QA_BINARY=scripts/zed/launch.sh` drives the fork
  with `scripts/qa/session.sh`.

## Milestones

1. **M0: research and plan.** This file and [prior-work.md](prior-work.md).
2. **M1: engine and CLI.** `review-core` and `cr`, done when the acceptance tests below pass.
3. **M2: lite slice.** The thin slice on gpui + gpui-component; built, see [lite](#lite).
4. **M3: zed slice.** The same slice as a Zed fork crate.
5. **M4: compare and choose.** Measure both, then grow the chosen one to the full feature set.
6. **Later:** post comments and reviews to GitHub; import historical PR pushes from GitHub's
   force-push timeline events; an MCP server over the engine for agents; more languages; in-app
   walkthrough generation; risk hints per file; CI and lint results inline.

## Acceptance tests for M1

Each runs against temporary git repositories built in the test:

- Force-push carry-over: mark all units seen, amend one chunk, rebase onto a newer base and
  force-update the head; only the amended chunk is unreviewed, shown as changed since reviewed.
- Line shift: an unrelated edit above a reviewed chunk does not reopen it.
- Duplicate edit: an identical edit added earlier in the file is new, and the reviewed one stays
  reviewed.
- Skipped revision: reviewed at revision 1, unreviewed changes at 2, more at 3; revision 3 shows the
  interdiff against revision 1.
- Base-only change: the head stays, the target branch changes the removed side of a reviewed chunk;
  the chunk is changed since reviewed and the interdiff shows the removed side.
- Deletion-only chunks carry over and reopen like others.
- Ambiguity: two identical files with identical edits, renamed in a rebase, reopen; so does an edit
  repeated in a file whose context does not distinguish it.
- `unseen` on an inherited mark keeps the unit unreviewed in later revisions.
- Merge and split of chunks reopen, and notes keep their provenance.
- Re-wrapping a Rust call is formatting-only; `git diff -w` would show it.
- A file with a re-wrapped call and one real edit has no formatting-only chunk, and the real edit's
  tokens are highlighted.
- JavaScript `return isAdmin;` to `return\nisAdmin;` is not formatting-only.
- Re-indenting a Python block is not formatting-only, and neither is dedenting one statement out of
  an `if` block.
- A change inside a multi-line string literal is not formatting-only.
- An unsupported language is never formatting-only.
- A function moved to another file is one move; a block copied to two places is one move and one
  addition; a moved block with a one-line edit is expanded and shows that edit.
- A YAML block moved and re-nested is not collapsed as a plain move.
- Acknowledging a move that only partly covers a chunk leaves that chunk unreviewed.
- A mode-only change, an empty-file addition and a pure rename each produce a file unit; a mode
  change on a file with text changes adds a file unit next to its chunks.
- A renamed file keeps its reviewed chunks.
- Uncommitted work: two different captures are two revisions; the user's index is unchanged.
- Two processes recording a revision for different heads at once get distinct revision numbers and
  consistent pins.
- A writer killed mid-append leaves a log that reopens with the partial record truncated; a reader
  stopped at any byte boundary loses no complete record, including when the partial record it saw is
  truncated and replaced by a different one.
- A crash after pinning but before the revision event leaves a state from which the next attempt
  succeeds.
- A walkthrough survives a new revision: units keep their steps, new units go to "New since
  walkthrough", and stale references are dropped.
- Lockfiles and `@generated` files are classified as noise and still need a mark.
- A 50,000-line diff is analyzed in under a second in release mode.

## Choosing between the front ends (M4)

Compare on: clean and incremental build time, binary size, startup time, scrolling a 50,000-line
diff, LSP quality, lines of code we own, upstream churn (lite: gpui and gpui-component releases;
zed: rebase conflicts per month), and how easy the walkthrough and marking UI were to build.

## Open questions

- How should stacked PRs be handled: one review per PR, or a review spanning the stack?
- Where should the Zed fork live, given that a GitHub fork of a public repository is public?
