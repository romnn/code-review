//! Named syntactic scopes, such as `impl Ledger › fn apply_batch`, for breadcrumbs that say where
//! a hunk sits in its file.
//!
//! Scopes are a reading aid only: they come from tree-sitter's error-recovering parse, so a file
//! with syntax errors still gets the scopes the parser could recover, and nothing about review
//! state depends on them.

use serde::Serialize;
use tree_sitter::{Node, Parser, Tree};

use crate::syntax::Language;

/// What kind of construct a scope is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// A module or namespace.
    Module,
    /// A struct, enum, union, class, interface or type declaration.
    Type,
    /// A Rust `impl` block.
    Impl,
    /// A Rust trait.
    Trait,
    /// A function, method or function-valued binding.
    Function,
    /// A JSON object member or a TOML table.
    Section,
}

/// One named scope of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scope {
    /// What kind of construct it is.
    pub kind: ScopeKind,
    /// How it reads in a breadcrumb, in the language's own words: `fn apply_batch`,
    /// `impl Display for Ledger`, `class Parser`, `def parse`, `func (*Ledger) Apply`, `[package]`.
    pub label: String,
    /// The zero-based line where it starts.
    pub start_line: usize,
    /// The zero-based line where it ends, inclusive.
    pub end_line: usize,
}

/// Every named scope of one file, for finding the scopes around a line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeMap {
    /// Scopes in document order; a scope comes before the scopes nested in it.
    scopes: Vec<Scope>,
}

impl ScopeMap {
    /// Parses `text` as the language of `path`; unsupported languages have no scopes.
    #[must_use]
    pub fn for_path(path: &str, text: &str) -> Self {
        Language::from_path(path).map_or_else(Self::default, |language| Self::new(language, text))
    }

    /// Parses `text` as `language`.
    #[must_use]
    pub fn new(language: Language, text: &str) -> Self {
        let mut parser = Parser::new();
        if parser.set_language(&language.grammar()).is_err() {
            return Self::default();
        }
        let Some(tree) = parser.parse(text, None) else {
            return Self::default();
        };
        Self::from_tree(language, &tree, text)
    }

    /// Reads the scopes from `tree`, a parse of `text` as `language`, for callers that parse
    /// the file anyway.
    #[must_use]
    pub fn from_tree(language: Language, tree: &Tree, text: &str) -> Self {
        let mut scopes = Vec::new();
        collect(language, tree.root_node(), text, &mut scopes);
        Self { scopes }
    }

    /// The scopes that contain the zero-based `line` and started on an earlier line, outermost
    /// first. A scope whose first line is `line` is left out: that line already shows it.
    #[must_use]
    pub fn enclosing(&self, line: usize) -> Vec<&Scope> {
        self.scopes
            .iter()
            .filter(|scope| scope.start_line < line && line <= scope.end_line)
            .collect()
    }

    /// Every scope of the file, in document order.
    #[must_use]
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }
}

/// Joins scopes into one breadcrumb, `impl Ledger › fn apply_batch`.
#[must_use]
pub fn breadcrumb(scopes: &[&Scope]) -> String {
    scopes
        .iter()
        .map(|scope| scope.label.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

/// Appends the scopes at and below `node`, depth first.
fn collect(language: Language, node: Node<'_>, text: &str, scopes: &mut Vec<Scope>) {
    if let Some((kind, label)) = scope_of(language, node, text) {
        scopes.push(Scope {
            kind,
            label,
            start_line: node.start_position().row,
            end_line: node.end_position().row,
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(language, child, text, scopes);
    }
}

/// The text of `node`'s field `field`, with its whitespace collapsed to single spaces.
fn field(node: Node<'_>, field: &str, text: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    let raw = text.get(child.byte_range())?;
    Some(raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whether `node` is a scope, and its kind and label.
fn scope_of(language: Language, node: Node<'_>, text: &str) -> Option<(ScopeKind, String)> {
    let name = || field(node, "name", text);
    match language {
        Language::Rust => rust_scope(node, text),
        Language::Python => match node.kind() {
            "class_definition" => Some((ScopeKind::Type, format!("class {}", name()?))),
            "function_definition" => Some((ScopeKind::Function, format!("def {}", name()?))),
            _ => None,
        },
        Language::Go => match node.kind() {
            "function_declaration" => Some((ScopeKind::Function, format!("func {}", name()?))),
            "method_declaration" => {
                let receiver = node
                    .child_by_field_name("receiver")
                    .and_then(|receiver| receiver.named_child(0))
                    .and_then(|parameter| field(parameter, "type", text));
                Some((
                    ScopeKind::Function,
                    match receiver {
                        Some(receiver) => format!("func ({receiver}) {}", name()?),
                        None => format!("func {}", name()?),
                    },
                ))
            }
            "type_spec" => Some((ScopeKind::Type, format!("type {}", name()?))),
            _ => None,
        },
        Language::JavaScript | Language::TypeScript | Language::Tsx => script_scope(node, text),
        Language::Json => {
            if node.kind() != "pair" {
                return None;
            }
            let value = node.child_by_field_name("value")?;
            if !matches!(value.kind(), "object" | "array") {
                return None;
            }
            let key = field(node, "key", text)?;
            Some((ScopeKind::Section, key.trim_matches('"').to_owned()))
        }
        Language::Toml => match node.kind() {
            "table" | "table_array_element" => {
                let key = node.named_child(0)?;
                let key = text.get(key.byte_range())?.trim();
                let label = if node.kind() == "table" {
                    format!("[{key}]")
                } else {
                    format!("[[{key}]]")
                };
                Some((ScopeKind::Section, label))
            }
            _ => None,
        },
    }
}

fn rust_scope(node: Node<'_>, text: &str) -> Option<(ScopeKind, String)> {
    let name = || field(node, "name", text);
    match node.kind() {
        "mod_item" => Some((ScopeKind::Module, format!("mod {}", name()?))),
        "struct_item" => Some((ScopeKind::Type, format!("struct {}", name()?))),
        "enum_item" => Some((ScopeKind::Type, format!("enum {}", name()?))),
        "union_item" => Some((ScopeKind::Type, format!("union {}", name()?))),
        "trait_item" => Some((ScopeKind::Trait, format!("trait {}", name()?))),
        "impl_item" => {
            let target = field(node, "type", text)?;
            Some((
                ScopeKind::Impl,
                match field(node, "trait", text) {
                    Some(implemented) => format!("impl {implemented} for {target}"),
                    None => format!("impl {target}"),
                },
            ))
        }
        "function_item" | "function_signature_item" => {
            Some((ScopeKind::Function, format!("fn {}", name()?)))
        }
        "macro_definition" => Some((ScopeKind::Function, format!("macro_rules! {}", name()?))),
        _ => None,
    }
}

fn script_scope(node: Node<'_>, text: &str) -> Option<(ScopeKind, String)> {
    let name = || field(node, "name", text);
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" | "class" => {
            Some((ScopeKind::Type, format!("class {}", name()?)))
        }
        "interface_declaration" => Some((ScopeKind::Type, format!("interface {}", name()?))),
        "enum_declaration" => Some((ScopeKind::Type, format!("enum {}", name()?))),
        "internal_module" | "module" => Some((ScopeKind::Module, format!("namespace {}", name()?))),
        "function_declaration" | "generator_function_declaration" => {
            Some((ScopeKind::Function, format!("function {}", name()?)))
        }
        "method_definition" => Some((ScopeKind::Function, format!("{}()", name()?))),
        // `const handler = () => {}` and `const f = function () {}`.
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "function"
            )
            .then(|| Some((ScopeKind::Function, format!("{}()", name()?))))
            .flatten()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The breadcrumb at the zero-based `line` of `text`.
    fn at(language: Language, text: &str, line: usize) -> String {
        breadcrumb(&ScopeMap::new(language, text).enclosing(line))
    }

    #[test]
    fn rust_scopes_name_impls_traits_and_functions() {
        let text = "mod ledger {\n\
                    impl Display for Ledger {\n\
                    fn fmt(&self) {\n\
                    let a = 1;\n\
                    }\n\
                    }\n\
                    impl<T> Ledger<T> {\n\
                    fn apply(&mut self) {\n\
                    self.n += 1;\n\
                    }\n\
                    }\n\
                    }\n";
        assert_eq!(
            at(Language::Rust, text, 3),
            "mod ledger › impl Display for Ledger › fn fmt"
        );
        assert_eq!(
            at(Language::Rust, text, 8),
            "mod ledger › impl Ledger<T> › fn apply"
        );
        // A scope's own first line is not inside it.
        assert_eq!(at(Language::Rust, text, 7), "mod ledger › impl Ledger<T>");
        assert_eq!(at(Language::Rust, text, 0), "");
    }

    #[test]
    fn python_scopes_are_classes_and_defs() {
        let text =
            "class Parser:\n    def parse(self):\n        return 1\n\ndef main():\n    pass\n";
        assert_eq!(at(Language::Python, text, 2), "class Parser › def parse");
        assert_eq!(at(Language::Python, text, 5), "def main");
    }

    #[test]
    fn go_methods_name_their_receiver() {
        let text = "package main\n\nfunc (l *Ledger) Apply() {\n\tl.n++\n}\n\ntype T struct {\n\tA int\n}\n";
        assert_eq!(at(Language::Go, text, 3), "func (*Ledger) Apply");
        assert_eq!(at(Language::Go, text, 7), "type T");
    }

    #[test]
    fn script_scopes_include_methods_and_function_bindings() {
        let text = "class View {\n  render() {\n    return 1;\n  }\n}\nconst onClick = () => {\n  go();\n};\n";
        assert_eq!(at(Language::TypeScript, text, 2), "class View › render()");
        assert_eq!(at(Language::JavaScript, text, 6), "onClick()");
        let tsx = "function App() {\n  return <div>\n  </div>;\n}\n";
        assert_eq!(at(Language::Tsx, tsx, 2), "function App");
    }

    #[test]
    fn json_and_toml_scopes_are_sections() {
        let json = "{\n  \"server\": {\n    \"port\": 1\n  }\n}\n";
        assert_eq!(at(Language::Json, json, 2), "server");
        let toml = "[package]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n";
        assert_eq!(at(Language::Toml, toml, 1), "[package]");
        assert_eq!(at(Language::Toml, toml, 4), "[[bin]]");
    }

    /// Unsupported files have no scopes, and broken files keep what the parser recovers.
    #[test]
    fn unsupported_and_broken_files() {
        assert_eq!(
            ScopeMap::for_path("notes.txt", "fn f() {\n}\n").scopes(),
            &[] as &[Scope]
        );
        let broken = "impl Ledger {\n    fn f(&self) {\n        let x = ;\n    }\n}\n";
        assert_eq!(at(Language::Rust, broken, 2), "impl Ledger › fn f");
    }
}
