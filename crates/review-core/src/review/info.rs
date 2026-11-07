//! What a review is about: the pull request's metadata, or the commits of a local range,
//! fetched at refresh and stored with the review.

use serde::{Deserialize, Serialize};

/// A pull request's metadata, as the forge reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// The pull request number.
    pub number: u64,
    /// The title.
    pub title: String,
    /// The description, in Markdown.
    pub body: String,
    /// The author's login.
    pub author: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    /// The branch it merges into.
    pub base_ref: String,
    /// The branch it merges from.
    pub head_ref: String,
    /// Label names.
    pub labels: Vec<String>,
    /// When it was opened, in RFC 3339.
    pub created_at: String,
    /// When it last changed, in RFC 3339.
    pub updated_at: String,
    /// Its web page.
    pub url: String,
}

/// One commit of a reviewed range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// The full commit ID.
    pub id: String,
    /// The first line of the message.
    pub subject: String,
    /// The rest of the message.
    pub body: String,
    /// The author's name.
    pub author: String,
    /// The author date, in RFC 3339.
    pub date: String,
}

/// What a review is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewInfo {
    /// A pull request.
    PullRequest(Box<PullRequest>),
    /// A local range or the working tree: its commits, newest first.
    Commits {
        /// The commits.
        commits: Vec<Commit>,
    },
}

/// `gh pr view --json` output for the fields the overview shows.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhPullRequest {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    author: GhLogin,
    state: String,
    base_ref_name: String,
    head_ref_name: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    created_at: String,
    updated_at: String,
    url: String,
}

#[derive(Deserialize)]
struct GhLogin {
    login: String,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

impl From<GhPullRequest> for PullRequest {
    fn from(pr: GhPullRequest) -> Self {
        Self {
            number: pr.number,
            title: pr.title,
            body: pr.body,
            author: pr.author.login,
            state: pr.state,
            base_ref: pr.base_ref_name,
            head_ref: pr.head_ref_name,
            labels: pr.labels.into_iter().map(|label| label.name).collect(),
            created_at: pr.created_at,
            updated_at: pr.updated_at,
            url: pr.url,
        }
    }
}

/// The fields `gh pr view --json` is asked for.
pub(crate) const GH_FIELDS: &str =
    "number,title,body,author,state,baseRefName,headRefName,labels,createdAt,updatedAt,url";

/// Parses `git log` output written with [`LOG_FORMAT`]: records separated by `\x1e`, fields by
/// `\x1f`.
pub(crate) fn parse_log(output: &str) -> Vec<Commit> {
    output
        .split('\u{1e}')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\u{1f}');
            let id = fields.next()?.trim();
            if id.is_empty() {
                return None;
            }
            Some(Commit {
                id: id.to_owned(),
                author: fields.next()?.to_owned(),
                date: fields.next()?.to_owned(),
                subject: fields.next()?.to_owned(),
                body: fields.next().unwrap_or_default().trim().to_owned(),
            })
        })
        .collect()
}

/// The `git log --format` that [`parse_log`] reads.
pub(crate) const LOG_FORMAT: &str = "--format=%H%x1f%an%x1f%aI%x1f%s%x1f%b%x1e";

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    #[test]
    fn gh_output_and_git_log_parse() {
        let gh: GhPullRequest = serde_json::from_str(
            r#"{"number":7,"title":"T","body":"B","author":{"login":"ana"},"state":"OPEN",
                "baseRefName":"main","headRefName":"f","labels":[{"name":"bug"}],
                "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z",
                "url":"https://github.com/o/r/pull/7"}"#,
        )
        .unwrap();
        let pr = PullRequest::from(gh);
        assert_eq!(
            (pr.number, pr.author.as_str(), pr.labels.as_slice()),
            (7, "ana", &["bug".to_owned()][..])
        );

        let log = indoc! {"
            abc\u{1f}Ana\u{1f}2026-01-01T00:00:00+00:00\u{1f}Fix it\u{1f}Because.
            \u{1e}
            def\u{1f}Bo\u{1f}2026-01-02T00:00:00+00:00\u{1f}Add\u{1f}\u{1e}
        "};
        let commits = parse_log(log);
        assert_eq!(commits.len(), 2);
        assert_eq!(
            (commits[0].subject.as_str(), commits[0].body.as_str()),
            ("Fix it", "Because.")
        );
        assert_eq!(commits[1].body, "");
    }
}
