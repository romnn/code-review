//! The commands: each loads a review, acts on it, and prints text or JSON.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::ValueEnum;
use color_eyre::eyre::{self, OptionExt as _, WrapErr as _};
use review_core::github::{self, CheckoutPullRequest};
use review_core::walkthrough::{partition, prompt};
use review_core::{
    History, MarkVerdict, MoveId, Recorded, Repo, Review, ReviewKey, ReviewPolicy, ReviewTag,
    ReviewTarget, RevisionNumber, RevisionView, Standing, Tagged, UnitId,
};
use serde::Serialize;

use crate::reference::Reference;
use crate::render::{self, Painter};
use crate::{ColorChoice, ExportFormat, GlobalOptions};

/// How long looking up the checked-out branch's pull request with `gh` may take.
const PULL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A revision as `--revision` gives it, `3` or `3@3fa2b1c0`.
pub type RevisionReference = Reference<RevisionNumber>;

/// A category of units that `cr mark --category` marks at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Category {
    /// Generated, lockfile and snapshot files.
    Noise,
    /// Proven formatting-only changes.
    Formatting,
    /// Units covered completely by plain moves.
    Moved,
    /// All of the above.
    Collapsed,
}

/// Resolved global options.
pub struct Context {
    data_dir: PathBuf,
    repo_dir: PathBuf,
    review: Option<String>,
    /// Whether the command line named the review, with `--review` or the demo, so that bare
    /// IDs are known to be its own.
    named: bool,
    json: bool,
    painter: Painter,
    /// Which units still need review in every view the commands read.
    policy: ReviewPolicy,
}

impl Context {
    pub fn new(options: &GlobalOptions) -> eyre::Result<Self> {
        if let Some(request) = demo_request(options)? {
            let paths = options.demo_dir.as_deref().map_or_else(
                review_core::demo::DemoPaths::in_project,
                review_core::demo::DemoPaths::under,
            );
            let review =
                review_core::demo::prepare(&paths, &review_core::demo::fixture_dir(), request)?;
            return Ok(Self {
                data_dir: paths.data,
                repo_dir: paths.repo,
                review: Some(review.key().as_str().to_owned()),
                named: true,
                json: options.json,
                painter: Painter {
                    color: color_enabled(options),
                },
                policy: policy(options),
            });
        }
        let data_dir = match &options.data_dir {
            Some(data_dir) => data_dir.clone(),
            None => review_core::store::default_data_dir(
                options.xdg_data_home.as_deref(),
                options.home.as_deref(),
            )
            .ok_or_eyre("cannot find a data directory; pass --data-dir")?,
        };
        let color = color_enabled(options);
        Ok(Self {
            data_dir,
            repo_dir: options.repo.clone(),
            review: options.review.clone(),
            named: options.review.is_some(),
            json: options.json,
            painter: Painter { color },
            policy: policy(options),
        })
    }

    /// Loads the review `--review` names, or else opens the checkout's.
    fn load(&self) -> eyre::Result<Review> {
        match &self.review {
            Some(key) => Ok(Review::load(
                &self.data_dir,
                &ReviewKey::parse(key.trim())?,
            )?),
            None => Ok(Review::open_checkout(
                &self.data_dir,
                Repo::discover(&self.repo_dir)?,
            )?),
        }
    }

    /// Loads the review to show, as [`Self::load`] does, and when it is the review of a
    /// checkout shown at its current revision (no `revision`), records what the checkout holds
    /// now first: what a command prints, others refer to, so it has to be a revision (ADR 4).
    ///
    /// While cr's data is at its limit, nothing is recorded and the last recorded revision
    /// shows, as in the windows, saying so.
    fn load_shown(&self, revision: Option<&RevisionReference>) -> eyre::Result<Shown> {
        let review = self.load()?;
        let recorded = match review.target() {
            ReviewTarget::Checkout { .. } if revision.is_none() => {
                match record(&review, &mut PullRequestLookup::default()) {
                    Ok(recorded) => Some(recorded),
                    Err(err)
                        if matches!(
                            err.downcast_ref(),
                            Some(review_core::Error::StorageFull { .. })
                        ) =>
                    {
                        eprintln!("cr: showing the last recorded revision: {err:#}");
                        None
                    }
                    Err(err) => return Err(err),
                }
            }
            _ => None,
        };
        Ok(Shown { review, recorded })
    }
}

/// A review a command shows, and what showing it recorded.
struct Shown {
    review: Review,
    /// The capture of the checkout that was recorded, or found recorded already, for the
    /// review of a checkout at its current revision.
    recorded: Option<Recorded>,
}

/// Records what the review's target holds now, as [`Review::refresh`] does, with the
/// checked-out branch's pull request for the review of a checkout.
fn record(review: &Review, lookup: &mut PullRequestLookup) -> eyre::Result<Recorded> {
    let pull_request = match review.target() {
        ReviewTarget::Checkout { .. } => lookup.find(review),
        _ => None,
    };
    match review.refresh_with(pull_request.as_ref(), &mut |_| {}) {
        Ok(recorded) => Ok(recorded),
        Err(review_core::Error::GitBusy(busy)) => {
            eyre::bail!("git is busy: {busy}; try again when it finishes")
        }
        Err(err) => Err(err.into()),
    }
}

/// Looks up the checked-out branch's pull request, and says once why it could not.
#[derive(Default)]
struct PullRequestLookup {
    reported: bool,
}

impl PullRequestLookup {
    /// Returns what `gh` finds for the branch checked out in `review`'s repository, or `None`
    /// when `HEAD` is detached, the repository has no remote, or `gh` fails, which is no reason
    /// to fail the command: the base is then the default branch.
    fn find(&mut self, review: &Review) -> Option<CheckoutPullRequest> {
        let repo = review.repo();
        let found = match repo.remotes() {
            Ok(remotes) if remotes.is_empty() => return None,
            Ok(_) => github::for_checkout(repo, PULL_REQUEST_TIMEOUT),
            Err(err) => Err(err),
        };
        found.unwrap_or_else(|err| {
            if !self.reported {
                self.reported = true;
                eprintln!(
                    "cr: no pull request looked up, so the base is the default branch: {:#}",
                    eyre::Report::new(err)
                );
            }
            None
        })
    }
}

/// The policy the global options ask for: marks only with `--no-auto-approve`, and otherwise
/// what the configuration file the apps share says, which is automatic approval when there is
/// no such file, as in the apps.
///
/// A configuration file `cr` cannot read decides by marks alone, with a warning, so a broken
/// file never approves units the reviewer may have asked to review.
fn policy(options: &GlobalOptions) -> ReviewPolicy {
    if options.no_auto_approve {
        return ReviewPolicy::MARKS_ONLY;
    }
    let path = options.config.clone().or_else(|| {
        review_core::store::default_config_file(
            options.xdg_config_home.as_deref(),
            options.home.as_deref(),
        )
    });
    let Some(path) = path else {
        return crate::config::UNCONFIGURED;
    };
    crate::config::review_policy(&path).unwrap_or_else(|err| {
        eprintln!("warning: {err:#}; formatting-only units need review");
        ReviewPolicy::MARKS_ONLY
    })
}

fn color_enabled(options: &GlobalOptions) -> bool {
    match options.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => options.no_color.is_none() && std::io::stdout().is_terminal(),
    }
}

/// Returns the demo state the global options ask for, if any.
fn demo_request(options: &GlobalOptions) -> eyre::Result<Option<review_core::demo::DemoRequest>> {
    use review_core::demo::DemoRequest;

    Ok(match (options.demo, options.demo_revision) {
        (true, _) => Some(DemoRequest::Fresh),
        (false, Some(revision)) => Some(DemoRequest::UpTo(
            RevisionNumber::new(revision).ok_or_eyre("demo revisions are numbered from 1")?,
        )),
        (false, None) => None,
    })
}

fn print_json<T: Serialize>(value: &T) -> eyre::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Returns the view of the revision `revision` names in `review`; without one, of the
/// revision the command `recorded`, or else of the current one.
///
/// What the command recorded is what it shows, with what that capture was of, even when
/// another process recorded a newer revision since.
fn pick_view<'h>(
    context: &Context,
    review: &Review,
    history: &'h History,
    revision: Option<&RevisionReference>,
    recorded: Option<&Recorded>,
) -> eyre::Result<RevisionView<'h>> {
    let view = match (revision, recorded) {
        (Some(revision), _) => history.view_under(revision.check(review.key())?, context.policy)?,
        (None, Some(recorded)) => history.view_under(recorded.number, context.policy)?,
        (None, None) => history.current_view_under(context.policy)?,
    };
    Ok(view)
}

fn print_warnings(history: &History) {
    for warning in history.warnings() {
        eprintln!("warning: {warning}");
    }
}

#[derive(Serialize)]
struct OpenOutput<'a> {
    review: &'a str,
    tag: ReviewTag,
    recorded: &'a Recorded,
    status: review_core::Status,
}

pub fn open(context: &Context, target: Option<&str>, worktree: Option<&str>) -> eyre::Result<()> {
    let repo = Repo::discover(&context.repo_dir)?;
    let review = if target.is_none() && worktree.is_none() {
        Review::open_checkout(&context.data_dir, repo)?
    } else {
        let target = ReviewTarget::from_arguments(target, worktree, &repo)?;
        Review::open(&context.data_dir, repo, target)?
    };
    let recorded = record(&review, &mut PullRequestLookup::default())?;
    let history = review.history()?;
    print_warnings(&history);
    let status = history
        .view_under(recorded.number, context.policy)?
        .status();
    if context.json {
        return print_json(&OpenOutput {
            review: review.key().as_str(),
            tag: review.key().tag(),
            recorded: &recorded,
            status,
        });
    }
    print!("{}", render::open_summary(review.key(), &recorded, &status));
    Ok(())
}

/// The document `cr status --json` prints.
#[derive(Serialize)]
struct StatusOutput<'a> {
    review: &'a str,
    tag: ReviewTag,
    /// What the capture of the checkout this command recorded was of.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<&'a review_core::Subject>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    filtered: &'a [String],
    #[serde(flatten)]
    status: review_core::Status,
}

pub fn status(context: &Context, revision: Option<&RevisionReference>) -> eyre::Result<()> {
    let Shown { review, recorded } = context.load_shown(revision)?;
    let history = review.history()?;
    print_warnings(&history);
    let status = pick_view(context, &review, &history, revision, recorded.as_ref())?.status();
    if context.json {
        return print_json(&StatusOutput {
            review: review.key().as_str(),
            tag: review.key().tag(),
            subject: recorded
                .as_ref()
                .and_then(|recorded| recorded.subject.as_deref()),
            filtered: recorded
                .as_ref()
                .map_or(&[], |recorded| recorded.filtered.as_slice()),
            status,
        });
    }
    print!("{}", render::capture(recorded.as_ref()));
    print!("{}", render::status(review.key(), &status));
    Ok(())
}

/// What `cr show` renders.
pub struct ShowOptions {
    pub unreviewed: bool,
    pub step: Option<usize>,
    pub file: Option<String>,
    pub expand: bool,
    pub revision: Option<RevisionReference>,
}

pub fn show(context: &Context, options: &ShowOptions) -> eyre::Result<()> {
    let Shown { review, recorded } = context.load_shown(options.revision.as_ref())?;
    let history = review.history()?;
    print_warnings(&history);
    let view = pick_view(
        context,
        &review,
        &history,
        options.revision.as_ref(),
        recorded.as_ref(),
    )?;
    let mut parts = partition(&view);
    if let Some(step) = options.step {
        let index = step
            .checked_sub(1)
            .ok_or_eyre("steps are numbered from 1")?;
        let selected = parts.steps.get(index).cloned().ok_or_eyre("no such step")?;
        parts.steps = vec![selected];
    }
    for step in &mut parts.steps {
        step.units.retain(|&unit| {
            let in_file = options.file.as_ref().is_none_or(|path| {
                view.file(unit)
                    .is_ok_and(|file| file.display_path() == path)
            });
            let needs_review =
                !options.unreviewed || view.standing(unit).is_ok_and(Standing::needs_review);
            in_file && needs_review
        });
    }
    parts.steps.retain(|step| !step.units.is_empty());
    let shown = render::Shown {
        key: review.key(),
        view: &view,
        parts: &parts,
        recorded: recorded.as_ref(),
    };
    if context.json {
        return print_json(&render::show_json(&shown)?);
    }
    print!("{}", render::show(&shown, options.expand, context.painter)?);
    Ok(())
}

#[derive(Serialize)]
struct MarkOutput {
    verdict: &'static str,
    marked: Vec<Tagged<UnitId>>,
}

/// What `cr mark` marks.
pub struct MarkOptions<'a> {
    /// Unit and move IDs, followed by the verdict.
    pub args: &'a [String],
    pub category: Option<Category>,
    pub files: &'a [String],
    /// The revision the caller saw, which marks of files and categories go on.
    pub revision: Option<&'a RevisionReference>,
}

pub fn mark(context: &Context, options: &MarkOptions<'_>) -> eyre::Result<()> {
    let MarkOptions {
        args,
        category,
        files,
        revision,
    } = *options;
    let (verdict, ids) = args
        .split_last()
        .ok_or_eyre("pass the units and a verdict")?;
    let verdict: MarkVerdict = verdict.parse().map_err(|err: String| eyre::eyre!(err))?;
    let review = context.load()?;
    let history = review.history()?;
    if !files.is_empty() || category.is_some() {
        eyre::ensure!(
            ids.is_empty() && (files.is_empty() || category.is_none()),
            "pass either unit IDs, --category or --file"
        );
        let revision = revision.ok_or_eyre(
            "pass the revision `cr show` printed with --revision, such as --revision 3@<tag>, \
             so the marks go on what you saw",
        )?;
        let seen = revision.resolve(review.key(), context.named)?;
        let view = history.current_view_under(context.policy)?;
        eyre::ensure!(
            view.number() == seen,
            "revision {seen} is not current; the review is at revision {} now, so run `cr show` \
             again before marking",
            view.number()
        );
        let marked = match category {
            Some(category) => {
                let units = category_units(&view, category)?;
                review.mark(&view, &units, verdict)?;
                units
            }
            None => review.mark_files(&view, files, verdict)?,
        };
        return print_marked(context, &review, verdict, marked);
    }
    eyre::ensure!(
        revision.is_none(),
        "--revision goes with --file and --category; unit and move IDs carry their revision"
    );
    let mut by_revision: BTreeMap<RevisionNumber, Vec<UnitId>> = BTreeMap::new();
    for id in ids {
        if let Ok(unit) = id.parse::<Reference<UnitId>>() {
            let unit = unit.resolve(review.key(), context.named)?;
            by_revision.entry(unit.revision()).or_default().push(unit);
        } else {
            let found = id
                .parse::<Reference<MoveId>>()
                .wrap_err_with(|| format!("{id:?} is neither a unit nor a move ID"))?
                .resolve(review.key(), context.named)?;
            let view = history.view_under(found.revision(), context.policy)?;
            by_revision
                .entry(found.revision())
                .or_default()
                .extend(view.units_covered_by_move(found)?);
        }
    }
    let mut marked = Vec::new();
    for (revision, units) in by_revision {
        let view = history.view_under(revision, context.policy)?;
        review.mark(&view, &units, verdict)?;
        marked.extend(units);
    }
    print_marked(context, &review, verdict, marked)
}

/// Returns the units of `category` in `view`.
fn category_units(view: &RevisionView<'_>, category: Category) -> eyre::Result<Vec<UnitId>> {
    let mut units = Vec::new();
    for unit in view.unit_ids() {
        let record = view.unit(unit)?;
        let noise = view.noise(unit)?.is_some();
        let wanted = match category {
            Category::Noise => noise,
            Category::Formatting => record.formatting_only,
            Category::Moved => record.moved,
            Category::Collapsed => noise || record.formatting_only || record.moved,
        };
        if wanted {
            units.push(unit);
        }
    }
    Ok(units)
}

fn print_marked(
    context: &Context,
    review: &Review,
    verdict: MarkVerdict,
    marked: Vec<UnitId>,
) -> eyre::Result<()> {
    if context.json {
        return print_json(&MarkOutput {
            verdict: verdict.name(),
            marked: marked
                .into_iter()
                .map(|unit| render::tagged(review.key(), unit))
                .collect(),
        });
    }
    println!("marked {} units {}", marked.len(), verdict.name());
    Ok(())
}

pub fn note(context: &Context, unit: &str, text: &str) -> eyre::Result<()> {
    let unit: Reference<UnitId> = unit.parse()?;
    let review = context.load()?;
    let unit = unit.resolve(review.key(), context.named)?;
    let history = review.history()?;
    review.note(
        &history.view_under(unit.revision(), context.policy)?,
        unit,
        text,
    )?;
    let unit = render::tagged(review.key(), unit);
    if context.json {
        return print_json(&serde_json::json!({ "unit": unit, "note": text }));
    }
    println!("noted {unit}");
    Ok(())
}

pub fn drop_review(context: &Context, review: &str) -> eyre::Result<()> {
    let key = ReviewKey::parse(review.trim())?;
    Review::remove(&context.data_dir, &key)?;
    if context.json {
        return print_json(&serde_json::json!({ "dropped": key }));
    }
    println!("dropped {key}");
    Ok(())
}

pub fn export(
    context: &Context,
    format: ExportFormat,
    revision: Option<&RevisionReference>,
) -> eyre::Result<()> {
    let Shown { review, recorded } = context.load_shown(revision)?;
    let history = review.history()?;
    print_warnings(&history);
    let view = pick_view(context, &review, &history, revision, recorded.as_ref())?;
    let exported = review_core::export::export(&view, review.key());
    if context.json || format == ExportFormat::Json {
        return print_json(&exported);
    }
    print!("{}", exported.to_markdown());
    Ok(())
}

pub fn walkthrough_prompt(context: &Context) -> eyre::Result<()> {
    let Shown { review, recorded } = context.load_shown(None)?;
    let history = review.history()?;
    let text = prompt(
        &pick_view(context, &review, &history, None, recorded.as_ref())?,
        review.key(),
    );
    if context.json {
        return print_json(&serde_json::json!({ "prompt": text }));
    }
    print!("{text}");
    Ok(())
}

pub fn walkthrough_import(context: &Context, file: &Path) -> eyre::Result<()> {
    let json = if file == Path::new("-") {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        std::fs::read_to_string(file)
            .wrap_err_with(|| format!("failed to read {}", file.display()))?
    };
    let review = context.load()?;
    let history = review.history()?;
    let revision = review.import_walkthrough(&history, &json)?;
    if context.json {
        return print_json(&serde_json::json!({ "imported_for_revision": revision }));
    }
    println!("imported a walkthrough for revision {revision}");
    Ok(())
}

pub fn walkthrough_show(
    context: &Context,
    revision: Option<&RevisionReference>,
) -> eyre::Result<()> {
    let Shown { review, recorded } = context.load_shown(revision)?;
    let history = review.history()?;
    let view = pick_view(context, &review, &history, revision, recorded.as_ref())?;
    let parts = partition(&view);
    if context.json {
        return print_json(&render::partition_json(review.key(), &parts));
    }
    print!("{}", render::partition(&view, &parts));
    Ok(())
}

pub fn watch(context: &Context, interval: u64, count: Option<u64>) -> eyre::Result<()> {
    let review = context.load()?;
    let mut lookup = PullRequestLookup::default();
    let mut polls = 0;
    loop {
        match record(&review, &mut lookup) {
            Ok(recorded) if recorded.created => {
                let history = review.history()?;
                let status = history
                    .view_under(recorded.number, context.policy)?
                    .status();
                let number = render::tagged(review.key(), recorded.number);
                if context.json {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "revision": number,
                            "recorded": recorded,
                            "status": status,
                        }))?
                    );
                } else {
                    println!(
                        "New revision {number}: {} new and {} changed units",
                        status.total.new, status.total.changed
                    );
                }
            }
            Ok(_) => {}
            Err(err) => eprintln!("warning: refresh failed: {err:#}"),
        }
        polls += 1;
        if count.is_some_and(|count| polls >= count) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Runs git in `dir`, isolated from the user's and the system's configuration, with the
    /// workspace's `tests/gitconfig` in place of the user's, and with a fixed identity.
    fn git(dir: &Path, args: &[&str]) -> eyre::Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env(
                "GIT_CONFIG_GLOBAL",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()?;
        eyre::ensure!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    /// A command shows the revision it recorded, which its capture's subject describes, even
    /// when another process recorded a newer one before the command read the history.
    #[test]
    fn a_command_shows_the_revision_it_recorded() -> eyre::Result<()> {
        let repo = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        git(repo.path(), &["init", "--quiet", "--initial-branch=main"])?;
        std::fs::write(repo.path().join("a.rs"), "fn a() {}\n")?;
        git(repo.path(), &["add", "--all"])?;
        git(repo.path(), &["commit", "--quiet", "-m", "base"])?;
        std::fs::write(repo.path().join("a.rs"), "fn a() { first(); }\n")?;
        let review = Review::open_checkout(data.path(), Repo::discover(repo.path())?)?;
        let recorded = review.refresh()?;

        // Another process records the next edit.
        std::fs::write(repo.path().join("a.rs"), "fn a() { second(); }\n")?;
        let newer = review.refresh()?;
        assert_ne!(newer.number, recorded.number);

        let context = Context {
            data_dir: data.path().to_path_buf(),
            repo_dir: repo.path().to_path_buf(),
            review: None,
            named: false,
            json: false,
            painter: Painter { color: false },
            policy: ReviewPolicy::MARKS_ONLY,
        };
        let history = review.history()?;
        let view = pick_view(&context, &review, &history, None, Some(&recorded))?;
        assert_eq!(view.number(), recorded.number);
        Ok(())
    }
}
