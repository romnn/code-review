//! The review snapshot on screen and the marks waiting for the engine.
//!
//! A snapshot pins one revision of one history; everything shown is derived from it and from
//! caches keyed by blob identity, so replacing the history can never pair new changes with old
//! file text. A mark captures its exact units and the revision they were shown in when the
//! reviewer acts, so a later revision can never widen it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use review_core::{
    Counts, FileStatus, History, MarkVerdict, ObjectId, RevisionNumber, RevisionView, UnitId,
    UnitState, Verdict,
};

use review_core::tree::{NodeKind, TreeNode};

use crate::rows::Badge;

/// One revision of a review's history, as shown.
#[derive(Clone)]
pub struct Snapshot {
    /// The history, as last read from the store.
    pub history: Arc<History>,
    /// The revision shown; switching to a newer one is the reviewer's decision.
    pub revision: RevisionNumber,
}

impl Snapshot {
    /// The shown revision's view.
    #[must_use]
    pub fn view(&self) -> Option<RevisionView<'_>> {
        self.history.view(self.revision).ok()
    }

    /// The newest revision in the history, when it is newer than the one shown.
    #[must_use]
    pub fn newer(&self) -> Option<RevisionNumber> {
        self.history
            .current()
            .filter(|&newest| newest > self.revision)
    }
}

/// A mark the reviewer asked for: the exact units, the revision they were shown in, and the
/// history that showed them.
#[derive(Clone)]
pub struct MarkJob {
    /// The units, resolved when the reviewer acted.
    pub units: Vec<UnitId>,
    /// The revision the units were shown in.
    pub revision: RevisionNumber,
    /// The verdict.
    pub verdict: MarkVerdict,
    /// The history the units were resolved in, which the engine checks the mark against.
    pub history: Arc<History>,
}

/// What background work makes of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Product {
    /// Its text, for context lines and the file buffer.
    Text,
    /// Its highlighting and scopes.
    Syntax,
}

/// Work running on blobs, per product: a blob's text and its highlighting are claimed apart, so
/// a running highlight never keeps its text from loading, and each claim is free again once its
/// work finishes, so a blob that comes back after its results were pruned is worked on again.
#[derive(Debug, Default)]
pub struct Outstanding(HashSet<(ObjectId, Product)>);

impl Outstanding {
    /// Claims `product` of `oid` for new work; `false` when that work is already running.
    pub fn claim(&mut self, oid: &ObjectId, product: Product) -> bool {
        self.0.insert((oid.clone(), product))
    }

    /// Frees the claims of finished work, whatever its outcome.
    pub fn finish(&mut self, claims: &[(ObjectId, Product)]) {
        for claim in claims {
            self.0.remove(claim);
        }
    }
}

/// The files a folder node shows below it: the file nodes after it, deeper than it, up to the
/// next node at its depth or above. A folder acts on exactly these, the files its counts sum,
/// never on a file that only shares the folder's name, such as a deleted `foo` beside `foo/`.
#[must_use]
pub fn folder_files(tree: &[TreeNode], folder: usize) -> Vec<&str> {
    let Some(depth) = tree.get(folder).map(|node| node.depth) else {
        return Vec::new();
    };
    tree.iter()
        .skip(folder + 1)
        .take_while(|node| node.depth > depth)
        .filter(|node| node.kind == NodeKind::File)
        .map(|node| node.path.as_str())
        .collect()
}

/// The nodes of `tree` a filter `query` keeps, as indices: the files whose path contains it,
/// ignoring ASCII case, and the folders that hold them. Folds do not apply while filtering.
#[must_use]
pub fn filtered(tree: &[TreeNode], query: &str) -> Vec<usize> {
    let needle = query.to_ascii_lowercase();
    let mut kept = Vec::new();
    // Walking backwards, the first node shallower than the last kept one is its folder.
    let mut wanted_above = 0;
    for (index, node) in tree.iter().enumerate().rev() {
        let keep = if node.kind == NodeKind::File {
            node.path.to_ascii_lowercase().contains(&needle)
        } else {
            node.depth < wanted_above
        };
        if keep {
            kept.push(index);
            wanted_above = node.depth;
        }
    }
    kept.reverse();
    kept
}

/// Where in `visible` (indices into `tree`) the node `path` of `kind` is, or, when a fold
/// hides it, the innermost visible folder that holds it. Only a node of the same kind counts as
/// the node itself, and only a folder as its holder.
#[must_use]
pub fn cursor_position(
    tree: &[TreeNode],
    visible: &[usize],
    path: &str,
    kind: NodeKind,
) -> Option<usize> {
    let node = |position: usize| visible.get(position).and_then(|&index| tree.get(index));
    (0..visible.len())
        .find(|&position| node(position).is_some_and(|node| node.kind == kind && node.path == path))
        .or_else(|| {
            (0..visible.len()).rev().find(|&position| {
                node(position).is_some_and(|node| {
                    node.kind == NodeKind::Directory
                        && node.path != path
                        && review_core::tree::is_under(path, &node.path)
                })
            })
        })
}

/// Whether `path` is a file of `tree`; a selection survives a new revision only as the same
/// file, never as a folder that took its name.
#[must_use]
pub fn is_file(tree: &[TreeNode], path: &str) -> bool {
    tree.iter()
        .any(|node| node.kind == NodeKind::File && node.path == path)
}

/// The badge a pending verdict shows until the engine has recorded it.
#[must_use]
pub fn pending_badge(verdict: MarkVerdict) -> Badge {
    match verdict {
        MarkVerdict::Seen => Badge::Seen,
        MarkVerdict::Accepted => Badge::Accepted,
        MarkVerdict::Flagged => Badge::Flagged,
        MarkVerdict::Unseen => Badge::Unreviewed,
    }
}

/// The badges of `units`, with the latest pending verdict of each in place of its recorded
/// state.
#[must_use]
pub fn badges(
    view: &RevisionView<'_>,
    units: &[UnitId],
    pending: &HashMap<UnitId, MarkVerdict>,
) -> Vec<Badge> {
    units
        .iter()
        .map(|unit| match pending.get(unit) {
            Some(&verdict) => pending_badge(verdict),
            None => view.state(*unit).map_or(Badge::Unreviewed, Badge::of),
        })
        .collect()
}

/// Whether a badge counts as approved: seen or accepted, and not changed since.
#[must_use]
pub fn is_approved(badge: Badge) -> bool {
    matches!(badge, Badge::Seen | Badge::Accepted)
}

/// The verdict that toggles `badges` towards `toward`: back to unreviewed when every unit
/// already has it, `toward` otherwise. Approval counts seen units as approved.
#[must_use]
pub fn toggled(badges: &[Badge], toward: MarkVerdict) -> MarkVerdict {
    let all = !badges.is_empty()
        && badges.iter().all(|&badge| match toward {
            MarkVerdict::Accepted | MarkVerdict::Seen => is_approved(badge),
            MarkVerdict::Flagged => badge == Badge::Flagged,
            MarkVerdict::Unseen => badge == Badge::Unreviewed,
        });
    if all { MarkVerdict::Unseen } else { toward }
}

/// The engine's file statuses with `pending` verdicts applied, so the tree reacts to a mark at
/// once.
#[must_use]
pub fn statuses(
    view: &RevisionView<'_>,
    pending: &HashMap<UnitId, MarkVerdict>,
) -> Vec<FileStatus> {
    let mut files = view.status().files;
    if pending.is_empty() {
        return files;
    }
    let index: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(position, file)| (file.path.clone(), position))
        .collect();
    for (&unit, &verdict) in pending {
        let (Ok(state), Ok(file)) = (view.state(unit), view.file(unit)) else {
            continue;
        };
        let Some(counts) = index
            .get(file.display_path())
            .and_then(|&position| files.get_mut(position))
            .map(|file| &mut file.counts)
        else {
            continue;
        };
        *slot(counts, from_state(state)) = slot(counts, from_state(state)).saturating_sub(1);
        *slot(counts, from_verdict(verdict)) += 1;
    }
    files
}

/// The count a unit's state falls under.
#[derive(Clone, Copy)]
enum Category {
    New,
    Changed,
    Seen,
    Accepted,
    Flagged,
}

fn from_state(state: UnitState) -> Category {
    match state {
        UnitState::New | UnitState::Unseen { .. } => Category::New,
        UnitState::ChangedSinceReviewed { .. } => Category::Changed,
        UnitState::Reviewed { verdict, .. } => match verdict {
            Verdict::Seen => Category::Seen,
            Verdict::Accepted => Category::Accepted,
            Verdict::Flagged => Category::Flagged,
        },
    }
}

fn from_verdict(verdict: MarkVerdict) -> Category {
    match verdict {
        MarkVerdict::Seen => Category::Seen,
        MarkVerdict::Accepted => Category::Accepted,
        MarkVerdict::Flagged => Category::Flagged,
        MarkVerdict::Unseen => Category::New,
    }
}

fn slot(counts: &mut Counts, category: Category) -> &mut usize {
    match category {
        Category::New => &mut counts.new,
        Category::Changed => &mut counts.changed,
        Category::Seen => &mut counts.reviewed,
        Category::Accepted => &mut counts.accepted,
        Category::Flagged => &mut counts.flagged,
    }
}

/// How much of a file or folder still needs the reviewer, for coloring its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Nothing reviewed yet.
    Open,
    /// Some units reviewed.
    Partial,
    /// Every unit approved.
    Done,
    /// Some unit flagged.
    Flagged,
    /// Some unit changed since it was reviewed.
    Changed,
}

impl Progress {
    /// The progress that `counts` describe.
    #[must_use]
    pub fn of(counts: &Counts) -> Self {
        let reviewed = counts.reviewed + counts.accepted;
        if counts.flagged > 0 {
            Self::Flagged
        } else if counts.changed > 0 {
            Self::Changed
        } else if counts.units > 0 && reviewed == counts.units {
            Self::Done
        } else if reviewed > 0 {
            Self::Partial
        } else {
            Self::Open
        }
    }

    /// Whether the reviewer still needs to look at it.
    #[must_use]
    pub fn needs_attention(self) -> bool {
        self != Self::Done
    }
}

#[cfg(test)]
mod tests {
    use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};

    use super::*;

    /// Approving toggles: a hunk that is all approved goes back to unreviewed, anything else
    /// becomes approved; flagging works the same way.
    #[test]
    fn toggles_go_back_only_when_everything_has_the_verdict() {
        let approved = [Badge::Accepted, Badge::Seen];
        assert_eq!(
            toggled(&approved, MarkVerdict::Accepted),
            MarkVerdict::Unseen
        );
        let mixed = [Badge::Accepted, Badge::Unreviewed];
        assert_eq!(
            toggled(&mixed, MarkVerdict::Accepted),
            MarkVerdict::Accepted
        );
        assert_eq!(
            toggled(&[Badge::Changed], MarkVerdict::Accepted),
            MarkVerdict::Accepted
        );
        assert_eq!(
            toggled(&[Badge::Flagged], MarkVerdict::Flagged),
            MarkVerdict::Unseen
        );
        assert_eq!(toggled(&[], MarkVerdict::Accepted), MarkVerdict::Accepted);
    }

    /// A mark keeps the exact units and revision the reviewer saw: once a newer revision is
    /// recorded, the engine refuses it instead of applying it to the new revision's units, and
    /// the snapshot announces the newer revision rather than switching to it.
    #[test]
    fn marks_never_move_to_a_newer_revision() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DemoPaths::under(dir.path());
        let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh).unwrap();
        let history = Arc::new(review.history().unwrap());
        let revision = history.current().unwrap();
        let view = history.view(revision).unwrap();
        let units = view.units_under(&["src/engine.rs".to_owned()]);
        let job = MarkJob {
            units: units.clone(),
            revision,
            verdict: MarkVerdict::Accepted,
            history: Arc::clone(&history),
        };
        // Another process records revision 2 while the mark waits.
        let second = review_core::RevisionNumber::new(2).unwrap();
        let review = prepare(&paths, &fixture_dir(), DemoRequest::UpTo(second)).unwrap();
        let saved = job
            .history
            .view(job.revision)
            .and_then(|view| review.mark(&view, &job.units, job.verdict));
        assert!(
            saved.is_err(),
            "a mark against revision 1 must not land on revision 2"
        );
        let newer = Snapshot {
            history: Arc::new(review.history().unwrap()),
            revision,
        };
        assert_eq!(newer.newer(), Some(second));
        assert_eq!(
            newer.revision, revision,
            "the shown revision does not change by itself"
        );
    }

    /// A blob's text and highlighting are claimed apart, each once while its work runs and
    /// again after it finished, as when its text returns in a later revision (A, then B, then A)
    /// after its results were pruned.
    #[test]
    fn finished_work_frees_its_claims() {
        let a = ObjectId::parse(&"a".repeat(40)).unwrap();
        let b = ObjectId::parse(&"b".repeat(40)).unwrap();
        let mut outstanding = Outstanding::default();
        assert!(outstanding.claim(&a, Product::Syntax));
        assert!(
            outstanding.claim(&a, Product::Text),
            "highlighting a blob never keeps its text from loading"
        );
        assert!(
            !outstanding.claim(&a, Product::Syntax),
            "running work is not started twice"
        );
        outstanding.finish(&[(a.clone(), Product::Syntax), (a.clone(), Product::Text)]);
        assert!(outstanding.claim(&b, Product::Syntax));
        outstanding.finish(&[(b, Product::Syntax)]);
        assert!(
            outstanding.claim(&a, Product::Syntax),
            "A is highlighted again when it returns"
        );
        assert!(outstanding.claim(&a, Product::Text));
    }

    /// A folder covers the files its counts sum, never a deleted file of the same name.
    #[test]
    fn a_folder_covers_exactly_its_displayed_files() {
        let file = |path: &str| FileStatus {
            path: path.to_owned(),
            counts: Counts {
                units: 1,
                new: 1,
                ..Counts::default()
            },
            renamed_from: None,
            change: review_core::FileChangeSummary::modified(),
        };
        let tree = review_core::tree::file_tree(&[file("foo"), file("foo/bar.rs"), file("zed.rs")]);
        let folder = tree
            .iter()
            .position(|node| node.kind == NodeKind::Directory && node.path == "foo")
            .unwrap();
        assert_eq!(folder_files(&tree, folder), ["foo/bar.rs"]);
        assert_eq!(tree[folder].counts.units, 1, "the counts agree");
        assert!(is_file(&tree, "foo"), "the deleted file is its own node");
    }

    /// A filter keeps the matching files and exactly the folders above them.
    #[test]
    fn a_filter_keeps_matches_and_their_folders() {
        let file = |path: &str| FileStatus {
            path: path.to_owned(),
            counts: Counts {
                units: 1,
                new: 1,
                ..Counts::default()
            },
            renamed_from: None,
            change: review_core::FileChangeSummary::modified(),
        };
        let tree = review_core::tree::file_tree(&[
            file("a/b/needle.rs"),
            file("a/b/other.rs"),
            file("a/c/x.rs"),
            file("d/Needle.md"),
            file("top.rs"),
        ]);
        let shown = |query: &str| -> Vec<String> {
            filtered(&tree, query)
                .into_iter()
                .map(|index| tree[index].path.clone())
                .collect()
        };
        assert_eq!(
            shown("needle"),
            ["a", "a/b", "a/b/needle.rs", "d", "d/Needle.md"]
        );
        assert_eq!(shown("top"), ["top.rs"]);
        assert_eq!(shown("a/c"), ["a", "a/c", "a/c/x.rs"]);
        assert_eq!(shown("absent"), Vec::<String>::new());
    }

    /// The cursor keeps a node's kind: with a deleted file `foo` beside a folder `foo/`, the
    /// folder stays the folder when folds hide or show its files, and the file stays the file.
    #[test]
    fn the_cursor_keeps_file_and_folder_apart() {
        let file = |path: &str| FileStatus {
            path: path.to_owned(),
            counts: Counts {
                units: 1,
                new: 1,
                ..Counts::default()
            },
            renamed_from: None,
            change: review_core::FileChangeSummary::modified(),
        };
        let tree = review_core::tree::file_tree(&[file("foo"), file("foo/bar.rs")]);
        let all: Vec<usize> = (0..tree.len()).collect();
        let folder = cursor_position(&tree, &all, "foo", NodeKind::Directory).unwrap();
        let deleted = cursor_position(&tree, &all, "foo", NodeKind::File).unwrap();
        assert_ne!(folder, deleted);
        assert_eq!(tree[all[folder]].kind, NodeKind::Directory);
        assert_eq!(tree[all[deleted]].kind, NodeKind::File);
        // Folded: `foo/bar.rs` is hidden, and its cursor goes to the folder, not the file.
        let hidden: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&index| tree[index].path != "foo/bar.rs")
            .collect();
        let holder = cursor_position(&tree, &hidden, "foo/bar.rs", NodeKind::File).unwrap();
        assert_eq!(tree[hidden[holder]].kind, NodeKind::Directory);
        assert_eq!(tree[hidden[holder]].path, "foo");
    }

    /// A selected file resolves to exactly its own units, and only a file keeps a selection;
    /// a folder of the same name, as when a file becomes a directory, does not.
    #[test]
    fn selections_resolve_to_exactly_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let review = prepare(
            &DemoPaths::under(dir.path()),
            &fixture_dir(),
            DemoRequest::Fresh,
        )
        .unwrap();
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let tree = review_core::tree::file_tree(&view.status().files);
        assert!(is_file(&tree, "src/engine.rs"));
        assert!(!is_file(&tree, "src"), "a folder is never a file selection");
        assert_eq!(view.units_of_file("src"), Vec::new());
        assert_ne!(view.units_under(&["src".to_owned()]).len(), 0);
        let engine = view.units_of_file("src/engine.rs");
        assert_ne!(engine.len(), 0);
        assert!(
            engine
                .iter()
                .all(|&unit| { view.file(unit).unwrap().display_path() == "src/engine.rs" })
        );
    }

    /// A pending verdict moves its unit between the counts of its file before the engine has
    /// recorded it, and the snapshot reports a newer revision only when there is one.
    #[test]
    fn pending_verdicts_show_in_file_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let review = prepare(
            &DemoPaths::under(dir.path()),
            &fixture_dir(),
            DemoRequest::Fresh,
        )
        .unwrap();
        let history = Arc::new(review.history().unwrap());
        let snapshot = Snapshot {
            revision: history.current().unwrap(),
            history: Arc::clone(&history),
        };
        assert!(snapshot.newer().is_none());
        let view = snapshot.view().unwrap();
        let unit = view
            .unit_ids()
            .into_iter()
            .find(|&unit| matches!(view.state(unit), Ok(UnitState::New)))
            .unwrap();
        let path = view.file(unit).unwrap().display_path().to_owned();
        let counts = |files: &[FileStatus]| {
            files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .counts
                .clone()
        };
        let before = counts(&statuses(&view, &HashMap::new()));
        let pending = HashMap::from([(unit, MarkVerdict::Accepted)]);
        let after = counts(&statuses(&view, &pending));
        assert_eq!(after.new + 1, before.new);
        assert_eq!(after.accepted, before.accepted + 1);
        assert_eq!(
            badges(&view, &[unit], &pending),
            [Badge::Accepted],
            "the pending verdict shows as the unit's badge"
        );
    }
}
