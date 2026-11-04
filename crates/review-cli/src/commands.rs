//! The commands: each loads a review, acts on it, and prints text or JSON.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::ValueEnum;
use color_eyre::eyre::{self, OptionExt as _, WrapErr as _};
use review_core::walkthrough::{partition, prompt};
use review_core::{
    History, MarkVerdict, MoveId, Repo, Review, ReviewKey, ReviewTarget, RevisionNumber,
    RevisionView, UnitId,
};
use serde::Serialize;

use crate::render::{self, Painter};
use crate::{ColorChoice, ExportFormat, GlobalOptions};

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
    json: bool,
    painter: Painter,
}

impl Context {
    pub fn new(options: &GlobalOptions) -> eyre::Result<Self> {
        if let Some(request) = demo_request(options)? {
            let review = review_core::demo::prepare(
                &review_core::demo::DemoPaths::in_project(),
                &review_core::demo::fixture_dir(),
                request,
            )?;
            return Ok(Self {
                data_dir: review_core::demo::DemoPaths::in_project().data,
                repo_dir: review_core::demo::DemoPaths::in_project().repo,
                review: Some(review.key().as_str().to_owned()),
                json: options.json,
                painter: Painter {
                    color: color_enabled(options),
                },
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
            json: options.json,
            painter: Painter { color },
        })
    }

    fn remember(&self, review: &Review) -> eyre::Result<()> {
        Ok(review_core::store::remember_review(
            &self.data_dir,
            review.repo(),
            review.key(),
        )?)
    }

    fn load(&self) -> eyre::Result<Review> {
        let key = if let Some(key) = &self.review {
            ReviewKey::parse(key.trim())?
        } else {
            let repo = Repo::discover(&self.repo_dir)?;
            review_core::store::remembered_review(&self.data_dir, &repo)?.ok_or_eyre(
                "no review opened for this repository and branch; run `cr open` or pass --review",
            )?
        };
        Ok(Review::load(&self.data_dir, &key)?)
    }
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

fn pick_view(history: &History, revision: Option<u32>) -> eyre::Result<RevisionView<'_>> {
    match revision {
        Some(number) => {
            let number = RevisionNumber::new(number).ok_or_eyre("revisions are numbered from 1")?;
            Ok(history.view(number)?)
        }
        None => Ok(history.current_view()?),
    }
}

fn print_warnings(history: &History) {
    for warning in history.warnings() {
        eprintln!("warning: {warning}");
    }
}

#[derive(Serialize)]
struct OpenOutput<'a> {
    review: &'a str,
    recorded: &'a review_core::Recorded,
    status: review_core::Status,
}

pub fn open(context: &Context, target: Option<&str>, worktree: Option<&str>) -> eyre::Result<()> {
    let repo = Repo::discover(&context.repo_dir)?;
    let target = ReviewTarget::from_arguments(target, worktree, &repo)?;
    let review = Review::open(&context.data_dir, repo, target)?;
    let recorded = review.refresh()?;
    context.remember(&review)?;
    let history = review.history()?;
    print_warnings(&history);
    let status = history.view(recorded.number)?.status();
    if context.json {
        return print_json(&OpenOutput {
            review: review.key().as_str(),
            recorded: &recorded,
            status,
        });
    }
    print!("{}", render::open_summary(review.key(), &recorded, &status));
    Ok(())
}

pub fn status(context: &Context, revision: Option<u32>) -> eyre::Result<()> {
    let review = context.load()?;
    let history = review.history()?;
    print_warnings(&history);
    let status = pick_view(&history, revision)?.status();
    if context.json {
        return print_json(&status);
    }
    print!("{}", render::status(&status));
    Ok(())
}

/// What `cr show` renders.
pub struct ShowOptions {
    pub unreviewed: bool,
    pub step: Option<usize>,
    pub file: Option<String>,
    pub expand: bool,
    pub revision: Option<u32>,
}

pub fn show(context: &Context, options: &ShowOptions) -> eyre::Result<()> {
    let review = context.load()?;
    let history = review.history()?;
    print_warnings(&history);
    let view = pick_view(&history, options.revision)?;
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
                !options.unreviewed || view.state(unit).is_ok_and(|state| !state.is_reviewed());
            in_file && needs_review
        });
    }
    parts.steps.retain(|step| !step.units.is_empty());
    if context.json {
        return print_json(&render::show_json(&view, &parts)?);
    }
    print!(
        "{}",
        render::show(&view, &parts, options.expand, context.painter)?
    );
    Ok(())
}

#[derive(Serialize)]
struct MarkOutput {
    verdict: &'static str,
    marked: Vec<UnitId>,
}

pub fn mark(context: &Context, args: &[String], category: Option<Category>) -> eyre::Result<()> {
    let (verdict, ids) = args
        .split_last()
        .ok_or_eyre("pass the units and a verdict")?;
    let verdict: MarkVerdict = verdict.parse().map_err(|err: String| eyre::eyre!(err))?;
    let review = context.load()?;
    let history = review.history()?;
    let mut by_revision: BTreeMap<RevisionNumber, Vec<UnitId>> = BTreeMap::new();
    if let Some(category) = category {
        eyre::ensure!(
            ids.is_empty(),
            "pass either unit IDs or --category, not both"
        );
        let view = history.current_view()?;
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
                by_revision.entry(view.number()).or_default().push(unit);
            }
        }
    }
    for id in ids {
        if let Ok(unit) = id.parse::<UnitId>() {
            by_revision.entry(unit.revision()).or_default().push(unit);
        } else {
            let found: MoveId = id
                .parse()
                .wrap_err_with(|| format!("{id:?} is neither a unit nor a move ID"))?;
            let view = history.view(found.revision())?;
            by_revision
                .entry(found.revision())
                .or_default()
                .extend(view.units_covered_by_move(found)?);
        }
    }
    let mut marked = Vec::new();
    for (revision, units) in by_revision {
        let view = history.view(revision)?;
        review.mark(&view, &units, verdict)?;
        marked.extend(units);
    }
    if context.json {
        return print_json(&MarkOutput {
            verdict: verdict.name(),
            marked,
        });
    }
    println!("marked {} units {}", marked.len(), verdict.name());
    Ok(())
}

pub fn note(context: &Context, unit: &str, text: &str) -> eyre::Result<()> {
    let unit: UnitId = unit.parse()?;
    let review = context.load()?;
    let history = review.history()?;
    review.note(&history.view(unit.revision())?, unit, text)?;
    if context.json {
        return print_json(&serde_json::json!({ "unit": unit, "note": text }));
    }
    println!("noted {unit}");
    Ok(())
}

pub fn export(context: &Context, format: ExportFormat, revision: Option<u32>) -> eyre::Result<()> {
    let review = context.load()?;
    let history = review.history()?;
    print_warnings(&history);
    let view = pick_view(&history, revision)?;
    let exported = review_core::export::export(&view, review.key().as_str());
    if context.json || format == ExportFormat::Json {
        return print_json(&exported);
    }
    print!("{}", exported.to_markdown());
    Ok(())
}

pub fn walkthrough_prompt(context: &Context) -> eyre::Result<()> {
    let review = context.load()?;
    let history = review.history()?;
    let text = prompt(&history.current_view()?, review.key());
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

pub fn walkthrough_show(context: &Context, revision: Option<u32>) -> eyre::Result<()> {
    let review = context.load()?;
    let history = review.history()?;
    let view = pick_view(&history, revision)?;
    let parts = partition(&view);
    if context.json {
        return print_json(&parts);
    }
    print!("{}", render::partition(&view, &parts));
    Ok(())
}

pub fn watch(context: &Context, interval: u64, count: Option<u64>) -> eyre::Result<()> {
    let review = context.load()?;
    let mut polls = 0;
    loop {
        match review.refresh() {
            Ok(recorded) if recorded.created => {
                let history = review.history()?;
                let status = history.view(recorded.number)?.status();
                if context.json {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &serde_json::json!({ "recorded": recorded, "status": status })
                        )?
                    );
                } else {
                    println!(
                        "New revision {}: {} new and {} changed units",
                        recorded.number, status.total.new, status.total.changed
                    );
                }
            }
            Ok(_) => {}
            Err(err) => eprintln!("warning: refresh failed: {err}"),
        }
        polls += 1;
        if count.is_some_and(|count| polls >= count) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

pub fn worktree(context: &Context, revision: Option<u32>) -> eyre::Result<()> {
    let review = context.load()?;
    let history = review.history()?;
    let view = pick_view(&history, revision)?;
    let record = view.record().ok_or_eyre("revision has no record")?;
    let path = review.worktree(record)?;
    if context.json {
        return print_json(&serde_json::json!({ "revision": view.number(), "path": path }));
    }
    println!("{}", path.display());
    Ok(())
}
