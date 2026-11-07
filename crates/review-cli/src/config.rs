//! The `[review]` section of the configuration file `cr` shares with the apps.
//!
//! `cr` reads only that section and ignores the rest, which configures the apps' look and keys,
//! so a setting `cr` does not know never stops it.

use std::path::Path;

use color_eyre::eyre::{self, WrapErr as _};
use review_core::ReviewPolicy;

/// The policy when the file does not say otherwise: proven formatting-only units are approved
/// automatically, as in the apps.
pub const UNCONFIGURED: ReviewPolicy = ReviewPolicy {
    auto_approve_formatting: true,
};

/// Reads the review policy from the configuration file at `path`; a missing file, section or
/// key keeps automatic approval on, as the apps do.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read, is not TOML, or its
/// `[review] auto_approve_formatting` is not `true` or `false`.
pub fn review_policy(path: &Path) -> eyre::Result<ReviewPolicy> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(UNCONFIGURED),
        Err(err) => {
            return Err(err).wrap_err_with(|| format!("failed to read {}", path.display()));
        }
    };
    parse_review_policy(&text).wrap_err_with(|| format!("failed to read {}", path.display()))
}

/// Reads the review policy from the text of a configuration file.
fn parse_review_policy(text: &str) -> eyre::Result<ReviewPolicy> {
    let file: toml::Table = toml::from_str(text).map_err(|err| eyre::eyre!("{}", err.message()))?;
    let Some(review) = file.get("review") else {
        return Ok(UNCONFIGURED);
    };
    let review = review
        .as_table()
        .ok_or_else(|| eyre::eyre!("[review] is not a table"))?;
    match review.get("auto_approve_formatting") {
        None => Ok(UNCONFIGURED),
        Some(&toml::Value::Boolean(auto_approve_formatting)) => Ok(ReviewPolicy {
            auto_approve_formatting,
        }),
        Some(_) => eyre::bail!("[review] auto_approve_formatting is neither true nor false"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `[review] auto_approve_formatting` matters: other sections, even ones the apps
    /// would reject, leave it alone, and an absent setting keeps automatic approval on.
    #[test]
    fn reads_only_the_review_section() {
        let off = ReviewPolicy::MARKS_ONLY;
        assert_eq!(parse_review_policy("").unwrap(), UNCONFIGURED);
        assert_eq!(
            parse_review_policy("[theme]\nname = \"nightfox\"\n").unwrap(),
            UNCONFIGURED
        );
        assert_eq!(
            parse_review_policy("[review]\nauto_approve_formatting = false\n").unwrap(),
            off
        );
        assert_eq!(
            parse_review_policy(
                "[future]\nunknown = 1\n[review]\nauto_approve_formatting = false\n"
            )
            .unwrap(),
            off
        );
        assert_eq!(
            parse_review_policy("[review]\nauto_approve_formatting = true\n").unwrap(),
            UNCONFIGURED
        );
    }

    /// A value `cr` cannot read is an error rather than silently a policy.
    #[test]
    fn rejects_what_it_cannot_read() {
        assert!(parse_review_policy("[review]\nauto_approve_formatting = \"no\"\n").is_err());
        assert!(parse_review_policy("review = 1\n").is_err());
        assert!(parse_review_policy("[review\n").is_err());
    }

    /// A missing file keeps automatic approval on, as in the apps.
    #[test]
    fn a_missing_file_keeps_the_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            review_policy(&dir.path().join("config.toml")).unwrap(),
            UNCONFIGURED
        );
    }
}
