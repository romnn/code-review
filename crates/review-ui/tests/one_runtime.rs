//! The workspace links exactly one tree-sitter runtime, romnn/tree-sitter, which review-core's
//! formatting proof, tree-house and every grammar share (ADR 3).
//!
//! Cargo checks `links = "tree-sitter"` only between packages that declare it, so a second
//! runtime that does not, or a grammar that brings one, would link silently; these tests read
//! the locked graph instead.

use std::process::Command;

use color_eyre::eyre::{self, OptionExt as _};
use serde_json::Value;

/// The fork every tree-sitter package comes from.
const FORK: &str = "git+https://github.com/romnn/tree-sitter";

/// The workspace's locked package graph, every feature of every member on.
fn metadata() -> eyre::Result<Value> {
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--all-features",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()?;
    if !output.status.success() {
        eyre::bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// The packages of `metadata`.
fn packages(metadata: &Value) -> eyre::Result<&[Value]> {
    metadata["packages"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_eyre("cargo metadata lists no packages")
}

/// Exactly one `tree-sitter` and one `tree-sitter-language`, by package ID, both from the fork,
/// and no other package claims the runtime's `links` key.
#[test]
fn the_graph_has_one_tree_sitter_runtime() -> eyre::Result<()> {
    let metadata = metadata()?;
    for name in ["tree-sitter", "tree-sitter-language"] {
        let found: Vec<&Value> = packages(&metadata)?
            .iter()
            .filter(|package| package["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "one {name}: {found:#?}");
        let source = found[0]["source"].as_str().unwrap_or_default();
        assert!(source.starts_with(FORK), "{name} comes from {source}");
    }
    let linking: Vec<&str> = packages(&metadata)?
        .iter()
        .filter(|package| package["links"] == "tree-sitter")
        .filter_map(|package| package["name"].as_str())
        .collect();
    assert_eq!(linking, ["tree-sitter"]);
    Ok(())
}

/// Every grammar crate, review-languages included, depends in the resolved graph only on
/// tree-sitter-language: none brings a runtime of its own.
#[test]
fn grammars_depend_only_on_tree_sitter_language() -> eyre::Result<()> {
    let metadata = metadata()?;
    let packages = packages(&metadata)?;
    let name_of = |id: &Value| {
        packages
            .iter()
            .find(|package| &package["id"] == id)
            .and_then(|package| package["name"].as_str())
            .unwrap_or_default()
    };
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or_eyre("cargo metadata resolved nothing")?;
    let mut grammars = 0;
    for node in nodes {
        let name = name_of(&node["id"]);
        let grammar = name == "review-languages"
            || (name.starts_with("tree-sitter-") && name != "tree-sitter-language");
        if !grammar {
            continue;
        }
        let normal: Vec<&str> = node["deps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|dep| {
                dep["dep_kinds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|kind| kind["kind"].is_null())
            })
            .map(|dep| name_of(&dep["pkg"]))
            .collect();
        assert_eq!(normal, ["tree-sitter-language"], "{name}'s dependencies");
        grammars += 1;
    }
    assert!(grammars >= 7, "the grammars are found: {grammars}");
    Ok(())
}
