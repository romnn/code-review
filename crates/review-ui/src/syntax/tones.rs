//! Highlight classes, and how both engines' names for code map onto them: Helix's capture names
//! for the tree-house tier and the scopes of bat's syntaxes for the bat tier.

/// A highlight class; the palette gives each a color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Plain code, such as variables: the default text color.
    Plain,
    /// Keywords, such as `fn` and `return`.
    Keyword,
    /// Function and method names.
    Function,
    /// Type names, built-in or not.
    Type,
    /// String and character literals.
    String,
    /// Numbers, booleans and other constants.
    Constant,
    /// Comments, doc comments included.
    Comment,
    /// Function parameters.
    Parameter,
    /// Attributes and decorators.
    Attribute,
    /// Operators, such as `+` and `=>`.
    Operator,
    /// Brackets, delimiters and other punctuation.
    Punctuation,
    /// Fields and properties.
    Property,
    /// `self`, `this` and other special variables.
    Special,
    /// Markup tags, such as HTML and JSX elements.
    Tag,
    /// Escapes, regexes and other special string parts.
    Escape,
    /// Markup headings, drawn bold.
    Heading,
    /// Emphasized markup text, drawn italic.
    Emphasis,
    /// Strong markup text, drawn bold.
    Strong,
    /// Links and their targets in markup.
    Link,
    /// Code spans and code blocks in markup.
    Raw,
    /// List markers in markup.
    List,
    /// Quoted blocks in markup.
    Quote,
    /// Lines a patch adds.
    Inserted,
    /// Lines a patch removes.
    Deleted,
}

impl Tone {
    /// Every tone, in declaration order, which is also the order of [`Self::index`].
    pub const ALL: [Self; 24] = [
        Self::Plain,
        Self::Keyword,
        Self::Function,
        Self::Type,
        Self::String,
        Self::Constant,
        Self::Comment,
        Self::Parameter,
        Self::Attribute,
        Self::Operator,
        Self::Punctuation,
        Self::Property,
        Self::Special,
        Self::Tag,
        Self::Escape,
        Self::Heading,
        Self::Emphasis,
        Self::Strong,
        Self::Link,
        Self::Raw,
        Self::List,
        Self::Quote,
        Self::Inserted,
        Self::Deleted,
    ];

    /// The tone's position in [`Self::ALL`], which tree-house carries as its highlight id.
    pub(super) fn index(self) -> u32 {
        self as u32
    }

    /// The tone at `index` in [`Self::ALL`].
    pub(super) fn from_index(index: u32) -> Option<Self> {
        Self::ALL.get(usize::try_from(index).ok()?).copied()
    }
}

/// Helix capture names and the tones they take, matched by their longest known prefix.
///
/// [`Tone::Plain`] marks an explicit reset, such as `@variable` inside a string interpolation,
/// which paints the default color over what encloses it.
const CAPTURES: &[(&str, Tone)] = &[
    ("attribute", Tone::Attribute),
    ("comment", Tone::Comment),
    ("constant", Tone::Constant),
    ("constant.character", Tone::String),
    ("constant.character.escape", Tone::Escape),
    ("constructor", Tone::Type),
    ("diff.minus", Tone::Deleted),
    ("diff.plus", Tone::Inserted),
    ("function", Tone::Function),
    ("keyword", Tone::Keyword),
    ("label", Tone::Special),
    ("markup.bold", Tone::Strong),
    ("markup.heading", Tone::Heading),
    ("markup.italic", Tone::Emphasis),
    ("markup.link", Tone::Link),
    ("markup.list", Tone::List),
    ("markup.quote", Tone::Quote),
    ("markup.raw", Tone::Raw),
    ("none", Tone::Plain),
    ("operator", Tone::Operator),
    ("punctuation", Tone::Punctuation),
    ("special", Tone::Special),
    ("string", Tone::String),
    ("string.escape", Tone::Escape),
    ("string.regexp", Tone::Escape),
    ("string.special", Tone::Escape),
    ("string.special.symbol", Tone::Constant),
    ("tag", Tone::Tag),
    ("type", Tone::Type),
    ("variable", Tone::Plain),
    ("variable.builtin", Tone::Special),
    ("variable.other.member", Tone::Property),
    ("variable.parameter", Tone::Parameter),
];

/// The tone of a Helix capture name, or of a local definition's name without its
/// `local.definition.` prefix: the tone of its longest known prefix.
///
/// `None` leaves the node to whatever encloses it: helper captures, such as `@spell`,
/// `@nospell` and `@_name`, and names no tone covers, such as `@namespace` or `@embedded`.
pub(super) fn capture_tone(name: &str) -> Option<Tone> {
    if name.starts_with('_') || matches!(name, "spell" | "nospell") {
        return None;
    }
    longest_prefix(CAPTURES, name)
}

/// The scopes of bat's syntaxes and the tones they take, matched by their longest known prefix.
///
/// [`Tone::Plain`] marks code embedded in a string, such as an interpolation, whose own scopes
/// lie inside it.
const SCOPES: &[(&str, Tone)] = &[
    ("comment", Tone::Comment),
    ("constant", Tone::Constant),
    ("constant.character", Tone::String),
    ("constant.character.escape", Tone::Escape),
    ("entity.name.class", Tone::Type),
    ("entity.name.function", Tone::Function),
    ("entity.name.section", Tone::Heading),
    ("entity.name.tag", Tone::Tag),
    ("entity.name.type", Tone::Type),
    ("entity.other.attribute-name", Tone::Attribute),
    ("entity.other.inherited-class", Tone::Type),
    ("keyword", Tone::Keyword),
    ("keyword.operator", Tone::Operator),
    ("markup.bold", Tone::Strong),
    ("markup.deleted", Tone::Deleted),
    ("markup.heading", Tone::Heading),
    ("markup.inserted", Tone::Inserted),
    ("markup.italic", Tone::Emphasis),
    ("markup.list", Tone::List),
    ("markup.quote", Tone::Quote),
    ("markup.raw", Tone::Raw),
    ("markup.underline.link", Tone::Link),
    ("meta.embedded", Tone::Plain),
    ("meta.interpolation", Tone::Plain),
    ("punctuation", Tone::Punctuation),
    ("punctuation.definition.comment", Tone::Comment),
    ("punctuation.definition.list", Tone::List),
    ("punctuation.definition.string", Tone::String),
    ("storage", Tone::Keyword),
    ("string", Tone::String),
    ("string.regexp", Tone::Escape),
    ("support.class", Tone::Type),
    ("support.constant", Tone::Constant),
    ("support.function", Tone::Function),
    ("support.type", Tone::Type),
    ("variable.function", Tone::Function),
    ("variable.language", Tone::Special),
    ("variable.other.member", Tone::Property),
    ("variable.other.property", Tone::Property),
    ("variable.parameter", Tone::Parameter),
];

/// The tone of one scope of a bat syntax, such as `comment.line.double-slash.rust`, or `None`
/// when no known prefix covers it and the scope around it decides.
pub(super) fn scope_tone(scope: &str) -> Option<Tone> {
    longest_prefix(SCOPES, scope)
}

/// The tone of the longest prefix of `name` in `table` that ends at a `.` or at its end.
fn longest_prefix(table: &[(&str, Tone)], name: &str) -> Option<Tone> {
    table
        .iter()
        .filter(|(prefix, _)| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|&(_, tone)| tone)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture takes the tone of its most specific known prefix, helper captures take none,
    /// and an explicit reset is plain rather than nothing.
    #[test]
    fn captures_map_by_longest_prefix() {
        // Families and their specific members
        assert_eq!(capture_tone("keyword.storage.type"), Some(Tone::Keyword));
        assert_eq!(capture_tone("function.macro"), Some(Tone::Function));
        assert_eq!(capture_tone("markup.heading.1"), Some(Tone::Heading));
        assert_eq!(capture_tone("markup.heading.marker"), Some(Tone::Heading));
        assert_eq!(capture_tone("diff.plus"), Some(Tone::Inserted));
        assert_eq!(
            capture_tone("constant.character.escape"),
            Some(Tone::Escape)
        );
        assert_eq!(capture_tone("constant.character"), Some(Tone::String));
        assert_eq!(
            capture_tone("constant.numeric.integer"),
            Some(Tone::Constant)
        );
        assert_eq!(capture_tone("variable.parameter"), Some(Tone::Parameter));
        assert_eq!(capture_tone("variable.other.member"), Some(Tone::Property));

        // A prefix matches only at a dot: `constructor` is not `constant`, `types` not `type`
        assert_eq!(capture_tone("constructor"), Some(Tone::Type));
        assert_eq!(capture_tone("types"), None);

        // Resets paint plain; helpers and unknown names leave the enclosing highlight
        assert_eq!(capture_tone("none"), Some(Tone::Plain));
        assert_eq!(capture_tone("variable"), Some(Tone::Plain));
        assert_eq!(capture_tone("spell"), None);
        assert_eq!(capture_tone("nospell"), None);
        assert_eq!(capture_tone("_name"), None);
        assert_eq!(capture_tone("namespace"), None);
        assert_eq!(capture_tone("embedded"), None);
    }

    /// bat's scopes map by prefix too, so quotes take the string's tone and an interpolation
    /// resets to plain.
    #[test]
    fn scopes_map_by_longest_prefix() {
        assert_eq!(
            scope_tone("comment.line.double-slash.kotlin"),
            Some(Tone::Comment)
        );
        assert_eq!(
            scope_tone("punctuation.definition.string.begin.kotlin"),
            Some(Tone::String)
        );
        assert_eq!(
            scope_tone("keyword.operator.arithmetic"),
            Some(Tone::Operator)
        );
        assert_eq!(
            scope_tone("storage.type.function.kotlin"),
            Some(Tone::Keyword)
        );
        assert_eq!(scope_tone("meta.interpolation.kotlin"), Some(Tone::Plain));
        assert_eq!(scope_tone("source.kotlin"), None);
        assert_eq!(scope_tone("meta.function.kotlin"), None);
    }

    /// Every tone survives the trip through tree-house's highlight id.
    #[test]
    fn indices_round_trip() {
        for tone in Tone::ALL {
            assert_eq!(Tone::from_index(tone.index()), Some(tone));
        }
        let count = u32::try_from(Tone::ALL.len()).unwrap();
        assert_eq!(Tone::from_index(count), None);
    }
}
