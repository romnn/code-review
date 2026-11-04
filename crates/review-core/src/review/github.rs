//! GitHub pull request metadata through the `gh` CLI, and fetching into temporary refs.

use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::error::Error;
use crate::git::Repo;
use crate::ids::ObjectId;

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
    let mut command = Command::new("gh");
    if let Some(work_tree) = repo.work_tree() {
        command.current_dir(work_tree);
    }
    let args = ["pr", "view", spec, "--json", "url,baseRefName"];
    let output = command
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Spawn {
            program: "gh",
            args: args.join(" "),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "gh",
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|err| Error::UnexpectedOutput {
        program: "gh",
        command: "pr view".to_owned(),
        detail: err.to_string(),
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
    let wanted = format!("{}/{}", url.owner, url.repo).to_lowercase();
    for remote in repo.remotes()? {
        let normalized = remote
            .url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .replace(':', "/")
            .to_lowercase();
        if normalized.contains(&url.host.to_lowercase())
            && normalized.ends_with(&format!("/{wanted}"))
        {
            return Ok(remote.name);
        }
    }
    Ok(format!(
        "https://{}/{}/{}.git",
        url.host, url.owner, url.repo
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pull_request_urls() {
        let url = parse_url("https://github.com/owner/repo/pull/123/files").unwrap();
        assert_eq!(url.owner, "owner");
        assert_eq!(url.repo, "repo");
        assert_eq!(url.number, 123);
        assert!(parse_url("https://github.com/owner/repo/issues/1").is_none());
    }
}
