//! GitHub pull request metadata through the `gh` CLI, and fetching into temporary refs.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::git::{BranchIdentity, Repo};
use crate::ids::ObjectId;

/// How long a `gh` command that the caller gave no time limit may take.
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// What `gh pr view` reports about a pull request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestInfo {
    pub url: String,
    pub base_ref_name: String,
}

/// A pull request's repository coordinates, parsed from its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequestUrl {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

/// Parses `https://<host>/<owner>/<repo>/pull/<number>[/...]`.
pub(crate) fn parse_url(url: &str) -> Option<PullRequestUrl> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let mut parts = rest.split('/');
    let host = parts.next()?;
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next()? != "pull" {
        return None;
    }
    let number = parts.next()?.split(['#', '?']).next()?.parse().ok()?;
    let valid = |part: &str| !part.is_empty() && !part.contains(['#', ' ']);
    (valid(host) && valid(owner) && valid(repo)).then(|| PullRequestUrl {
        host: host.to_owned(),
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
    })
}

/// Runs `gh pr view <spec>` in the repository and returns the pull request's metadata.
pub(crate) fn view(repo: &Repo, spec: &str) -> Result<PullRequestInfo, Error> {
    pr_view(repo, Some(spec), "url,baseRefName", GH_TIMEOUT)
}

/// Runs `gh pr view <spec>` for the pull request's title, description, author, labels and
/// dates.
pub(crate) fn details(repo: &Repo, spec: &str) -> Result<super::info::GhPullRequest, Error> {
    pr_view(repo, Some(spec), super::info::GH_FIELDS, GH_TIMEOUT)
}

/// A repository on a forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repository {
    /// The forge host, such as `github.com`.
    pub host: String,
    /// The owner's login.
    pub owner: String,
    /// The repository's name.
    pub name: String,
}

/// Whether a pull request is open, merged or closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    /// Open for review.
    Open,
    /// Merged into its base branch.
    Merged,
    /// Closed without merging.
    Closed,
}

/// The pull request of a branch, as [`for_checkout`] finds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchPullRequest {
    /// Its number.
    pub number: u64,
    /// Its web page.
    pub url: String,
    /// Whether it is open, merged or closed.
    pub state: PullRequestState,
    /// The branch it merges into, in [`Self::base_repository`].
    pub base_branch: String,
    /// The repository it merges into.
    pub base_repository: Repository,
    /// The branch it merges from, in [`Self::head_repository`].
    pub head_branch: String,
    /// The repository it merges from, or `None` when the forge no longer knows it, as after a
    /// fork was deleted.
    pub head_repository: Option<Repository>,
    /// The author's login, when the forge reports one.
    pub author: Option<String>,
}

/// `gh pr view --json` output for [`for_checkout`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhBranchPullRequest {
    number: u64,
    url: String,
    state: String,
    base_ref_name: String,
    head_ref_name: String,
    head_repository: Option<GhName>,
    head_repository_owner: Option<GhLogin>,
    author: Option<GhLogin>,
}

#[derive(Deserialize)]
struct GhName {
    name: String,
}

#[derive(Deserialize)]
struct GhLogin {
    login: String,
}

/// What `gh` found for the branch checked out: its pull request, if it has one, and the branch
/// as it was while `gh` looked, which decides which pull request it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckoutPullRequest {
    /// The branch, where it pulls from and pushes to, and its upstream's commit.
    pub branch: BranchIdentity,
    /// Its pull request, or `None` when `gh` found none.
    pub pull_request: Option<BranchPullRequest>,
}

/// Looks up the checked-out branch's pull request with one `gh pr view` in the working tree,
/// without naming the branch, so `gh` resolves it as it does for the reviewer: from where the
/// branch pushes and pulls, whatever its local name, from a fork, or from a pull request's own
/// ref as `gh pr checkout` sets it up.
///
/// Returns `None` while `HEAD` is detached, which has no pull request to look up.
/// The answer is for the branch as it was before and after `gh` ran, which must be the same.
///
/// # Errors
///
/// Returns [`Error::Raced`] when the branch, where it pulls from or pushes to, or its upstream's
/// commit changed while `gh` ran, or the pull request is not the one the branch pulls, so the
/// answer may be for another; [`Error::TimedOut`] when
/// `gh` takes longer than `timeout`, which kills it; and an error when `gh` cannot run, fails
/// other than by finding nothing, or reports something else.
pub fn for_checkout(repo: &Repo, timeout: Duration) -> Result<Option<CheckoutPullRequest>, Error> {
    let identity = |repo: &Repo| -> Result<Option<BranchIdentity>, Error> {
        match repo.head()?.branch {
            Some(branch) => Ok(Some(repo.branch_identity(&branch)?)),
            None => Ok(None),
        }
    };
    let Some(before) = identity(repo)? else {
        return Ok(None);
    };
    let found = pull_request_of_checkout(repo, timeout)?;
    if identity(repo)?.as_ref() != Some(&before)
        || found
            .as_ref()
            .is_some_and(|found| !belongs_to(found, &before))
    {
        return Err(Error::Raced("branch"));
    }
    Ok(Some(CheckoutPullRequest {
        branch: before,
        pull_request: found,
    }))
}

/// Whether `pull_request` is the one `branch` pulls: the one whose head is the branch its merge
/// ref names, the one whose own ref it pulls, or, without a merge ref, the one whose head has the
/// branch's local name.
///
/// The branch read before and after `gh` ran may be the same while `gh` read another one in
/// between, as when the checkout switched away and back, so the answer itself is checked.
fn belongs_to(pull_request: &BranchPullRequest, branch: &BranchIdentity) -> bool {
    let merge = branch.merge.as_deref();
    if let Some(head) = merge.and_then(|merge| merge.strip_prefix("refs/heads/")) {
        return pull_request.head_branch == head;
    }
    if let Some(number) = merge
        .and_then(|merge| merge.strip_prefix("refs/pull/"))
        .and_then(|rest| rest.strip_suffix("/head"))
    {
        return number.parse() == Ok(pull_request.number);
    }
    pull_request.head_branch == branch.branch
}

/// Runs `gh pr view` for the checked-out branch and reads its answer, `None` when it finds no
/// pull request.
fn pull_request_of_checkout(
    repo: &Repo,
    timeout: Duration,
) -> Result<Option<BranchPullRequest>, Error> {
    let found: GhBranchPullRequest = match pr_view(
        repo,
        None,
        "number,url,state,baseRefName,headRefName,headRepository,headRepositoryOwner,author",
        timeout,
    ) {
        Ok(found) => found,
        Err(Error::CommandFailed { stderr, .. })
            if stderr.to_lowercase().contains("no pull requests found") =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let unexpected = |detail: String| Error::UnexpectedOutput {
        program: "gh",
        command: "pr view".to_owned(),
        detail,
    };
    let url = parse_url(&found.url)
        .filter(|url| url.number == found.number)
        .ok_or_else(|| unexpected(format!("unrecognized pull request URL {}", found.url)))?;
    let state = match found.state.as_str() {
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        other => return Err(unexpected(format!("unknown pull request state {other}"))),
    };
    let head_repository = match (found.head_repository, found.head_repository_owner) {
        (Some(repository), Some(owner)) => Some(Repository {
            host: url.host.clone(),
            owner: owner.login,
            name: repository.name,
        }),
        _ => None,
    };
    Ok(Some(BranchPullRequest {
        number: found.number,
        url: found.url,
        state,
        base_branch: found.base_ref_name,
        base_repository: Repository {
            host: url.host,
            owner: url.owner,
            name: url.repo,
        },
        head_branch: found.head_ref_name,
        head_repository,
        author: found.author.map(|author| author.login),
    }))
}

/// Runs `gh pr view [<spec>] --json <fields>` in the repository's working tree, or in its git
/// directory when it is bare, and kills it once it has run for `timeout`; without `spec`, `gh`
/// looks at the branch checked out there.
///
/// `gh` finds the repository from where it runs, so running it anywhere else would ask about
/// whatever repository the process happens to be in.
fn pr_view<T: DeserializeOwned>(
    repo: &Repo,
    spec: Option<&str>,
    fields: &str,
    timeout: Duration,
) -> Result<T, Error> {
    let args: Vec<&str> = ["pr", "view"]
        .into_iter()
        .chain(spec)
        .chain(["--json", fields])
        .collect();
    let stdout = run_gh(repo, &args, timeout)?;
    serde_json::from_slice(&stdout).map_err(|err| Error::UnexpectedOutput {
        program: "gh",
        command: "pr view".to_owned(),
        detail: err.to_string(),
    })
}

/// Runs `gh` with `args` in the repository, as [`pr_view`] does, and returns its standard
/// output; kills it once it has run for `timeout`.
fn run_gh(repo: &Repo, args: &[&str], timeout: Duration) -> Result<Vec<u8>, Error> {
    let shown = args.join(" ");
    let mut command = Command::new("gh");
    command.current_dir(repo.work_tree().unwrap_or_else(|| repo.git_dir()));
    repo.apply_env(&mut command);
    repo.prepare_remote(&mut command);
    // Detached from the console, gh would start the git it runs on a console window of its own
    // each time; on a console without a window, they share that one, and gh asks nothing on it.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        /// The process creation flag that starts a console program on a console of its own
        /// that shows no window, replacing the detachment `prepare_remote` asks for.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            program: "gh",
            args: shown.clone(),
            source,
        })?;
    // Both pipes drain on threads of their own, so a full pipe never stalls `gh`.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let spawn_error = |source| Error::Spawn {
        program: "gh",
        args: shown.clone(),
        source,
    };
    let status = loop {
        if let Some(status) = child.try_wait().map_err(spawn_error)? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(Error::TimedOut {
                program: "gh",
                args: shown,
                after: timeout,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::CommandFailed {
            program: "gh",
            args: shown,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        });
    }
    Ok(stdout)
}

/// Reads `pipe` to its end on a thread of its own.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes).ok();
        }
        bytes
    })
}

/// Temporary refs that receive a fetch, deleted when dropped.
pub(crate) struct TemporaryRefs<'a> {
    repo: &'a Repo,
    pub head: String,
    pub target: String,
}

impl<'a> TemporaryRefs<'a> {
    pub(crate) fn new(repo: &'a Repo) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let id = format!("{}-{nanos}", std::process::id());
        Self {
            repo,
            head: format!("refs/code-review/tmp/{id}-head"),
            target: format!("refs/code-review/tmp/{id}-target"),
        }
    }
}

impl Drop for TemporaryRefs<'_> {
    fn drop(&mut self) {
        for name in [&self.head, &self.target] {
            if let Err(err) = self.repo.delete_ref(name) {
                // A leftover temporary ref only keeps objects alive.
                drop(err);
            }
        }
    }
}

/// Fetches a pull request's head and its target branch into temporary refs and returns the
/// fetched commits; identity comes from the fetched objects, never an earlier observation.
pub(crate) fn fetch<'a>(
    repo: &'a Repo,
    url: &PullRequestUrl,
    base_ref_name: &str,
) -> Result<FetchedPullRequest<'a>, Error> {
    let remote = remote_for(repo, url)?;
    let refs = TemporaryRefs::new(repo);
    repo.fetch(
        &remote,
        &[
            format!("+refs/pull/{}/head:{}", url.number, refs.head),
            format!("+refs/heads/{base_ref_name}:{}", refs.target),
        ],
    )?;
    Ok(FetchedPullRequest {
        head: repo.resolve_commit(&refs.head)?,
        target: repo.resolve_commit(&refs.target)?,
        remote,
        refs,
    })
}

/// The commits fetched for a pull request, kept alive by their temporary refs until pinned.
pub(crate) struct FetchedPullRequest<'a> {
    pub head: ObjectId,
    pub target: ObjectId,
    /// The remote name or URL the commits came from, for deepening a shallow clone.
    pub remote: String,
    #[expect(
        dead_code,
        reason = "the temporary refs keep the fetched objects alive until they are pinned"
    )]
    pub refs: TemporaryRefs<'a>,
}

/// Finds a configured remote for the pull request's repository, or falls back to its URL.
fn remote_for(repo: &Repo, url: &PullRequestUrl) -> Result<String, Error> {
    Ok(matching_remote(repo, &url.host, &url.owner, &url.repo)?
        .unwrap_or_else(|| format!("https://{}/{}/{}.git", url.host, url.owner, url.repo)))
}

/// Returns the name of the configured remote whose URL is the repository `owner/name` on
/// `host`, if one is: a network URL whose host is exactly `host`, and whose path ends in
/// `owner/name`, with or without `.git`.
pub(crate) fn matching_remote(
    repo: &Repo,
    host: &str,
    owner: &str,
    name: &str,
) -> Result<Option<String>, Error> {
    let wanted = format!("{owner}/{name}").to_lowercase();
    // A URL as configured counts as well as as rewritten, so a remote whose `insteadOf`
    // shorthand names the forge matches, and so does one rewritten to a mirror of it.
    let remotes = repo
        .remotes()?
        .into_iter()
        .chain(repo.configured_remote_urls()?);
    for remote in remotes {
        let Some(location) = RemoteLocation::of(&remote.url) else {
            continue;
        };
        let path = location.path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path).to_lowercase();
        let named = path == wanted || path.ends_with(&format!("/{wanted}"));
        if canonical_host(location.host) == canonical_host(host) && named {
            return Ok(Some(remote.name));
        }
    }
    Ok(None)
}

/// Where a remote's URL points: its host and the path on it.
#[derive(Debug, PartialEq, Eq)]
struct RemoteLocation<'a> {
    host: &'a str,
    path: &'a str,
}

/// `host` as a forge is known by: `ssh.github.com`, GitHub's SSH over port 443, is
/// `github.com`; any other host is itself, in lowercase.
fn canonical_host(host: &str) -> String {
    let host = host.to_lowercase();
    if host == "ssh.github.com" {
        "github.com".to_owned()
    } else {
        host
    }
}

impl<'a> RemoteLocation<'a> {
    /// Parses a network URL, `https://`, `http://`, `ssh://` or `git://` with
    /// `[user@]host[:port]/path`, or git's scp-like `[user@]host:path`; `None` for a local path
    /// or a `file://` URL, which no forge serves.
    fn of(url: &'a str) -> Option<Self> {
        let (authority, path) = match url.split_once("://") {
            Some((scheme, rest)) => {
                let network = ["https", "http", "ssh", "git", "git+ssh", "ssh+git"]
                    .iter()
                    .any(|known| scheme.eq_ignore_ascii_case(known));
                if !network {
                    return None;
                }
                rest.split_once('/')?
            }
            // A colon before any slash makes it scp-like; a path may hold one after a slash.
            None => url
                .split_once(':')
                .filter(|(host, _)| !host.contains('/'))?,
        };
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = match host.strip_prefix('[') {
            // An IPv6 address, which holds colons of its own.
            Some(bracketed) => bracketed.split_once(']').map(|(address, _)| address)?,
            None => host.split_once(':').map_or(host, |(host, _)| host),
        };
        (!host.is_empty()).then_some(Self { host, path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote matches a pull request's repository only on its exact host, not on one that
    /// merely contains it.
    #[test]
    fn a_remote_matches_only_on_its_exact_host() -> color_eyre::eyre::Result<()> {
        let repo = crate::testing::committed(&[("a.txt", "a\n")])?;
        crate::testing::git(
            repo.path(),
            &[
                "remote",
                "add",
                "lookalike",
                "https://github.com.evil.example/owner/repo.git",
            ],
        )?;
        let git = Repo::discover(repo.path())?;
        assert_eq!(matching_remote(&git, "github.com", "owner", "repo")?, None);
        crate::testing::git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:Owner/Repo.git"],
        )?;
        assert_eq!(
            matching_remote(&git, "github.com", "owner", "repo")?.as_deref(),
            Some("origin")
        );

        // GitHub's SSH over port 443 is GitHub.
        let other = crate::testing::committed(&[("a.txt", "a\n")])?;
        crate::testing::git(
            other.path(),
            &[
                "remote",
                "add",
                "origin",
                "ssh://git@ssh.github.com:443/owner/repo.git",
            ],
        )?;
        let other_git = Repo::discover(other.path())?;
        assert_eq!(
            matching_remote(&other_git, "github.com", "owner", "repo")?.as_deref(),
            Some("origin")
        );
        Ok(())
    }

    /// A remote's host is read exactly, whatever the URL's form, and a local path has none.
    #[test]
    fn remote_urls_name_their_host() {
        let location = |host, path| Some(RemoteLocation { host, path });
        assert_eq!(
            RemoteLocation::of("https://github.com/owner/repo.git"),
            location("github.com", "owner/repo.git")
        );
        assert_eq!(
            RemoteLocation::of("ssh://git@github.com:22/owner/repo"),
            location("github.com", "owner/repo")
        );
        assert_eq!(
            RemoteLocation::of("git@github.com:owner/repo.git"),
            location("github.com", "owner/repo.git")
        );
        assert_eq!(
            RemoteLocation::of("https://user@github.com.evil.example/owner/repo"),
            location("github.com.evil.example", "owner/repo")
        );
        assert_eq!(
            RemoteLocation::of("/srv/git/github.com/owner/repo.git"),
            None
        );
        assert_eq!(RemoteLocation::of("file:///srv/git/owner/repo.git"), None);
        assert_eq!(
            RemoteLocation::of("file://localhost/srv/git/owner/repo.git"),
            None
        );
    }

    #[test]
    fn parses_pull_request_urls() {
        let url = parse_url("https://github.com/owner/repo/pull/123/files").unwrap();
        assert_eq!(url.owner, "owner");
        assert_eq!(url.repo, "repo");
        assert_eq!(url.number, 123);
        assert!(parse_url("https://github.com/owner/repo/issues/1").is_none());
    }
}
