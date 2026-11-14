#!/usr/bin/env -S uv run --script --quiet
# /// script
# requires-python = ">=3.12"
# dependencies = ["tomlkit==0.15.2"]
# ///
"""Moves review-languages' manifest to a Helix commit, which also moves the queries it embeds.

Usage: task grammars:sync -- <helix commit>, which runs
uv run --script scripts/grammars/sync.py <helix commit>, with tomlkit pinned exactly in the
header above.

1. Downloads Helix's languages.toml at the commit from codeload.github.com into a fresh
   directory, and reads it as data.
2. Rewrites crates/review-languages/languages.toml with tomlkit, keeping its comments: the
   `helix` commit, and for every row not `locked`, its fetched grammar's `git`, `rev` and
   `subpath`, its `extensions`, `filenames`, `globs` and `shebangs`, and its `injection-regex`,
   as Helix's languages.toml has them.
   A list of fields in `locked` keeps those; names, tiers, queries, local queries, template
   data, aliases, sources, licenses and bat syntaxes are never Helix's to change.
   review-languages' build script fetches Helix's queries at the `helix` commit, so moving it
   moves them too.
3. Compares the grammar.json and node-types.json of review-core's grammar crates, as Cargo.lock
   locks them, with those of the revisions Helix pins, and reports drift: those crates come from
   crates.io, so Helix's queries fit them only as long as they match.
4. Fetches the new pins and queries, which checks that each grammar still has its `sources` and
   each query directory still exists, and runs the tests that compile every row's queries and
   resolve every `; inherits:` line.

Run `task test` afterwards: the token fixtures of review-ui show what the new queries changed,
and `task third-party` writes the new commit into THIRD-PARTY-SYNTAXES.
"""

import json
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path

import tomlkit

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "crates/review-languages/languages.toml"
# review-core's grammar crates, by the Helix grammar they should match: the crate, and the
# grammar's directory in it.
PROOF = {
    "rust": ("tree-sitter-rust", ""),
    "python": ("tree-sitter-python", ""),
    "go": ("tree-sitter-go", ""),
    "json": ("tree-sitter-json", ""),
    "toml": ("tree-sitter-toml-ng", ""),
    "typescript": ("tree-sitter-typescript", "typescript"),
    "tsx": ("tree-sitter-typescript", "tsx"),
}


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return 0
    if len(sys.argv) != 2 or not re.fullmatch(r"[0-9a-f]{40}", sys.argv[1]):
        print("usage: task grammars:sync -- <full helix commit id>", file=sys.stderr)
        return 2
    commit = sys.argv[1]
    with tempfile.TemporaryDirectory(prefix="grammars-sync-") as scratch:
        scratch = Path(scratch)
        helix = download(
            f"https://codeload.github.com/helix-editor/helix/tar.gz/{commit}",
            scratch / "helix",
            lambda path: path == "languages.toml",
        )
        upstream = tomllib.loads((helix / "languages.toml").read_text())
        problems = sync_manifest(commit, upstream)
        problems += check_proof_grammars(upstream, scratch)
    run(["cargo", "run", "--quiet", "--locked", "-p", "review-grammar-sources"])
    run(
        [
            "cargo", "nextest", "run", "-P", "default", "--locked", "-p", "review-ui", "-E",
            "test(every_tree_house_row_compiles) | test(every_tree_house_rows_queries_are_embedded)"
            " | test(every_inherits_line_resolves)",
        ]
    )
    for problem in problems:
        print(f"warning: {problem}", file=sys.stderr)
    print(f"synced to Helix {commit}; run `task test` to see what the new queries change")
    return 0


def download(url: str, directory: Path, wanted) -> Path:
    """Downloads the tarball at `url` into the fresh `directory` and extracts the files whose
    path below the tarball's top directory `wanted` accepts, returning that top directory."""
    directory.mkdir()
    archive = directory / "archive.tar.gz"
    run(["curl", "--silent", "--show-error", "--fail", "--location", "--max-time", "600",
         "--output", str(archive), url])
    extracted = directory / "extracted"
    with tarfile.open(archive) as tar:
        members = []
        for member in tar.getmembers():
            _, _, path = member.name.partition("/")
            if member.isfile() and wanted(path):
                members.append(member)
        # The "data" filter refuses links, absolute paths and paths that leave the directory.
        tar.extractall(extracted, members=members, filter="data")
    tops = list(extracted.iterdir()) if extracted.is_dir() else []
    if len(tops) != 1:
        sys.exit(f"error: {url} does not hold one top directory with the wanted files")
    return tops[0]


def sync_manifest(commit: str, upstream: dict) -> list[str]:
    """Moves every unlocked row of the manifest to Helix's languages.toml, returning the rows
    Helix does not describe."""
    text, problems = sync_text(MANIFEST.read_text(), commit, upstream)
    MANIFEST.write_text(text)
    return problems


def sync_text(text: str, commit: str, upstream: dict) -> tuple[str, list[str]]:
    """`sync_manifest` on the manifest's text: the new text, and the rows Helix does not
    describe."""
    languages = {language["name"]: language for language in upstream.get("language", [])}
    grammars = {grammar["name"]: grammar for grammar in upstream.get("grammar", [])}
    document = tomlkit.parse(text)
    document["helix"] = commit
    problems = []
    for row in document["language"]:
        locked = locked_fields(row)
        if locked is None:
            continue
        name = row["name"]
        helix = languages.get(name)
        if helix is None:
            problems.append(f"Helix has no language {name}; its row stays as it is")
            continue
        if "grammar" in row and "grammar" not in locked:
            grammar_name = helix.get("grammar", name)
            source = grammars.get(grammar_name, {}).get("source", {})
            if "git" not in source or "rev" not in source:
                problems.append(f"Helix pins no git revision for {name}'s grammar")
            else:
                grammar = row["grammar"]
                set_value(grammar, "git", source["git"])
                set_value(grammar, "rev", source["rev"])
                set_value(grammar, "subpath", source.get("subpath"))
        detection = detect(helix)
        for field, value in detection.items():
            if field not in locked:
                set_value(row, field, value)
    return tomlkit.dumps(document), problems


def locked_fields(row) -> set[str] | None:
    """The fields of `row` that `locked` keeps, or None when it keeps the whole row.

    tomlkit hands a TOML boolean back as a plain bool, and an array as items of strings.
    """
    locked = row.get("locked", False)
    if isinstance(locked, bool):
        return None if locked else set()
    return {str(field) for field in locked}


def self_test() -> None:
    """Syncs a small manifest against a made-up Helix, checking that every form of `locked` is
    honored: `true` keeps the row, `false` and no `locked` move it, and a list keeps its
    fields."""
    manifest = """\
# A comment that survives.
helix = "0000000000000000000000000000000000000000"

[[language]]
name = "kept"
tier = "tree-house"
extensions = ["old"]
locked = true

[language.grammar]
git = "https://example.com/kept"
rev = "1111111111111111111111111111111111111111"

[[language]]
name = "moved"
tier = "tree-house"
extensions = ["old"]
locked = false

[language.grammar]
git = "https://example.com/moved"
rev = "1111111111111111111111111111111111111111"

[[language]]
name = "partly"
tier = "bat"
extensions = ["old"]
shebangs = ["old"]
locked = ["extensions"]
"""
    commit = "2222222222222222222222222222222222222222"
    upstream = {
        "language": [
            {"name": name, "file-types": ["new"], "shebangs": ["new"]}
            for name in ("kept", "moved", "partly")
        ],
        "grammar": [
            {"name": name, "source": {"git": f"https://example.com/{name}", "rev": "3" * 40}}
            for name in ("kept", "moved")
        ],
    }
    text, problems = sync_text(manifest, commit, upstream)
    rows = {row["name"]: row for row in tomllib.loads(text)["language"]}
    checks = {
        "the commit moves": tomllib.loads(text)["helix"] == commit,
        "comments survive": text.startswith("# A comment that survives."),
        "locked = true keeps the row": rows["kept"]["extensions"] == ["old"]
        and rows["kept"]["grammar"]["rev"] == "1" * 40,
        "locked = false moves the row": rows["moved"]["extensions"] == ["new"]
        and rows["moved"]["grammar"]["rev"] == "3" * 40,
        "a locked field stays": rows["partly"]["extensions"] == ["old"],
        "an unlocked field moves": rows["partly"]["shebangs"] == ["new"],
        "every language is known": problems == [],
    }
    failed = [check for check, passed in checks.items() if not passed]
    if failed:
        sys.exit(f"self-test failed: {', '.join(failed)}\n{text}")
    print(f"self-test passed: {len(checks)} checks")


def detect(helix: dict) -> dict:
    """A Helix language's detection in the manifest's fields.

    A Helix glob without a wildcard or `/` names a whole file, so it is a file name.
    """
    extensions, filenames, globs = [], [], []
    for file_type in helix.get("file-types", []):
        if isinstance(file_type, str):
            extensions.append(file_type)
        elif re.search(r"[*?/]", file_type["glob"]):
            globs.append(file_type["glob"])
        else:
            filenames.append(file_type["glob"])
    return {
        "extensions": extensions,
        "filenames": filenames,
        "globs": globs,
        "shebangs": list(helix.get("shebangs", [])),
        "injection-regex": helix.get("injection-regex"),
    }


def set_value(table, key: str, value) -> None:
    """Sets `key` of `table` to `value`, removing it for an empty one, and leaves an equal value
    as it is written."""
    if value in (None, [], ""):
        if key in table:
            del table[key]
        return
    current = table.get(key)
    if current is not None and current.unwrap() == value:
        return
    if isinstance(value, list):
        array = tomlkit.array()
        array.extend(value)
        array.multiline(len(", ".join(json.dumps(item) for item in value)) > 80)
        value = array
    table[key] = value


def check_proof_grammars(upstream: dict, scratch: Path) -> list[str]:
    """Compares review-core's grammar crates with the revisions Helix pins, returning the
    grammars that differ."""
    pins = {grammar["name"]: grammar.get("source", {}) for grammar in upstream.get("grammar", [])}
    metadata = json.loads(
        run(["cargo", "metadata", "--locked", "--format-version", "1"], capture=True)
    )
    crates = {
        package["name"]: Path(package["manifest_path"]).parent
        for package in metadata["packages"]
    }
    problems = []
    for name, (crate, subdirectory) in PROOF.items():
        source = pins.get(name, {})
        repository = re.fullmatch(
            r"https://github\.com/([^/]+)/([^/]+?)(?:\.git)?", source.get("git", "")
        )
        if crate not in crates or repository is None or "rev" not in source:
            problems.append(f"cannot compare {crate} with Helix's {name} grammar")
            continue
        owner, repo = repository.groups()
        subpath = source.get("subpath", "")
        prefix = f"{subpath}/src/" if subpath else "src/"
        pinned = download(
            f"https://codeload.github.com/{owner}/{repo}/tar.gz/{source['rev']}",
            scratch / f"proof-{name}",
            lambda path: path in (prefix + "grammar.json", prefix + "node-types.json"),
        )
        crate_src = crates[crate] / subdirectory / "src"
        for file in ("grammar.json", "node-types.json"):
            locked = json.loads((crate_src / file).read_text())
            helix = json.loads((pinned / prefix / file).read_text())
            if locked != helix:
                problems.append(
                    f"{crate}'s {subdirectory or '.'}/src/{file} differs from {owner}/{repo} at "
                    f"{source['rev']}, which Helix pins for {name}"
                )
        print(f"{name}: compared {crate} with {owner}/{repo} at {source['rev']}", flush=True)
    return problems


def run(command: list[str], capture: bool = False) -> str:
    """Runs `command` in the repository, failing the sync when it fails."""
    result = subprocess.run(command, cwd=ROOT, check=False, text=True,
                            stdout=subprocess.PIPE if capture else None)
    if result.returncode != 0:
        sys.exit(f"error: {' '.join(command)} failed")
    return result.stdout or ""


if __name__ == "__main__":
    sys.exit(main())
