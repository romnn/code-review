# Plan

A fast, native desktop tool for reviewing large, often AI-written diffs and GitHub pull requests.
It remembers what you have already reviewed down to the hunk, across pushes and force-pushes,
walks you through a change in a sensible order, and keeps reformatting and moved code from
drowning the real edits. See [prior-work.md](prior-work.md) for the survey this plan builds on.

We build the review engine once and put two light front ends on it:

- **review-gui**: a standalone desktop app on `gpui` and `gpui-component`, copying the GPL Zed
  code it needs (highlighting queries and similar).
- **review-tui**: the same review in a terminal, with the same keys, configuration and behavior.

Both draw a model that knows no UI toolkit, `review-ui`. An earlier plan also built the review
into a fork of Zed and kept whichever front end proved easier to grow; the fork was dropped
because a whole editor is far too heavy to ship for this, and the two apps do the job as well.

No web stack and no JavaScript: everything is Rust.

## Goals

1. **Seen state per hunk that survives pushes.** After a push, rebase or force-push, only new or
   changed hunks are unreviewed; changed hunks show what changed since you last reviewed them.
2. **Format-independent diff.** Changes that provably only touch whitespace, indentation or line
   wrapping are classified as formatting-only and hidden behind a visible count; real edits inside
   reformatted code are highlighted. Proven formatting-only units count as approved automatically
   (see "Approving reformatting automatically").
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
engine's dependency tree small enough to embed in another application without version conflicts. The git layer
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
- A front end can add variables to the environment of every `git` command a review runs
  (`Review::set_command_env`); review-tui uses it to keep fetches from prompting on the terminal
  it draws on.
- Partial clones: analysis reads every blob it needs, which makes git fetch missing objects, and
  the analysis cache stores the review evidence (the text of each unit), so past reviews stay
  reproducible offline. Browsing an old revision needs its objects and may need the network.

### Snapshots of uncommitted work

To review an agent's uncommitted turn, capture the working tree without touching the user's index:
with a temporary `GIT_INDEX_FILE` holding a copy of the user's index (which keeps git's stat cache
and sparse-checkout entries, and the index's date, so git still rereads files as new as the index
whose stat cannot show an edit; `git read-tree HEAD` when there is no index), `git add -A` (tracked
and untracked files, respecting `.gitignore`), `git write-tree` and `git commit-tree` with a fixed
identity and date produce a synthetic snapshot commit, pinned like any other. Staged and unstaged changes are captured together. Two captures of
different contents are two revisions; a capture identical to the current revision deduplicates.
Whether a working tree has uncommitted changes is decided by the same capture: its tree against
`HEAD`'s, so the two can never disagree.

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

MVP languages: Rust, Python, JavaScript, TypeScript and TSX, Go, JSON and TOML, on the
crates.io releases of tree-sitter and its grammars. Like Zed, JavaScript is parsed with the TSX
grammar. Every language above has a justified wrap-site list, so none is dropped;
TypeScript's list rests on its parser rather than a specification and is the first to drop if
review finds a construct it cannot justify. Unsupported languages and parse errors are never
formatting-only; lines whose only difference is whitespace may be dimmed there, but they stay
visible and unreviewed.

### Approving reformatting automatically

A unit whose change is proven formatting-only, and that no mark on its chain decides, counts as
approved without a mark: `RevisionView::standing` derives a `Standing` (open, approved
automatically, reviewed with a verdict, or changed since reviewed) from the unit's state, its
proof and the view's `ReviewPolicy`. Counts gain `auto_approved`, and progress, folding,
next-open navigation, the tree's glyphs, walkthrough progress and `cr show --unreviewed` all read
the standing, so every front end and `cr` agree.

The policy is explicit, never an engine default. `ReviewPolicy` has no `Default`; a front end
that approves automatically builds its views with `History::view_under` (or
`current_view_under`) and its policy, while `History::view` and `current_view` decide by marks
alone (`ReviewPolicy::MARKS_ONLY`). An embedder that never opted in therefore counts every unmarked unit as open, matching the badges it draws from `UnitState`.
The apps and `cr` opt in by default and share one setting,
`[review] auto_approve_formatting` in `$XDG_CONFIG_HOME/code-review/config.toml`
(`review_core::store::default_config_file`, or `CODE_REVIEW_CONFIG`): the apps read the whole
file and offer the setting in the settings list, which saves it there; `cr` reads only the
`[review]` section and ignores the rest. `cr --no-auto-approve` turns it off whatever the file
says; a file `cr` cannot read decides by marks alone, with a warning.

This keeps the overriding safety rule:

- Nothing is written on the reviewer's behalf: the event log holds only the reviewer's own
  marks, and `cr export` lists only those.
- It is decided again for every revision from that revision's proof, never carried as a mark,
  so a unit that stops being formatting-only in a later revision needs review.
- Any mark on the unit's chain decides instead: a flag keeps it flagged, an `unseen` mark (`u`,
  or Space on an automatically approved hunk) makes it need review, and a change since a review
  shows that change. Only a unit with no mark at all is approved automatically.
- The proof is the same conservative one that labels the unit `reformatted only`;
  unsupported languages, parse errors and unproven whitespace changes (`whitespace changed ·
  not proven equivalent`) are never approved automatically.
- It shows apart from a human approval: the hunk's header says `(reformatted only ·
  auto-approved)`, its `✓` is quieter, the status line counts them (`13 of 27 hunks reviewed ·
  6 auto-approved`) and the hunks bar fills their share in a quieter color.

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
- `synced.json`: when the review last read its target's state and which revision was current
  then, written atomically by every successful refresh (`Review::synced`). Like the view state
  it is not review state: no lock, never in the event log, and the last writer of two
  concurrent refreshes wins, either being a true sync time. Writing it is best-effort: a
  refresh that recorded its revision succeeds even when the record cannot be stored, and
  `Recorded::sync_error` says why.
- `view/<front-end>.json`: each front end's own view state, through `Review::view_state` and
  `Review::save_view_state`, such as the folded folders. review-gui and review-tui share theirs
  under the name `review-lite`, review-gui's name when it first stored them, so folds made in
  one app show in the other and folds saved before the rename keep loading. It is how the review
  was last shown, not review state: written atomically without the lock, never in the event log,
  and a front end that cannot read it starts afresh.

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
which would conflict with an embedder's own `libsqlite3-sys`.

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
| `crates/review-cli` | `review-cli` (binary `cr`) | GPL-3.0-or-later | The one released binary ([ADR 1](adr/0001-cr-is-the-one-binary-and-runs-the-terminal-app.md)): the terminal app without a command, and commands over the engine with `--json` output for agents. GPL because it contains the terminal app. |
| `crates/review-ui` | `review-ui` | GPL-3.0-or-later | The apps' shared model, free of any UI toolkit: state and behavior, keymap and key matcher, configuration, themes, rows, frame geometry and paint decisions. GPL because its highlighting queries are copied from Zed. |
| `crates/review-gui` | `review-gui` | GPL-3.0-or-later | The standalone gpui + gpui-component app, a view over `review-ui`. |
| `crates/review-tui` | `review-tui` | GPL-3.0-or-later | The ratatui terminal app, a view over the same model; a library `cr` runs, and a `review-tui` binary for the QA scripts, which is not released. |

`review-core` stays embeddable in other applications, editors included: no async runtime, no
SQLite, no git library, and only small, common dependencies (imara-diff, serde, serde_json,
thiserror, a hash crate, and tree-sitter with its grammars).

## CLI (MVP)

| Command | Purpose |
|---|---|
| `cr [pr-number \| pr-url \| base..head \| --worktree [ref] \| --demo]` | The terminal app (see [review-tui](#review-tui)) on that review, or without one on the review chosen as below. |
| `cr open [pr-number \| pr-url \| base..head \| --worktree [ref]]` | Start or refresh a review; records a revision if anything changed and prints a summary. Without a target, see below. |
| `cr status` | Counts per file and in total: new, changed since reviewed, reviewed, accepted, flagged, approved automatically, noise. |
| `cr show [--unreviewed] [--step N] [--file PATH]` | Colored text rendering with moves collapsed and formatting-only units hidden behind counts, saying how many of them were approved automatically. |
| `cr mark <unit>... seen \| accepted \| flagged \| unseen` | Set review state. |
| `cr note <unit> <text>` | Add a note. |
| `cr export [--format md \| json]` | Numbered list of flagged and annotated units with `file:line`, unit ID and excerpt, for an agent. |
| `cr walkthrough prompt \| import <file> \| show` | Agent-generated walkthroughs. |
| `cr watch` | Poll for new revisions and print what changed. |

Every command accepts `--review <key>`, defaulting to the review for the current repository and
branch, `--json`, and `--no-auto-approve`, which counts proven formatting-only units as needing
review, whatever the shared configuration's `[review] auto_approve_formatting` says.

`cr` and `cr open`, review-gui and review-tui without a target and without `--worktree` choose one
([ADR 2](adr/0002-uncommitted-work-is-reviewed-against-the-default-branch.md)) in
`review-core` (`Review::open_for_current_branch`), and say on standard error which rule fired
(the apps also in their status line when the review opens, after any configuration problem):

1. The review last opened for this repository and branch.
2. Otherwise the branch's open pull request, as `gh pr view` resolves it, keyed exactly as when it
   is opened by number or URL so marks are shared.
   A merged or closed pull request is skipped, because its review would no longer follow the
   branch; reviewing one stays possible by number or URL.
3. Otherwise, on a branch whose working tree has uncommitted changes, the working tree against
   the default branch (`--worktree <default branch>`), whether or not the branch has commits yet,
   so uncommitted work is never left out.
   Uncommitted means what a capture records: staged and unstaged changes and untracked files that
   are not ignored, but not changes inside a submodule's checkout, which a capture cannot see.
   The key names the checkout and the default branch, not the branch, so work begun on the
   default branch and moved with `git checkout -b` stays one review; rule 1 then ties the new
   branch to it once it is opened there, and it keeps following the branch after commits.
   A detached `HEAD` never gets this rule, because a working-tree review is trusted and a
   detached commit may be someone else's code.
4. Otherwise `<default branch>..<branch>` (`..HEAD` when detached), where the default branch is
   what `origin/HEAD` names, or else the first of `origin/main`, `origin/master`, `main` and
   `master` that exists.
   Any `gh` failure (not installed, not authenticated, no GitHub remote, no pull request) only
   skips rule 2 and is reported as the reason.
   With no default branch, or neither uncommitted changes nor a commit on `HEAD` beyond it,
   opening fails and asks for a target or `--worktree`.

A remembered review always wins, so a pull request opened after a range or working-tree review of
the same branch is picked up only by naming it once.
Work committed in full on a new branch before any review is opened there gets a range review of
its own, whose units start unreviewed; the marks stay with the working-tree review.
A review key is never reopened for a different target (a range ending at a branch named
`WORKTREE` would otherwise share the working-tree key); opening one fails instead.

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

### review-gui

#### Product direction

This direction holds for review-gui and review-tui alike; review-tui departs from it only where a
terminal cannot follow (see [review-tui](#review-tui)).

review-gui works like lazygit, with approving where lazygit stages. The reviewer moves through
files, opens one file's diff, steps from hunk to hunk and approves each with one key; a whole
file or folder is approved from the tree the same way. The look is a bold terminal editor's, but
calm: code is an uninterrupted grid, and everything the tool says about the code sits apart from
it, muted, so it never reads as code.

- **Modes.** Three modes, each its own key area, and one at a time:
  - *Files* (the left pane): `j`/`k` move and preview each file's diff, `n`/`N` move to the
    next and previous file, past folders and a folded folder's files, `h`/`l` fold and unfold
    folders, Enter folds a folder or opens a file, Tab or `0` opens the diff, Space approves or
    unapproves the file or folder, `!` flags it and `u` resets it. `/` filters the tree as
    lazygit does: typing narrows it to the files whose path contains the query (and their
    folders), Enter keeps the filter and returns to the list, Esc clears it. A filtered folder
    cannot be marked as a whole, since it hides some of its files. Folded folders are saved with
    the review whenever they change and fold again when it reopens, except folders the shown
    revision no longer has; the review opens at the first file that needs the reviewer outside
    them, and unfolds one only when every such file is folded away.
  - *Diff*: a file opens at its first hunk that needs the reviewer. `j`/`k` move between hunks,
    `Ctrl+D`/`Ctrl+U` scroll, Space approves the hunk and moves to the next open one (and, at the
    end of a file, names the next file to review), `!` flags, `za` folds, Tab, Esc or `h` goes
    back to the files. `n`/`N` show the next and previous file in the tree's visible order, at
    its first hunk that needs the reviewer, as the tree's cursor moves: folders are no stops, a
    folded folder's files are skipped, and at either end it stays and says so. `/` searches the
    diff's code as vim does, without leaving the diff: matches are highlighted, `n`/`N` step
    through them (while a search is active they step rather than moving to another file), and
    the selected hunk follows the match, so Space acts on the hunk that holds it. Leaving the
    diff ends the search.
  - *File* (`=`): the whole head version as a read-only buffer at the selected hunk, with the
    changed lines marked in the gutter and syntax everywhere; vim motions (`j`/`k`, `gg`, `G`,
    `Ctrl+D`/`Ctrl+U`, `w`/`b`), `/` with `n`/`N`, `gd` when trusted, and Space approves the hunk
    under the cursor. `=` or Esc goes back to the diff at the same place; Tab goes to the files,
    as it does from the diff, since Tab switches between the files and the code.
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
- **Actions as data.** Every action is a variant of `review-ui`'s `Action` enum, and
  `review_ui::keymap` is one table of them: their areas, default keys, group, help and hint
  priority. The shared key matcher, the bottom bar, the `?` sheet and `[keys]` in the
  configuration all read it, so a key does the same in both apps. The bottom bar shows the
  review's freshness at the left and every hinted key of the current mode at the right, in rank
  order with `? keys` last at the edge. The left side says which revision shows and its head
  commit, a newer revision waiting, and when the review last synced (`rev 1 · 08990e0 · synced
  4m ago`); while a fetch runs it says what it does (`syncing…`, `analyzing rev 4…`, `carrying
  marks…`) with a spinner, and a failed fetch says so for five seconds before the last good
  sync shows again. The engine records each sync in `synced.json` (`Review::synced`) and reports
  a refresh's steps through `Review::refresh_reporting`; a minute timer keeps the time current
  and the spinner turns only while something runs. When room runs out the commit makes way
  first, then the time, then the newer revision. The file shown and the selected hunk are in
  the file's header, so the bar does not repeat them. Keys that cannot act now are dimmed, not removed, and a
  narrow window drops whole hints from the highest rank down, so the bar never shifts as the
  selection changes.
- **Visual grammar.** Code rows keep one gutter: line number, sign, code. Our own rows sit on a
  separate band without a gutter, in muted text: a hunk header is one quiet line with the review
  state's glyph, the scope breadcrumb first with its innermost scope brightest (what the hunk
  changes), the lines it covers (`L27–29`) only when its code is folded away or no scope names
  it, since the gutter numbers the shown lines (unit IDs stay out; `y i` copies one), and last
  at most one tag (`reformatted only`, `reformatted only · auto-approved`, `whitespace changed ·
  not proven equivalent`, `changed since revision 1`): the lines and the tag are asides, faint,
  the tag italic and in parentheses, four cells apart, on the header's own background, so
  nothing breaks the band as a chip would; unchanged lines left out are one rule (`15
  unchanged lines`, Enter shows more); a note is a block of italic yellow text with a yellow
  bar; every hunk has a gray bar along its left edge, and its header is a gray band that turns
  green once all of it is approved and red once some of it is flagged. The selected hunk's bar
  and header take the solid color of its state, blue while it still needs the reviewer, green
  once approved, red once flagged, so each hunk's start, extent and state show at a glance.
  Enter or `p` (as `za`) shows a folded hunk's code and folds it again. Approved hunks fold to
  their header, and unchanged lines between folded hunks or between one and the file's edge
  show nothing, not even a gap rule or a blank row, so a run of folded hunks reads as a list of
  headers; `zA` (`[diff] approved = "folded"` or `"shown"`) shows approved hunks like any other,
  and fully reviewed files then stay open too. Formatting-only hunks still show, on a neutral background without red and green,
  labeled `reformatted only`, and once approved automatically they fold like approved hunks;
  an unproven whitespace change keeps red and green and reads
  `whitespace changed · not proven equivalent`. The tree draws as lazygit's file tree does
  (`pkg/gui/presentation/files.go`): two cells of indent per depth, then one marker cell, a
  space and the name, so a folder's children start under its name; the marker is lazygit's
  `▼`/`▶` for an open or folded folder and the change-kind letter, colored by kind, for a file,
  and folder names have no trailing `/`, as lazygit's tree directories have none. Names are
  colored by review state, as lazygit colors staged and unstaged files: a done file or folder
  is green whole, its kind letter and `✓` too, a flagged one red, one changed since reviewed
  purple, and one that still needs the reviewer bright; only state that needs action or
  confirms done carries a glyph (`✓` done, `◐` partly reviewed, `!` flagged, `↻` changed since
  reviewed), so unreviewed files are just bright names. Change details and counts live in the diff's header. Moves
  are one quiet line naming the other end (Enter follows it); a move with edits also shows its edits
  where the code arrives. The file buffer's header names the scopes around the cursor
  (`src/engine.rs › impl Ledger › fn balance`). On tinted lines, syntax colors too dim to read are
  lifted toward the text color until they reach a 3:1 contrast.
- **Diff styles.** How a diff is colored is independent of the theme: a diff style is a named
  bundle of syntax colors (on or off), what changed lines are drawn on (`none`, `muted`, mixed
  toward the editor background, or `bold`), and whether a change bar marks changed lines in
  the gutter, which no built-in style has. A style is colors only: it never moves a row, so
  cycling styles never shifts the content. Three are built in, in the order `|` cycles them: `plain`, lazygit's red and green
  text without syntax or backgrounds; `bold` (the default), delta's syntax colors on its line
  backgrounds (`#5f1519`, `#14471f`, overridable per theme); and `soft`, the Zed prototype's
  muted look. `[styles.<name>]` tables add more (one that sets `density` is rejected with an
  error saying where it belongs, and one that sets the retired `headers` says it is gone),
  `[diff] style` remembers the choice (the older `syntax` reads as `bold`), and the bar's `|`
  hint names the style in use. Changed-token emphasis is off by default
  (`[diff] token_emphasis`).
- **Density.** The rows' spacing is its own setting, `[diff] density`: `compact`, a terminal's
  line height with each hunk and note right after the code before it; `normal` (the default),
  a little more line height, a blank row before each hunk and one above and below each note;
  `comfortable`, Zed's taller lines with the same blank rows. A terminal's lines cannot grow,
  so there `comfortable` also puts a blank row between each gap of unchanged lines and the hunk
  after it. Every row keeps one height, and the air is whole blank rows, so places and
  scrolling stay row arithmetic; a half-line gap around notes would need rows of their own
  heights.
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
  pan out of view; another file starts back at the left edge. One of the tool's own rows that
  runs past its room moves with the code only as far as it runs past (a hunk header's outer
  scopes make way first and count for nothing), and the pan reaches its end, so a long note,
  such as a submodule's `before` entry or a move's description, is never cut off unreachably.
  Both front ends move and mark such a row by one rule (`paint::annotation::note_pan`), from
  the same width (`Note::width`) that sets how far the code pans.
  Shift+←/→ (or `z H`/`z L`) pans the code of either layout, and a line or a row of the tool's
  own that runs past the right edge ends in a dim `…`, so a clipped line never reads as
  complete. When a half
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
  `[diff]`, `[syntax]`). Fourteen are built in under `crates/review-ui/themes/`, each citing its
  source palette and license: the dark carbonfox (the default), nightfox, nordfox, terafox,
  duskfox, tokyonight-night, One Dark, Catppuccin Mocha and Rosé Pine, and the light GitHub
  Light, One Light, Catppuccin Latte, Solarized Light and Rosé Pine Dawn. Light themes whose
  source defines no diff backgrounds derive them by blending the base a fifth of the way to red
  and green, as nightfox's light palettes do. Files in
  `$XDG_CONFIG_HOME/code-review/themes/` add themes or replace a built-in one of the same name.
  `[theme] name` picks one, and the settings list previews and applies them. `[ui] note`, the
  theme's yellow, colors notes: italic text and a bar on a band tinted toward it, so a note
  never reads as code; a theme file without it gets One Dark's yellow. Themes carry no notion
  of light or dark and the app does not follow the system appearance: the reviewer picks a
  theme, and review-gui keeps gpui-component's own widgets dark whichever it is. The app is
  monospace throughout.
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
  so the Linux build links nothing from X11 and needs only `libxkbcommon-dev` and
  `libfontconfig-dev`.
- **One tree-sitter.** `gpui-component`'s tree-sitter highlighter is an optional feature and stays
  off, so the dependency graph has exactly one tree-sitter, the one `review-core` uses, and no `links`
  conflict.
- **Syntax highlighting.** `review-ui` runs Zed's `highlights.scm` queries (copied under
  `crates/review-ui/queries/` with their source commit; TOML's comes with its grammar) on the
  grammars `review-core` exposes through `Language::grammar`. Each blob is highlighted once, in
  the background after the first frame, and cached by its object ID; added and unchanged lines
  take the head's colors and removed lines the base's.
- **Scope breadcrumbs.** `review_core::scope` finds named scopes (`impl Ledger › fn apply`,
  `class Parser › def parse`, `func (*Ledger) Apply`, `[package]`) for every supported language,
  and `RevisionView::scope` gives a unit's breadcrumb, read from the head, or from the base for a
  deletion-only chunk. Each hunk header shows it.
- **Rendering.** `review_ui::rows` lays out single-line rows for the selected file from the
  engine's answers: collapse reasons, moves, interdiffs, display hunks and notes. Front ends never
  decide these themselves. review-gui draws them in a `uniform_list`; `gpui-component`'s editor
  and its diff view are not used: a uniform list of prepared rows is the simplest thing that
  stays smooth at 50,000 lines. Walkthrough steps stay in the engine and the CLI; the apps order
  by files.
- **Go to definition.** A minimal synchronous stdio JSON-RPC client (`review_ui::lsp`) speaks
  `initialize`, `didOpen` and `textDocument/definition` to rust-analyzer (and, if installed,
  gopls, pyright and typescript-language-server) rooted at the revision's worktree, as a
  background job of the model; a reader thread lets every request time out. Zed's `lsp` crate was
  the alternative, but it is async on Zed's executor and pulls in its settings, fs and http
  layers; one request type does not justify that. Build scripts, proc macros and `cargo check` stay off in
  rust-analyzer, but `cargo metadata` and `rustc` still run under the reviewed tree's Cargo
  configuration, which can name any program as a `rustc` wrapper. Running a language server on a
  reviewed head is therefore arbitrary code execution, and it is gated on trust (below). The
  definition opens in the file buffer; `Ctrl+O` goes back. `[lsp] enabled = false` keeps every
  language server from starting, trusted or not, for reviewers who would rather not give
  rust-analyzer the memory; `g d` then says it is off.
- **Trust.** No tool that can execute reviewed code starts unless the review is trusted.
  `Review::trust` and `Review::set_trusted` in `review-core` store the decision as a `trust` event
  in the review's log, so every front end shares it. Without a decision, the working tree and a
  range whose head is a local branch are trusted, and pull requests and other ranges, such as a
  remote-tracking ref or a bare commit, are not. A local branch can still hold someone else's
  code, for example one made by `gh pr checkout`; revoke trust for it. In both apps, `gd` on an
  untrusted review shows a one-line hint, and `Ctrl+T` explains what will run and trusts on a
  second press, or revokes trust and stops the language servers. The demo's head is a local
  branch, so it is trusted.
- **Window chrome.** gpui asks for server-side decorations; GNOME has no `xdg-decoration`, so
  the window falls back to client decorations and review-gui draws its own square title bar
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
  the tree's indent two cells a depth and its marker one cell and a cell of space, as in a
  terminal, and its state column one and three quarters), bars are one line of
  their text plus a grid step, and counts are padded to their widest, so nothing overlaps, clips
  or wraps at any font size from 9 to 40 px (zoom and `--font-size` clamp to that range). Where
  room runs out, long text truncates with an ellipsis (paths from their start) and whole
  lower-priority parts make way: the file header drops its line counts, then its meter. Only the
  title bar and its buttons keep the platform's fixed size, so previewing a font size never moves
  the settings panel. The files pane resizes by dragging its divider (double click resets, limits
  keep it at least 180 px and 16 text cells wide and the diff at least 400 px wide), `Ctrl+B`
  hides it, and the width is remembered in the `[layout]` table of the configuration. The pane
  has no header, since the window's title names the review: at its top two progress lines show
  the files and the hunks reviewed, each a bar, done of total, and the whole percent, rounded
  down so 100% means all of it; in a narrow pane the bar makes way first, then the percent, then
  the label. While a filter narrows the tree, a line under them shows it and how many files it
  leaves (`/engine  3/17`).
- **Scrolling.** `review-gui --bench-scroll` scrolls every row of every file of the review once,
  file after file, 30 rows per frame, and prints frame-interval and per-frame build-time
  percentiles. On the perf fixture (25 files, 90,000 rows) the app's work per frame is about
  1.4 ms at the median and 3.6 ms at p99 in a release build.

### review-ui: the shared model

review-gui began as one gpui crate. To run the same review in a terminal without a second
implementation of its behavior, everything that is not drawing or platform input moved into
`review-ui`, which knows no UI toolkit. review-gui and review-tui are views over it: each maps the
model's state onto its surface and its input onto model calls, and runs the effects the model
queues. Anything the two apps share that is not engine logic belongs here; engine logic still
belongs in `review-core`, so that the engine stays usable on its own.

- **License.** `review-ui` is GPL-3.0-or-later because Zed's `highlights.scm` queries are copied
  into `crates/review-ui/queries/`. Both apps link it, and `cr` contains the terminal app, so
  they are GPL too; `review-core` stays MIT OR Apache-2.0.
- **Model.** `model::Model` is the whole review window apart from its drawing: the snapshot on
  screen, the selected file and hunk, the mode, marks waiting for the engine, folds, find,
  pickers, settings and the file buffer. It is shaped like model-view-update: input arrives as an
  `Action` (`Model::dispatch`) or a typed call, and a front end changes the model only through
  `Driver::update`, which runs the model's after-update step (saving folded folders when they
  changed, and quitting once nothing is left to save) so no change can skip it.
- **Quitting.** `Model::quit` queues the `Quit` effect only once the marks queued and in
  flight and the folded folders are saved, saying so on the status line meanwhile, and while it
  waits it refuses new marks with a note, so what it waits for only shrinks. Who asks decides
  what a failure does. The reviewer (`QuitKind::Interactive`: `Quit` or `CloseWindow` from a
  key, a window's close button or the compositor's close request) sees the status line, so a
  mark that fails to save stops the quitting and its failure stays in view. Nothing the
  reviewer marked is ever dropped: asking again while the app waits changes nothing, so a held
  `q` or a double click on the close button cannot lose an approval just made. Asking again
  after a failure stopped the quitting, which the status line offers, waits for the marks still
  queued as an unattended quit does, so however many marks fail, two quits leave. A shorter
  wait is not worth a mark lost to a key pressed twice, and a save is never cut short anyway,
  since it holds `review.lock` and appends to the event log. A new mark after a failure makes
  the next quit an ordinary one again. Nobody sees an unattended quit (`QuitKind::Unattended`:
  a terminal's signal, hang-up or lost input, or QA mode's input ending), so no failure stops
  it: every queued mark is still tried, the log keeps each failure, and the app leaves once
  they settle. QA mode's `quit` quits as the reviewer
  does, answering the command that made it quit first.
- **Text as it shows.** `paint::display` is the one definition of how a line of text shows on
  a grid of cells, which every width the model computes (pan extents, the buffer cursor's cell,
  the widest row) follows and both front ends draw: grapheme clusters in the cells
  `unicode-width` gives the whole cluster, as ratatui's buffer assumes; tabs as four spaces; C0
  controls and DEL in caret notation and C1 controls as `<9b>`; and the characters a renderer
  may draw as nothing (bidi controls, zero-width characters, variation selectors, tag
  characters, Hangul fillers, line and paragraph separators) as their code points, such as
  `<U+202E>`, so no code can hide or reorder itself (Trojan Source). An emoji keeps the joiners
  inside it and an emoji presentation selector (VS16) right after a pictograph or inside a
  keycap such as `1️⃣`; every other joiner and VS16, so `isAdmin` followed by a selector never
  looks like `isAdmin`, the tags of subdivision flags and ideographic variation selectors show
  their codes. The text presentation selector (VS15) always shows its code: all it can change
  is how wide an emoji draws, where `unicode-width` gives one cell and most terminals two. review-tui draws
  everything through it; review-gui draws code, notes, paths and pull-request text through it,
  mapping highlights and the mouse between the source's bytes and the shown text.
- **Effects instead of async.** The model never runs anything itself. It queues `Effect`s, which
  `Driver::update` hands back for the front end to run: `Spawn` a background job, `After` a
  delay, `Copy` and `CopyPrimary`, the window `Title`, `Fullscreen`, `Quit`, `Log` a diagnostic, and `ScrollSheet` for the overlays each front end lays out itself.
  review-gui runs jobs on gpui's executor, review-tui each on a thread of its own, and the
  model's tests run them in place. So the model needs no async runtime and no executor of either
  toolkit.
- **Completions.** A job (a `Send` closure) or a timer hands back a `Completion`: a typed
  outcome and the subject it was computed for, which the front end gives back to
  `Model::complete` on the UI thread. `spawn` and `after` take the subject, so no job or timer
  can start without naming it: a preview's number, a picker search's generation, a picker, the
  buffer and revision of a definition lookup (while the review stays trusted), or a revision.
  `Model::complete` first ends what marks the job as running, then checks the subject against
  what shows in one place, and only a current outcome reaches its handler, so no handler sees a
  stale one. An outcome that must land whatever changed says so by name, `Always` with its
  reason: the opening, a save of marks or folded folders, whose failure must reach the reviewer
  and which quitting waits for, a refresh, which only adds newer history, and blob texts and
  highlights, keyed by content.
- **Geometry and scrolling.** The model owns every list's offset, in rows. Each frame the front
  end measures a `frame::Geometry` once, plain data in its units (logical pixels or cells): the
  diff pane's and lists' widths, the narrower half of a side-by-side row, each list's height and
  the code size the lists were laid out at. It hands it to the layout phase (`Model::layout`),
  which resolves pending scroll requests against the real viewports; the front end mirrors the
  offsets into its own scrolling and turns wheel and scrollbar input into calls on the model.
  The model never asks the front end for anything: it works out its `Grid` (row height, cell
  width, half cells, viewports), the rooms and the pan extents from the last geometry and the
  `frame::Sizes` of a `Scale`, a pure function of the code size, density and settings text size
  that the front end hands it once (the rows, cells, gutters and the bars around each list).
  So a zoom or a font size's preview keeps the reviewer's place against the viewports the next
  frame will have: each viewport grows by as much as its bars shrink. gpui lays its lists out
  only after a frame is built, so review-gui measures the lists' bounds of the frame before and
  checks after gpui's layout whether they changed (a resize, a bar coming or going, a list's
  first layout); if so it lays the frame out once more. review-tui lays out its panes itself
  before the layout phase, so it never needs that second pass. What needs a list's width waits
  for it rather than working against none: the buffer's cursor, shown sideways before the buffer
  list's first layout, is revealed by the first layout phase that has its width. Keeping places
  as rows is what lets presentation changes, side by side and both front ends share one scroll
  arithmetic.
- **Keys.** `keymap` is the one table of actions and default keys, with `[keys]` overrides.
  `keys::Matcher` is a port of gpui-pre 0.3.9's keystroke grammar (`Keystroke::parse`), typed-key
  matching (`Keystroke::should_match`) and key dispatch over a context stack, so multi-key
  sequences, pending timeouts and precedence behave the same in a terminal as they did when
  review-gui bound its keys in gpui; review-gui's tests run gpui and the port over the same keys.
  `line_edit` gives a terminal's text fields the editing keys of gpui-component's input.
- **Drawing decisions.** `paint` decides what every piece shows and in which colors (palettes,
  hunk headers, gutters, bars, the tree, pickers, settings, the overview), with colors that keep
  their alpha; the front ends decide only placement. So the same review looks the same in both,
  and a terminal composites translucent layers itself (`paint::flatten`).
- **What a front end can do.** `front_end::FrontEnd` (`Window` or `Terminal`) states its
  `Capabilities`: a code size that can change, a window of its own and smooth scrolling. The
  model offers only what the front end can honor: an action it cannot (zoom, fullscreen and
  closing the window in a terminal) is left out of the keymap and does nothing when dispatched,
  and the settings list has no font sizes in a terminal.
- **Starting and QA.** `launch` holds the command line both apps take to name a review, loads
  the configuration and opens the review; `qa` writes QA mode's `state` answer from the model
  and asks the front end only for its geometry, so a QA script's expectations read the same in
  both apps apart from the numbers.
- **Shared configuration.** One configuration file serves both apps; each keeps only the files
  pane's width apart (`[layout] sidebar` in logical pixels, `tui_sidebar` in cells). Saved view
  state is shared under the name `review-lite` (see "Storage").

### review-tui

The terminal app, which `cr` runs without a command (and the unreleased `review-tui` binary the
QA scripts drive), draws `review-ui`'s model in a terminal and works as review-gui does: the same keys
and `[keys]` overrides, configuration file, marks, folded folders, themes, diff styles, pickers,
settings, find, file buffer, overview and `?` sheet, and mouse selection with copies through OSC
52, which also works over ssh.

- **Stack.** ratatui 0.30 with its crossterm 0.29 backend. ratatui was chosen over lower-level
  crates such as crossterm alone or termwiz because it already gives a cell buffer and diffs
  frames, so only changed cells reach the terminal; none of its widgets are used. Each frame is
  written cell by cell into ratatui's buffer from `paint`'s pieces, only the rows on screen,
  every cell with opaque colors composited as gpui blends them. Without 24-bit color
  (`COLORTERM`), cells get their nearest of xterm's 256 colors.
- **Terminal.** While the review shows, the terminal is in raw mode on its alternate screen,
  with the mouse, bracketed paste and, where supported, the kitty keyboard protocol's
  disambiguation; every frame is wrapped in synchronized output. Quitting, panics and a failed
  take-over give back exactly what was taken, ending synchronized output first. On Unix,
  signals come only from outside, since raw mode keeps the terminal's keys from sending any, so
  Ctrl+Z does nothing: `SIGHUP`, `SIGINT` and `SIGTERM` quit unattended (see "Quitting" under
  review-ui), once the marks queued and in flight are saved or have failed, and a second one
  exits at once. Its signal thread never writes to the terminal itself: it gives the terminal
  back and replays the log on a thread of its own and exits after at most half a second
  whether or not they finished, without flushing, so a stalled terminal cannot keep the
  process alive. The app has no suspend of its own: stopping a program from inside it freezes
  it for good when its process group is orphaned (as when a terminal, `tmux` or `ssh` starts it
  directly), and could stop it while a save holds `review.lock`. A stop from outside
  (`SIGTSTP` or `SIGSTOP`) is left to its default action and stops the app as it is; when it
  continues (`SIGCONT`), it takes the terminal again, raw mode included, and redraws it whole.
  A terminal that can no longer be drawn to, and input that ended, quit unattended too.
- **Where a terminal differs.** No zoom, font size, fullscreen or smooth scrolling, and no
  scrollbars. The files pane's width is its own `[layout] tui_sidebar`, in cells. A paste goes
  only into a text field taking typing, never in as keys. A terminal has no title bar the app
  can count on, so the files pane keeps a header naming the review: a pull request's number and
  head branch (`#128 feature/reconcile`), or the range (`main..feature`, `main..working tree`);
  in a narrow pane the branch is cut first and the number always shows, and while a filter
  narrows the tree it shows at the header's right with how many files it leaves
  (`/engine 3/17`), taking room from the branch.
- **Fetches.** The review opens before the terminal goes raw, so the first fetch can still ask
  for credentials. Later fetches (`R`) run non-interactively through `Review::set_command_env`,
  because the app owns the terminal: `GIT_TERMINAL_PROMPT=0`, no ssh askpass helper, and the
  ssh git would run (`GIT_SSH_COMMAND`, else `core.sshCommand`, else `GIT_SSH`, else `ssh`) in
  batch mode when git takes it for OpenSSH, by `GIT_SSH_VARIANT` or `ssh.variant`, else by its
  name, as git's `connect.c` decides. plink and other ssh programs are left as they are, since
  they take no `-o`; a program of another name, which git probes with `-G`, is left as it is too.
  The children still inherit the terminal: a program that opens `/dev/tty` itself, such as a
  credential helper or `gpg`'s pinentry through `GPG_TTY`, could still draw over the screen. Its
  environment is left alone because review-core can only add variables, not remove them, and
  language servers share their environment with review-gui.
- **Diagnostics.** Nothing may be written over the screen, so diagnostics go to
  `$XDG_STATE_HOME/code-review/review-tui.log` and are printed again on standard error once the
  terminal is back.

### Demo review

`review_core::demo` materializes a built-in, deterministic sample pull request from the plain files
under `crates/review-core/demo/` into `target/demo/repo`, with its review data in
`target/demo/data`: fixed commit IDs, three revisions (revision 2 is a force-push that carries,
changes and adds units; revision 3 returns one hunk to its revision-1 form), a walkthrough and a
few marks. It covers every feature the front ends render: formatting-only files, a re-wrap with a
real edit, a plain move and a move with edits, a rename with edits, mode, symlink and binary
changes, deletion-only chunks and noise. `--demo` rebuilds it and opens revision 1, and
`--demo-revision N` advances to revision N keeping marks, in `cr`, review-gui and review-tui
alike. It is the shared fixture for every front end.

### QA harness

`review-gui --qa` reads line commands on standard input (`key`, `action`, `click`, `drag`,
`hover`, `hit`, `file`, `find`, `scroll`, `pan`, `resize`, `state`, `quit` and more; see
`crates/review-gui/src/app/qa.rs`) and answers each with one line once its background work is
done and a frame has rendered; `rapid <command>` answers without waiting, so a script can race
input against background work. `state` prints the visible UI state as JSON, written by
`review_ui::qa` so both apps answer alike apart from their geometry's units.
`quit` quits the way `q` does, once what the reviewer did is saved, after answering.
`scripts/qa/run.sh` (`task qa -- <script>`) runs the app inside a headless sway, feeds it a
script, checks answers with `expect` lines, waits with `exited` for an app that quits by itself
(as after `key q`) and saves `grim` screenshots to `target/qa/`.
`review-tui --qa` speaks the same protocol (with `paste` for a bracketed paste, and positions in
cells; see `crates/review-tui/src/qa.rs`), drawing into ratatui's off-screen `TestBackend`
without sway; `QA_FRONT_END=tui` (`task qa:tui -- <script>`) makes the runner drive it, and `shot`
then has the app write its screen as text and as SGR-colored `.ansi` under `target/qa/tui/`.
An `include <script>` line runs another script in place: `scripts/qa/smoke.qa` and
`scripts/qa/smoke-tui.qa` walk the demo in each app and share `scripts/qa/smoke-common.qa`, the
checks that do not depend on geometry. QA runs keep their configuration and demo under
`target/qa/`, so they never touch the user's settings or race another front end's demo.

The documentation's screenshots come from the same harness: `task screenshots` runs
`scripts/screenshots/run.sh`, which plays `scripts/screenshots/scenes.qa` in both apps, once in
carbonfox and once in GitHub Light, and writes every scene to
`target/screenshots/all/{gui,tui}/<scene>.png` and `<scene>-light.png`. Only the images
README.md shows are committed: the script copies those into `docs/screenshots/`, reduced to a
palette with pngquant (half the size, with no difference visible even zoomed in) and recompressed
with oxipng, and deletes any other image there. review-gui is shot at a fixed 1600x1000 window at scale 2; review-tui's
`.ansi` screens at 150x44 become terminal windows through freeze's SVG and resvg, in JetBrains
Mono NL with the symbols it lacks copied in from DejaVu Sans Mono, both downloaded at pinned
versions, so the images do not depend on the fonts installed. The scenes leave out what reads
the clock, such as the overview's revision times, so a rerun without code changes reproduces
every image byte for byte. `task screenshots:gallery` renders one scene in every theme and diff
style under `target/screenshots/gallery/`, uncommitted, for judging themes side by side.

## Milestones

1. **M0: research and plan.** This file and [prior-work.md](prior-work.md).
2. **M1: engine and CLI.** `review-core` and `cr`, done when the acceptance tests below pass.
3. **M2: gpui slice.** The thin slice on gpui + gpui-component; built, see
   [review-gui](#review-gui). Its model since moved into `review-ui`, and review-tui draws it in
   a terminal (see [review-tui](#review-tui)).
4. **M3: zed slice.** The same slice as a Zed fork crate; built, then dropped as too heavy to
   ship.
5. **M4: grow the apps.** Grow review-gui and review-tui to the full feature set.
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

## Open questions

- How should stacked PRs be handled: one review per PR, or a review spanning the stack?
