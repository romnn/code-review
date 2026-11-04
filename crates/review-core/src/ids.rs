//! Identifiers for snapshots, reviews, revisions, units and moves.
//!
//! Unit and move IDs carry the revision they belong to, so an ID from one revision can never be
//! used to address a unit of another: every lookup checks the revision first.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An identifier that failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseIdError {
    /// The text is not a full lowercase hexadecimal SHA-1 or SHA-256 object ID.
    #[error("invalid object id {0:?}")]
    ObjectId(String),
    /// The text is not a revision number of at least 1.
    #[error("invalid revision number {0:?}")]
    Revision(String),
    /// The text is not a unit ID of the form `u<revision>-<index>`.
    #[error("invalid unit id {0:?}, expected the form u3-0007")]
    Unit(String),
    /// The text is not a move ID of the form `m<revision>-<index>`.
    #[error("invalid move id {0:?}, expected the form m3-0002")]
    Move(String),
    /// The review key is empty or contains a line break.
    #[error("invalid review key {0:?}")]
    ReviewKey(String),
}

/// A full hexadecimal git object ID (SHA-1 or SHA-256), always lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectId(String);

impl ObjectId {
    /// Parses a full object ID.
    ///
    /// # Errors
    ///
    /// Returns [`ParseIdError::ObjectId`] unless `text` is 40 or 64 lowercase hex digits.
    pub fn parse(text: &str) -> Result<Self, ParseIdError> {
        let valid = matches!(text.len(), 40 | 64)
            && text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(ParseIdError::ObjectId(text.to_owned()))
        }
    }

    /// Returns the full hexadecimal form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the first 12 hex digits, for display only.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
    }

    /// Returns whether this is git's all-zero placeholder ID for a missing side.
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.0.bytes().all(|byte| byte == b'0')
    }
}

impl TryFrom<String> for ObjectId {
    type Error = ParseIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ObjectId> for String {
    fn from(oid: ObjectId) -> Self {
        oid.0
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The stable key of a review, such as `github.com/owner/repo#123`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReviewKey(String);

impl ReviewKey {
    /// Parses a review key.
    ///
    /// # Errors
    ///
    /// Returns [`ParseIdError::ReviewKey`] when `text` is empty or contains a line break.
    pub fn parse(text: &str) -> Result<Self, ParseIdError> {
        if text.is_empty() || text.contains(['\n', '\r', '\0']) {
            return Err(ParseIdError::ReviewKey(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }

    /// Returns the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Encodes the key as one path component and git ref component.
    ///
    /// Every byte outside `[A-Za-z0-9_-]` becomes `%XX`, so distinct keys never collide and the
    /// result contains no `/`, no `..` and no `.lock` suffix.
    #[must_use]
    pub fn encoded(&self) -> String {
        encode_component(&self.0)
    }
}

impl TryFrom<String> for ReviewKey {
    type Error = ParseIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ReviewKey> for String {
    fn from(key: ReviewKey) -> Self {
        key.0
    }
}

impl fmt::Display for ReviewKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Percent-encodes `text` into a single safe path or ref component.
pub(crate) fn encode_component(text: &str) -> String {
    use fmt::Write as _;

    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            encoded.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            write!(encoded, "%{byte:02X}").ok();
        }
    }
    encoded
}

/// The number of a revision within its review, starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionNumber(NonZeroU32);

impl RevisionNumber {
    /// The first revision of every review.
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    /// Returns revision `number`, or `None` for 0.
    #[must_use]
    pub fn new(number: u32) -> Option<Self> {
        NonZeroU32::new(number).map(Self)
    }

    /// Returns the number as an integer.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Returns the next revision number, or `None` on overflow.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    /// Returns the previous revision number, or `None` for the first revision.
    #[must_use]
    pub fn previous(self) -> Option<Self> {
        NonZeroU32::new(self.0.get() - 1).map(Self)
    }

    /// Returns the zero-based position of this revision in a list of all revisions.
    pub(crate) fn index(self) -> usize {
        self.0.get() as usize - 1
    }

    /// Returns the revision at zero-based position `index`.
    pub(crate) fn from_index(index: usize) -> Option<Self> {
        u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .and_then(NonZeroU32::new)
            .map(Self)
    }
}

impl FromStr for RevisionNumber {
    type Err = ParseIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse::<NonZeroU32>()
            .map(Self)
            .map_err(|_| ParseIdError::Revision(text.to_owned()))
    }
}

impl fmt::Display for RevisionNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The position of a unit within one revision's analysis.
///
/// An index means nothing without its revision; public APIs take a [`UnitId`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnitIndex(u32);

impl UnitIndex {
    pub(crate) fn new(index: usize) -> Self {
        Self(u32::try_from(index).unwrap_or(u32::MAX))
    }

    /// Returns the zero-based position.
    #[must_use]
    pub fn get(self) -> usize {
        self.0 as usize
    }
}

/// A revision-local review unit ID, written `u<revision>-<index>` such as `u3-0007`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnitId {
    revision: RevisionNumber,
    index: UnitIndex,
}

impl UnitId {
    pub(crate) fn new(revision: RevisionNumber, index: UnitIndex) -> Self {
        Self { revision, index }
    }

    /// Returns the revision this unit belongs to.
    #[must_use]
    pub fn revision(self) -> RevisionNumber {
        self.revision
    }

    /// Returns the unit's position within its revision.
    #[must_use]
    pub fn index(self) -> UnitIndex {
        self.index
    }
}

impl FromStr for UnitId {
    type Err = ParseIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let LocalId { revision, index } =
            parse_local_id(text, 'u').ok_or_else(|| ParseIdError::Unit(text.to_owned()))?;
        Ok(Self {
            revision,
            index: UnitIndex(index),
        })
    }
}

impl fmt::Display for UnitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "u{}-{:04}", self.revision, self.index.get())
    }
}

impl Serialize for UnitId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UnitId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A revision-local move ID, written `m<revision>-<index>` such as `m3-0002`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MoveId {
    revision: RevisionNumber,
    index: u32,
}

impl MoveId {
    pub(crate) fn new(revision: RevisionNumber, index: usize) -> Self {
        Self {
            revision,
            index: u32::try_from(index).unwrap_or(u32::MAX),
        }
    }

    /// Returns the revision this move belongs to.
    #[must_use]
    pub fn revision(self) -> RevisionNumber {
        self.revision
    }

    /// Returns the move's position within its revision.
    #[must_use]
    pub fn index(self) -> usize {
        self.index as usize
    }
}

impl FromStr for MoveId {
    type Err = ParseIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let LocalId { revision, index } =
            parse_local_id(text, 'm').ok_or_else(|| ParseIdError::Move(text.to_owned()))?;
        Ok(Self { revision, index })
    }
}

impl fmt::Display for MoveId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "m{}-{:04}", self.revision, self.index)
    }
}

impl Serialize for MoveId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The parts of a revision-local ID.
struct LocalId {
    revision: RevisionNumber,
    index: u32,
}

/// Parses `<prefix><revision>-<index>`.
fn parse_local_id(text: &str, prefix: char) -> Option<LocalId> {
    let rest = text.strip_prefix(prefix)?;
    let (revision, index) = rest.split_once('-')?;
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(revision) || !all_digits(index) {
        return None;
    }
    Some(LocalId {
        revision: revision.parse().ok()?,
        index: index.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_ids_round_trip_and_reject_garbage() {
        let id: UnitId = "u3-0007".parse().unwrap();
        assert_eq!(id.revision().get(), 3);
        assert_eq!(id.index().get(), 7);
        assert_eq!(id.to_string(), "u3-0007");
        assert!("u0-0001".parse::<UnitId>().is_err());
        assert!("m3-0001".parse::<UnitId>().is_err());
        assert!("u3-".parse::<UnitId>().is_err());
        assert!("u3-+1".parse::<UnitId>().is_err());
    }

    /// Unit and move indices are both zero-based, so the first of each in a revision is `-0000`.
    #[test]
    fn unit_and_move_ids_count_from_zero() {
        assert_eq!(
            UnitId::new(RevisionNumber::FIRST, UnitIndex::new(0)).to_string(),
            "u1-0000"
        );
        assert_eq!(MoveId::new(RevisionNumber::FIRST, 0).to_string(), "m1-0000");
        assert_eq!("m1-0000".parse::<MoveId>().unwrap().index(), 0);
    }

    #[test]
    fn review_keys_encode_to_one_safe_component() {
        let key = ReviewKey::parse("github.com/owner/repo#123").unwrap();
        assert_eq!(key.encoded(), "github%2Ecom%2Fowner%2Frepo%23123");
        let local = ReviewKey::parse("/a/b:main..x.lock").unwrap();
        assert!(!local.encoded().contains(['/', '.']));
    }
}
