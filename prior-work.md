# Prior work

Survey of open-source tools and building blocks for reviewing large, often AI-written diffs and
GitHub pull requests, compiled on 2026-11-04 by four research agents. Feature claims come from
READMEs, docs and `gh repo view` metadata; nothing was installed or run. "Not documented" means the
README is silent, not that the feature is absent. Stars and dates are as of 2026-11-04.

## Why GitHub's PR UI falls short

- Every push reloads the page; nothing updates in place.
- Large diffs are truncated or not rendered at all.
- No LSP: no go-to-definition, find-references or hover while reviewing.
- "Viewed" is per file and resets when the file changes; there is no hunk-level seen state and no
  "only what changed since I last looked" after a force-push.
- Whitespace hiding is per PR and line-based; reformatting and moved code show as full rewrites.
- No guided reading order, no private notes, no structured hand-back to a coding agent.

## Gaps no tool fills

These recur across all four surveys and define what this project is for:

1. **Hunk-level seen state that survives force-pushes.** Marks keyed by content, so only new or
   changed hunks reopen after a rebase or force-push. tuicr is hunk-level but its force-push
   behavior is undocumented; margin and revdiff keep file marks only while the content matches.
2. **Interdiff between PR versions on GitHub.** Gerrit and Review Board diff patch sets, but they
   are separate servers. Reviewable does it for GitHub but is commercial. GitHub itself can lose
   force-pushed commits.
3. **Moved-code display in a review tool.** No interactive reviewer shows "moved from a.rs:120-160"
   as one collapsed line. Engines (git `--color-moved`, sem, GumTree) detect moves but are not
   review UIs.
4. **Format-independent diff inside a review tool.** difftastic and sem are engines only, and
   difftastic has no move detection.
5. **Real LSP in a review tool.** Only checkout-based editor clients (VS Code GitHub PR extension,
   gh-review.nvim, fude.nvim) and dv (Rust-only, three weeks old) have it.
6. **Validated walkthroughs.** Tours exist (codiff, crit, plannotator, diff-tour, lareview) but are
   LLM prose with no check that every hunk is covered, ordered sensibly, or tied to seen state.
7. **Live updates.** At best opt-in polling (octo.nvim, fude.nvim); otherwise manual refresh.
8. **One store** for agent-turn review, pre-PR review and PR review, with private notes that follow
   their hunk across re-reviews.

## Standalone review tools

| Tool | License · ★ · last push · form | PR in / out | Seen state | Walkthrough | Whitespace / moves | Code intel | Notes |
|---|---|---|---|---|---|---|---|
| [tuicr](https://github.com/agavra/tuicr) | MIT · 3.5k · 10-31 · Rust TUI | `tuicr pr N`; `:submit` posts inline review to six forges | Per hunk or file, persisted; preselects commits since last submitted review | None | `ignore_whitespace`; no moves | `$EDITOR` only | Most established terminal PR round trip; Markdown export for agents |
| [codiff](https://github.com/nkzw-tech/codiff) | MIT · 1.5k · 11-04 · Electron + CLI, TS | `codiff pr N`; posts inline comments (unverified) | Per file | `codiff -w` LLM narrative with hunk IDs and order constraints | Hides whitespace-only; no moves | Heuristic Ctrl-click, no LSP | Best all-round feature fit; macOS app, Linux unverified |
| [crit](https://github.com/tomasz-tomczyk/crit) | MIT · 1.3k · 11-02 · Go, local browser UI | `crit pr N`; two-way GitHub/GitLab sync | Per file | "Story mode" LLM chapters | Not documented | None | Round-to-round diffs of agent edits; `/crit` agent loop |
| [dv](https://github.com/evanlinjin/diffview) | MIT/Apache-2.0 · 0 · 10-26 · Rust TUI, Helix keys | `dv N` checks out PR; comment, reply, resolve, submit | Per file, GitHub-synced | None | Word emphasis only | rust-analyzer `gd`/`gr` | Only standalone tool with LSP plus PR round trip; created 2026-10-16 |
| [margin](https://github.com/imrajyavardhan12/Margin) | Apache-2.0 · 0 · 11-04 · Rust TUI (v0.6 RC) | `margin pr N` via gh; no posting | Per file, content-digest keyed, survives force-pushes | None | None | None | Only measured perf: ~4 ms first paint on 100 files / 10k lines |
| [hunk](https://github.com/modem-dev/hunk) | MIT · 9.8k · 11-04 · TS TUI | `hunk gh pr N`; no posting | Not documented | None | None | `hunk open` browses cwd | Popular; live agent session, TS extensions |
| [lumen](https://github.com/jnsahaj/lumen) | MIT · 2.9k · 09-17 · Rust TUI | `lumen diff --pr N` | Per file per commit, synced to GitHub | `lumen explain` summaries | None | Tree-sitter highlighting | Annotations piped to agent |
| [difit](https://github.com/yoshiko-pg/difit) | MIT · 3.3k · 08-28 · TS local web | `--pr URL` reads unresolved threads; no posting | Per file, browser localStorage | None | None | None | Collapses lock/generated files; "copy as prompt" |
| [revdiff](https://github.com/umputun/revdiff) | MIT · 943 · 11-04 · Go TUI | None (git/jj only) | Per file, kept only if diff unchanged | None | Intra-line word diff | Browses all tracked files | Cleanest stdout-to-agent pipe; `??` = answer, don't edit |
| [critique](https://github.com/remorses/critique) | MIT · 1.3k · 11-02 · TS TUI + hosted web | `critique <PR URL>`; no comments | None (stable hunk IDs) | None | `--calldiff` experimental | None | Hides diffs over 6000 lines; sharing is hosted |
| [lareview](https://github.com/puemos/lareview) | Apache-2.0 · 0.2k · 10-31 · Rust desktop | Loads GitHub/GitLab PRs; submits feedback | Per task | Plan grouped by flow, ordered by risk; each task shows its hunks | None | None | Best verified task-based guided review |
| diff-tour (repo URL unverified) | MIT · 361 · 11-02 · Go TUI | None | None | LLM tour, per-step diffs, per-step Q&A | None | None | Best walkthrough UX; read-only, not a review tool |
| plannotator (repo URL unverified) | Apache-2.0 · 9.8k · 11-04 · TS browser | Local, commits, PRs; posts only on send | Per section | AI chapters with their own diffs | Not documented | None | Plan-first; GitHub resets whole files, per its docs |
| diffity (repo URL unverified) | MIT · 899 · 11-03 · macOS app | URL; submits review | Per file, badge on change | Tour skill | None | Any file; search | Mac-only GUI |
| stage-cli (repo URL unverified) | MIT · 276 · 09-07 · TS skill | `--pr` via gh | Per file (PR mode) | Agent chapters, "definitions before users" | None | None | `/stage-resolve` loop |
| octorus (repo URL unverified) | License unclear · 227 · 11-02 · Rust TUI | PR #; inline comments, approve | Per file | None | None | Symbols, jump-to-def without LSP | "AI Rally" automated fix loop |

Smaller or stale: prr (GPL-2.0, offline review file then submit), prtea, phui, diffx, diffnav
(file-tree pager, no review state), gitbutler (Fair Source, no inline review), gauge (stale),
iReview (stale). lazygit, gitui and meld have no PR review.

## Editor-integrated PR review

| Tool | License · ★ · last push | PR load / post | Seen state | Code intel | Verdict |
|---|---|---|---|---|---|
| [VS Code GitHub Pull Requests](https://github.com/microsoft/vscode-pull-request-github) | MIT · 2.6k · 11-04 | Checkout enters review mode; submit comments and reviews | Per file; "Show Changes Since Last Review" (baseline unverified) | Full IntelliSense via checkout | Best if on VS Code |
| octo.nvim | MIT · 3.4k · 11-02 | Full threads via GraphQL; submit, approve | Per file, GitHub-synced; opt-in polling | LSP undocumented; caps PRs at 100 files | Most mature Neovim client |
| gh-review.nvim | Apache-2.0 · 32 · 11-04 | Full via GraphQL, suggestions | Reviewed toggle | Checkout, `gF` LSP jump; caps 100 files | Promising, small |
| fude.nvim | MIT · 12 · 10-29 | `gh pr checkout`; threads, replies, resolve | Per file, synced | Real checkout | Stacked PRs; tiny |
| gh.nvim | MIT · 650 · 2025-01 | Checkout | None | LSP works | Stale |
| diffview.nvim | GPL-3.0+ · last push 2024-08 | Git revs only | None | Editor | Stale, no review state |

Not open source: JetBrains GitHub integration, Reviewable (revision model, "changes since you last
looked" across rebases), Graphite (Code Tours; acquisition by Cursor reported Dec 2025),
CodeRabbit, SemanticDiff (VS Code; moves and renames, closed source). Sapling ReviewStack is no
longer in Sapling's `addons/`.

## Self-hosted review systems

- **Gerrit** (Apache-2.0): patch-set A/B comparison that colors rebase-caused changes separately,
  per-file Reviewed marks, comments ported to newer patch sets (unresolved comments and drafts;
  file-level fallback when the position is gone), four whitespace levels, 30 s polling. Needs its
  own push-for-review workflow.
- **Review Board** (MIT): interdiff between revisions, moved-line indicators within one file,
  indent-only lines flagged. GitHub Enterprise integration is in the paid Power Pack.
- **Phabricator** is unmaintained since 2021; **Phorge** continues it (interdiff unverified).

## Diff engines

| Engine | License · language | Usable as library | Notes |
|---|---|---|---|
| [sem](https://github.com/Ataraxy-Labs/sem) | Apache-2.0 · Rust · created 2026-02 | `sem-core` 0.29 on crates.io; `--format json` | Tree-sitter entities, moves, renames, dependency graph; young, language count inconsistent (28–35) |
| [difftastic](https://github.com/Wilfred/difftastic) | MIT · Rust · 26k★ | CLI only; JSON unstable | AST diff ignoring formatting; "scales relatively poorly"; no moves |
| [diffsitter](https://github.com/afnanenayet/diffsitter) | MIT · Rust | CLI only | "Nowhere close to production ready" |
| [GumTree](https://github.com/GumTreeDiff/gumtree) | LGPL-3.0 · Java | Java API | Insert/delete/move/rename actions; few parsers |
| [RefactoringMiner](https://github.com/tsantalis/RefactoringMiner) | MIT · Java | Java library | Detects refactorings, mostly Java; RAID overlays it on GitHub diffs |
| [imara-diff](https://github.com/pascalkuthe/imara-diff) | Apache-2.0 · Rust | crate 0.2 | Fast Myers and Histogram line diff |
| [similar](https://github.com/mitsuhiko/similar) | Apache-2.0 · Rust | crate | Higher-level diff API |
| [delta](https://github.com/dandavison/delta) | MIT · Rust | Pager | Colors moved lines via git |

Git built-ins: `-w`, `-b`, `--ignore-blank-lines` are line-based, so re-wrapped lines still show as
changes. `--color-moved` (bare = `zebra`; `blocks` needs ≥ 20 alphanumeric characters) with
`--color-moved-ws=allow-indentation-change` tolerates uniform re-indentation; line-level only.

## Hunk identity across pushes

- `git patch-id --stable`: hash of a diff ignoring line numbers and whitespace. Per commit; a
  changed context line changes the ID.
- `git range-diff`: pairs commits across two versions of a series by diff-of-diffs cost
  (Jonker-Volgenant, `--creation-factor` 60). Commit level only.
- `interdiff` (patchutils, GPL-2.0) compares two patches.
- Gerrit comment porting and Reviewable revisions are the main precedents; neither documents its
  mapping algorithm.
- jj change IDs survive rewrites, but only inside jj.

## Code intelligence

- Run real language servers against a checkout of the PR head (what VS Code, dv and the Neovim
  clients do). Simplest and most precise; needs a buildable worktree.
- SCIP indexes ([scip-code/scip](https://github.com/scip-code/scip), Apache-2.0) exist for Rust,
  TypeScript, Python, Java, C/C++, Ruby, C#, Dart, PHP.
- stack-graphs (GitHub's precise navigation) is archived since 2025-09. Kythe and Glean are large
  fact stores.
- `ra_ap_*` crates allow in-process rust-analyzer.

## Ordering and untangling

- `git diff -O<orderfile>` gives deterministic file order.
- SmartCommit (FSE 2021) untangles composite changes by graph partitioning: 71–84 % median
  accuracy without developer input (secondary source). Fully automatic grouping is not reliable.
- LLM walkthroughs: PR-Agent (MIT, 14.6k★) is the open-source reference; review-roadmap and gnosis
  are small experiments; CodeRabbit and Graphite are commercial.

## Rendering huge diffs

- Gitea caps highlighted diffs at 1000 lines per file and 100 files by default; GitHub's thresholds
  are undocumented.
- Fast viewers virtualize rows and highlight per file on expand, caching by blob SHA.
- gpui-component 0.9 ships a read-only virtualized diff view "inspired by Diffs from Pierre"
  ([@pierre/diffs](https://github.com/pierrecomputer/pierre), Apache-2.0).

## UX ideas worth taking

- Key review state to hunk content, not line numbers; on push, keep the cursor and show "3 new
  hunks" instead of reloading (revdiff, critique, margin).
- Several baselines: since my last review, since the agent's last turn, since base.
- Walkthroughs as JSON over hunk IDs, rejected if a hunk is unassigned or a definition comes after
  its use (codiff's schema, stage-cli's ordering rule). Each step shows its own diff and Q&A.
- Calibrate trust: risk per file from change history, traffic-light chunks, gates for auth,
  payments, migrations and public APIs (JetBrains study, arXiv 2606.01969). Predicted review effort
  per file (arXiv 2601.00753).
- Noise defaults: hide whitespace- and reformat-only hunks behind a visible count; collapse
  generated and lock files.
- Moves shown as one line, "moved from a.ts:120-160", collapsed by default.
- Private drafts keyed by hunk; nothing leaves until one explicit submit.
- Agent round trip: numbered Markdown with file:line, hunk ID and a question/change tag; snapshot
  the tree at submit so the next view shows only the agent's changes.

## Zed as a foundation

Checked against Zed `main` at 056a44d (2026-11-04):

- Apache-2.0: `gpui`, `gpui_platform`, `sum_tree`, `collections`, `util`. `gpui` 0.2.2 is on
  crates.io; gpui-component 0.9 depends on the `gpui-pre` 0.3.11 packaging.
- GPL-3.0-or-later: `editor`, `multi_buffer`, `buffer_diff`, `language`, `languages`, `lsp`,
  `project`, `git`, `git_ui`, `ui`, `theme`, `settings`, `workspace` and about 200 more. None of the
  crates checked are AGPL.
- `editor` depends on about 70 Zed crates, including `project`, `workspace`, `client`, `rpc`, `dap`
  and `telemetry`; LSP reaches it through `Project`. Taking `editor` à la carte pulls in most of
  Zed and its app wiring.
- [gpui-component](https://github.com/longbridge/gpui-kit) 0.9 (Apache-2.0) has a virtualized
  read-only diff view, a code editor with LSP popovers (completion, hover, code actions,
  diagnostics) built on `lsp-types`, a syntax highlighter, virtual lists, trees, docks and tabs.
