//! References to units, moves and revisions on the command line, which name the review they
//! belong to with its tag, as in `u1-0000@3fa2b1c0` or `--revision 3@3fa2b1c0`.
//!
//! IDs are local to a review, so a bare `u1-0000` means a unit of whichever review a command
//! happens to act on. A command that changes review state therefore takes a bare ID only when
//! `--review` names the review, and refuses an ID whose tag is another review's.

use std::fmt;
use std::str::FromStr;

use color_eyre::eyre;
use review_core::{ParseIdError, ReviewKey, ReviewTag};

/// An ID as given, with the tag of the review it belongs to when it names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference<T> {
    id: T,
    tag: Option<ReviewTag>,
}

impl<T: Copy + fmt::Display> Reference<T> {
    /// Returns the ID of a reference that changes review state, once it is known to belong to
    /// the review `key`: its tag is that review's, or it has none and `named` says `--review`
    /// named the review.
    pub fn resolve(&self, key: &ReviewKey, named: bool) -> eyre::Result<T> {
        if self.tag.is_none() && !named {
            eyre::bail!(
                "{} does not say which review it belongs to; write it as {}@<tag>, with the tag \
                 `cr show` printed, or name the review with --review",
                self.id,
                self.id
            );
        }
        self.check(key)
    }

    /// Returns the ID of a reference that only reads, after checking that its tag, if it has
    /// one, is the review `key`'s.
    pub fn check(&self, key: &ReviewKey) -> eyre::Result<T> {
        let expected = key.tag();
        match self.tag {
            Some(tag) if tag != expected => eyre::bail!(
                "{self} belongs to another review than {key}, whose tag is {expected}; name \
                 its review with --review"
            ),
            _ => Ok(self.id),
        }
    }
}

impl<T: FromStr<Err = ParseIdError>> FromStr for Reference<T> {
    type Err = ParseIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(match text.split_once('@') {
            Some((id, tag)) => Self {
                id: id.parse()?,
                tag: Some(tag.parse()?),
            },
            None => Self {
                id: text.parse()?,
                tag: None,
            },
        })
    }
}

impl<T: fmt::Display> fmt::Display for Reference<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tag {
            Some(tag) => write!(formatter, "{}@{tag}", self.id),
            None => write!(formatter, "{}", self.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use review_core::{RevisionNumber, UnitId};

    use super::*;

    /// A reference with the review's own tag resolves whether or not `--review` named it; a
    /// bare one only when it did, and another review's tag never.
    #[test]
    fn references_resolve_only_in_their_own_review() -> eyre::Result<()> {
        let key = ReviewKey::parse("/repo:WORKTREE@0123456789abcdef")?;
        let other = ReviewKey::parse("github.com/owner/repo#1@/repo/.git")?;
        let tagged: Reference<UnitId> = format!("u1-0002@{}", key.tag()).parse()?;
        let bare: Reference<UnitId> = "u1-0002".parse()?;
        let foreign: Reference<UnitId> = format!("u1-0002@{}", other.tag()).parse()?;
        let unit: UnitId = "u1-0002".parse()?;

        // Qualified by the review's own tag
        assert_eq!(tagged.resolve(&key, false)?, unit);
        assert_eq!(tagged.resolve(&key, true)?, unit);

        // Bare
        assert!(bare.resolve(&key, false).is_err());
        assert_eq!(bare.resolve(&key, true)?, unit);
        assert_eq!(bare.check(&key)?, unit);

        // Another review's tag
        assert!(foreign.resolve(&key, false).is_err());
        assert!(foreign.resolve(&key, true).is_err());
        assert!(foreign.check(&key).is_err());
        Ok(())
    }

    #[test]
    fn revision_references_parse_with_and_without_a_tag() -> eyre::Result<()> {
        let tagged: Reference<RevisionNumber> = "3@3fa2b1c0".parse()?;
        assert_eq!(tagged.to_string(), "3@3fa2b1c0");
        let bare: Reference<RevisionNumber> = "3".parse()?;
        assert_eq!(bare.to_string(), "3");
        assert!("0@3fa2b1c0".parse::<Reference<RevisionNumber>>().is_err());
        assert!("3@3fa2".parse::<Reference<RevisionNumber>>().is_err());
        Ok(())
    }
}
