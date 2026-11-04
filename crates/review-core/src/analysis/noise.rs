//! Classification of generated, vendored and lockfile changes, which are collapsed by default but
//! still need a mark.

use serde::{Deserialize, Serialize};

/// Why a file counts as noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseCategory {
    /// Marked `linguist-generated` or `-diff` in `.gitattributes`, or carrying `@generated` or
    /// `DO NOT EDIT` in its first lines.
    Generated,
    /// A known package manager lockfile.
    Lockfile,
    /// A test snapshot (`*.snap`).
    Snapshot,
}

impl NoiseCategory {
    /// Returns the category's name as used on the command line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Lockfile => "lockfile",
            Self::Snapshot => "snapshot",
        }
    }
}

const LOCKFILES: [&str; 7] = [
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "go.sum",
    "poetry.lock",
    "uv.lock",
];

/// How many leading lines are searched for a generated-file marker.
const MARKER_LINES: usize = 5;

/// Classifies one file from its path, its gitattributes and the start of its content.
pub(super) fn classify(
    path: &str,
    generated_attribute: bool,
    head_text: Option<&str>,
) -> Option<NoiseCategory> {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if LOCKFILES.contains(&file_name) {
        return Some(NoiseCategory::Lockfile);
    }
    let is_snapshot = std::path::Path::new(file_name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("snap"));
    if is_snapshot {
        return Some(NoiseCategory::Snapshot);
    }
    let marked = head_text.is_some_and(|text| {
        text.lines()
            .take(MARKER_LINES)
            .any(|line| line.contains("@generated") || line.contains("DO NOT EDIT"))
    });
    (generated_attribute || marked).then_some(NoiseCategory::Generated)
}
