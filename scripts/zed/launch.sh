#!/usr/bin/env bash
# Launches the Zed fork (~/dev/zed-review) straight into a review, with its own Zed profile.
#
# Usage: scripts/zed/launch.sh [--qa] [--demo | --demo-revision N | --range BASE..HEAD | --worktree REF]
#
#   --demo             rebuild the built-in demo under target/demo and open revision 1 (default)
#   --demo-revision N  advance the demo to revision N, keeping marks
#   --range A..B       review a range of the repository in CODE_REVIEW_REPO or the current directory
#   --worktree REF     review the working tree against REF
#   --qa               read QA commands on standard input (see crates/code_review/src/qa.rs)
#
# The profile under target/zed-profile (ZED_PROFILE overrides it) holds the dark theme, the
# terminal font and the diff colors, so the user's own Zed settings and sessions stay untouched.
# It also keeps rust-analyzer from running `cargo check`, build scripts and proc macros: they
# would write a `target/` directory into the read-only review snapshot, which Zed then rescans
# on its main thread, and they run the reviewed code.
# ZED_BINARY overrides the binary, which defaults to the fork's release-fast build when it is
# newer than its debug build, and to the debug build otherwise.

set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
targets=/mnt/ssd2/cargo-targets/zed-review
# The optimized `release-fast` build when it is newer than the debug build; ZED_BINARY overrides.
binary=${ZED_BINARY:-}
if [[ -z $binary ]]; then
    binary=$targets/debug/zed
    if [[ -x $targets/release-fast/zed && ( ! -e $binary || $targets/release-fast/zed -nt $binary ) ]]; then
        binary=$targets/release-fast/zed
    fi
fi
# Standard output carries QA answers, so the choice goes to standard error.
echo "launch.sh: using $binary" >&2
profile=${ZED_PROFILE:-$root/target/zed-profile}

env_args=(CODE_REVIEW_DEMO=1)
while (($#)); do
    case $1 in
        # Headless QA renders on whatever Vulkan device sway offers, often a software one.
        --qa) env_args+=(CODE_REVIEW_QA=1 ZED_ALLOW_EMULATED_GPU=1) ;;
        --demo) env_args=("${env_args[@]/CODE_REVIEW_DEMO=1/}" CODE_REVIEW_DEMO=1) ;;
        --demo-revision)
            env_args=("${env_args[@]/CODE_REVIEW_DEMO=1/}" "CODE_REVIEW_DEMO_REVISION=${2:?--demo-revision takes N}")
            shift
            ;;
        --range)
            env_args=("${env_args[@]/CODE_REVIEW_DEMO=1/}" "CODE_REVIEW_RANGE=${2:?--range takes BASE..HEAD}")
            shift
            ;;
        --worktree)
            env_args=("${env_args[@]/CODE_REVIEW_DEMO=1/}" "CODE_REVIEW_WORKTREE=${2:?--worktree takes REF}")
            shift
            ;;
        *)
            echo "launch.sh: unknown argument $1" >&2
            exit 2
            ;;
    esac
    shift
done

# Zed restores a worktree's last tabs from its database; every launch starts clean instead.
rm -rf "$profile/db"
mkdir -p "$profile/config"
cat >"$profile/config/settings.json" <<'JSON'
{
  "theme": { "mode": "dark", "dark": "One Dark", "light": "One Dark" },
  "buffer_font_family": "SFMono Nerd Font",
  "buffer_font_weight": 500,
  "buffer_font_size": 13,
  "ui_font_size": 15,
  "restore_on_startup": "none",
  "sticky_scroll": { "enabled": true },
  "project_panel": { "starts_open": false },
  "telemetry": { "diagnostics": false, "metrics": false },
  "auto_update": false,
  "lsp": {
    "rust-analyzer": {
      "initialization_options": {
        "checkOnSave": false,
        "cargo": { "buildScripts": { "enable": false } },
        "procMacro": { "enable": false }
      }
    }
  },
  "theme_overrides": {
    "One Dark": {
      "editor.diff_hunk.added.background": "#14471fff",
      "editor.diff_hunk.deleted.background": "#5f1519ff"
    }
  }
}
JSON

filtered=("CODE_REVIEW_REPO=${CODE_REVIEW_REPO:-$PWD}")
for value in "${env_args[@]}"; do
    [[ -n $value ]] && filtered+=("$value")
done
# A debug build reads its assets from the checkout at run time, relative to the working directory.
cd "${ZED_CHECKOUT:-$HOME/dev/zed-review}"
exec env "${filtered[@]}" "$binary" --user-data-dir "$profile"
