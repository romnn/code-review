//! chezmoi's source names: the attributes chezmoi reads from the names in its source directory
//! and the target names they leave, so a dotfile chezmoi manages, such as `dot_gitconfig`, is
//! highlighted as the file it becomes, `.gitconfig`.
//!
//! The prefixes come in the order chezmoi reads them, each at most once, as its reference on
//! [source state attributes][attributes] lists them; `literal_` ends them, and the `.literal`
//! suffix is dropped.
//!
//! [attributes]: https://www.chezmoi.io/reference/source-state-attributes/

/// The suffix of chezmoi's templates, whose names alone among templates carry its attributes.
pub(super) const TEMPLATE_SUFFIX: &str = ".tmpl";

/// What chezmoi makes of a source file, which decides what its content is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A file whose content is its target's, such as `dot_bashrc`.
    Target,
    /// A symbolic link, whose content is the path it points to.
    Symlink,
    /// A script that modifies its target, unless it is a modify template.
    Modify,
    /// A script chezmoi runs, named for what it runs.
    Run,
}

/// A source file's kind and its target's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Source {
    pub(super) kind: Kind,
    /// The path chezmoi writes, such as `.config/git/config`, or the script's name without its
    /// attributes for a script.
    pub(super) target: String,
}

/// The attributes of a directory's name, each group at most once and in this order.
const DIRECTORY: &[&[&str]] = &[
    &["remove_"],
    &["external_"],
    &["exact_"],
    &["private_"],
    &["readonly_"],
];

/// The attributes of a regular file's name, and of a `create_` file's after its prefix.
const FILE: &[&[&str]] = &[
    &["encrypted_"],
    &["private_"],
    &["readonly_"],
    &["empty_"],
    &["executable_"],
];

/// The attributes of a `modify_` script's name after its prefix.
const MODIFY: &[&[&str]] = &[
    &["encrypted_"],
    &["private_"],
    &["readonly_"],
    &["executable_"],
];

/// The attributes of a `run_` script's name after its prefix.
const RUN: &[&[&str]] = &[&["once_", "onchange_"], &["before_", "after_"]];

/// The kind and target of the source file at `path`, or `None` when no name in it carries a
/// chezmoi attribute.
pub(super) fn source(path: &str) -> Option<Source> {
    let (directories, file) = match path.rsplit_once('/') {
        Some((directories, file)) => (Some(directories), file),
        None => (None, path),
    };
    let mut changed = false;
    let mut target = String::new();
    for directory in directories
        .into_iter()
        .flat_map(|directories| directories.split('/'))
    {
        let name = strip(directory, DIRECTORY, true);
        changed |= name != directory;
        target.push_str(&name);
        target.push('/');
    }
    let (kind, rest, groups, dot) = if let Some(rest) = file.strip_prefix("symlink_") {
        (Kind::Symlink, rest, &[][..], true)
    } else if let Some(rest) = file.strip_prefix("modify_") {
        (Kind::Modify, rest, MODIFY, true)
    } else if let Some(rest) = file.strip_prefix("run_") {
        (Kind::Run, rest, RUN, false)
    } else if let Some(rest) = file.strip_prefix("create_") {
        (Kind::Target, rest, FILE, true)
    } else if let Some(rest) = file.strip_prefix("remove_") {
        (Kind::Target, rest, &[][..], true)
    } else {
        (Kind::Target, file, FILE, true)
    };
    let name = strip(rest, groups, dot);
    let name = name.strip_suffix(".literal").unwrap_or(&name);
    changed |= name != file;
    target.push_str(name);
    changed.then_some(Source { kind, target })
}

/// `name` without the attributes of `groups`, each group's first that matches, and with a
/// `dot_` prefix after them as a `.` when `dot`; a `literal_` prefix ends the attributes.
fn strip(name: &str, groups: &[&[&str]], dot: bool) -> String {
    let mut rest = name;
    for group in groups {
        if let Some(literal) = rest.strip_prefix("literal_") {
            return literal.to_owned();
        }
        if let Some(stripped) = group.iter().find_map(|prefix| rest.strip_prefix(prefix)) {
            rest = stripped;
        }
    }
    if let Some(literal) = rest.strip_prefix("literal_") {
        return literal.to_owned();
    }
    match rest.strip_prefix("dot_") {
        Some(stripped) if dot => format!(".{stripped}"),
        _ => rest.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source at `path`, as a kind and a target.
    fn read(path: &str) -> Option<(Kind, String)> {
        source(path).map(|Source { kind, target }| (kind, target))
    }

    /// Each kind of source name leaves the target chezmoi writes, with every attribute and
    /// `dot_` read in chezmoi's order.
    #[test]
    fn source_names_leave_their_targets() {
        // Files
        assert_eq!(
            read("dot_gitconfig"),
            Some((Kind::Target, ".gitconfig".to_owned()))
        );
        assert_eq!(
            read("private_dot_config/exact_nvim/executable_init.lua"),
            Some((Kind::Target, ".config/nvim/init.lua".to_owned()))
        );
        assert_eq!(
            read("create_private_dot_netrc"),
            Some((Kind::Target, ".netrc".to_owned()))
        );
        // Scripts and links
        assert_eq!(
            read("modify_private_dot_claude.json"),
            Some((Kind::Modify, ".claude.json".to_owned()))
        );
        assert_eq!(
            read("run_onchange_after_42_incus.sh"),
            Some((Kind::Run, "42_incus.sh".to_owned()))
        );
        assert_eq!(
            read("symlink_dot_vimrc"),
            Some((Kind::Symlink, ".vimrc".to_owned()))
        );
        // `literal_` and `.literal` keep what follows and what precedes them.
        assert_eq!(
            read("literal_dot_keep.literal"),
            Some((Kind::Target, "dot_keep".to_owned()))
        );
        assert_eq!(
            read("private_literal_run_me"),
            Some((Kind::Target, "run_me".to_owned()))
        );
        // Names without attributes are no chezmoi source.
        assert_eq!(read("src/main.rs"), None);
        assert_eq!(read("dotfiles"), None);
    }
}
