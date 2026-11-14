# The demo pull request

`cr --demo`, `review-gui --demo` and `review-tui --demo` open a built-in pull request: a small
multi-language project whose branch is force-pushed twice. It is the shared fixture for manual
checks, the QA scripts, the screenshots and many tests. `crates/review-core/src/demo.rs` builds
it.

## The trees

Each directory under `trees/` is the full file tree of one commit:

| Tree     | Parent   | Commit                                                          |
| -------- | -------- | --------------------------------------------------------------- |
| `base-1` |          | the base branch when the pull request is opened                 |
| `head-1` | `base-1` | the pull request's first version                                |
| `base-2` | `base-1` | the base branch after an unrelated change landed                |
| `head-2` | `base-2` | the pull request rebased onto `base-2`, with a new change on it |
| `head-3` | `base-2` | the pull request pushed again, taking part of that change back  |

The pull request has three revisions, one per force-push, each a pair of trees that
`manifest.json` lists under `revisions`: `base-1` and `head-1`, `base-2` and `head-2`, then
`base-2` and `head-3`. The demo repository's `main` branch points at a revision's base and its
`feature` branch at its head.

`--demo` opens revision 1, and `--demo-revision N` pushes the pull request forward to revision
N, keeping your marks. Between revisions, units carry over unchanged, change, appear, and
return to an earlier form, so each revision shows how review progress follows the code.

Snapshots duplicate the files a commit does not change, but each one reads and edits as plain
files. A file the pull request adds is therefore in every `head-*` tree. Edit its copies in
`head-2` and `head-3` only where those revisions should change it.

## The other files

- `manifest.json`: the commits, their parents and messages, the entries a plain file cannot
  express (executable modes, symlinks, binary content), and the revisions.
- `pr.json`: the pull request's number, title, body and branches.
- `walkthrough.json`: the walkthrough for revision 1, its units chosen by path and text.
- `marks.json`: marks and notes made on revision 1, so revision 2 has units that carry over.

## Building

The demo is built under `target/demo/` with `git fast-import`, using a fixed identity and fixed
dates, so its commit IDs are the same on every machine. `tests/acceptance/demo.rs` pins
`head-1`'s: a change to `base-1` or `head-1` changes it, and the failing test prints every ref
with its new ID.
