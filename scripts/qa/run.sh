#!/usr/bin/env bash
# Drives review-lite inside a headless sway session and takes screenshots.
#
# Usage: scripts/qa/run.sh <script> [binary]
#
# The script has one command per line; blank lines and lines starting with `#` are skipped.
#   launch <args...>  start the app with `--qa <args...>`, quitting a running one first
#   shot <name>       save a screenshot to target/qa/<name>.png
#   expect <regex>    fail unless the previous answer matches the extended regex
#   anything else     a QA command for the app (see crates/review-lite/src/app/qa.rs); the
#                     runner waits for its one-line answer and fails on `error: ...`
#
# The binary defaults to target/debug/review-lite, built first.
# QA_VULKAN=lavapipe makes the app render with Mesa's software Vulkan driver.
# Needs sway and grim; sway and the app always shut down, even on failure.

set -euo pipefail

script=${1:?usage: scripts/qa/run.sh <script> [binary]}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
binary=${2:-}
out="$root/target/qa"
mkdir -p "$out"
log="$out/app.log"
: >"$log"

if [[ -z $binary ]]; then
    cargo build --quiet -p review-lite --manifest-path "$root/Cargo.toml"
    binary="$root/target/debug/review-lite"
fi

runtime=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}
sway_pid=""
app_pid=""
socket=""

cleanup() {
    local status=$?
    if [[ -n $app_pid ]] && kill -0 "$app_pid" 2>/dev/null; then
        kill "$app_pid" 2>/dev/null || true
        wait "$app_pid" 2>/dev/null || true
    fi
    if [[ -n $sway_pid ]] && kill -0 "$sway_pid" 2>/dev/null; then
        kill "$sway_pid" 2>/dev/null || true
        wait "$sway_pid" 2>/dev/null || true
    fi
    if ((status != 0)); then
        echo "qa: failed; the app's log is $log" >&2
    fi
    exit "$status"
}
trap cleanup EXIT

# Starts headless sway and finds the socket it creates.
start_sway() {
    local config="$out/sway.conf"
    printf 'output HEADLESS-1 resolution 1600x1000\ndefault_border none\n' >"$config"
    local -A before=()
    local path
    for path in "$runtime"/wayland-*; do
        before[$path]=1
    done
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
        timeout 900 sway -c "$config" >"$out/sway.log" 2>&1 &
    sway_pid=$!
    local attempt
    for attempt in $(seq 100); do
        for path in "$runtime"/wayland-*; do
            if [[ $path != *.lock && -S $path && -z ${before[$path]:-} ]]; then
                socket=$(basename "$path")
                return
            fi
        done
        if ! kill -0 "$sway_pid" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
    echo "qa: sway did not create a socket after $attempt tries; see $out/sway.log" >&2
    exit 1
}

# Sends one command to the app and reads its answer into `answer`.
send() {
    if [[ -z $app_pid ]]; then
        echo "qa: '$1' needs a running app; start one with launch" >&2
        exit 1
    fi
    printf '%s\n' "$1" >&"${APP[1]}"
    if ! IFS= read -r -t 180 answer <&"${APP[0]}"; then
        echo "qa: no answer to '$1'" >&2
        exit 1
    fi
}

quit_app() {
    if [[ -n $app_pid ]]; then
        send quit
        wait "$app_pid" || true
        app_pid=""
    fi
}

launch() {
    quit_app
    local -a env=(-u DISPLAY WAYLAND_DISPLAY="$socket")
    if [[ ${QA_VULKAN:-} == lavapipe ]]; then
        env+=(VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json)
    fi
    coproc APP { exec env "${env[@]}" timeout 600 "$binary" --qa "$@" 2>>"$log"; }
    app_pid=$APP_PID
    # The first answer also proves that the window has rendered a frame.
    send state
}

start_sway
answer=""
while IFS= read -r line || [[ -n $line ]]; do
    [[ -z ${line// /} || $line == \#* ]] && continue
    echo "> $line"
    case $line in
        launch*)
            read -r -a words <<<"${line#launch}"
            launch "${words[@]}"
            ;;
        shot\ *)
            WAYLAND_DISPLAY=$socket timeout 30 grim "$out/${line#shot }.png"
            echo "  saved $out/${line#shot }.png"
            continue
            ;;
        expect\ *)
            if [[ ! $answer =~ ${line#expect } ]]; then
                echo "qa: expected /${line#expect }/ in: $answer" >&2
                exit 1
            fi
            continue
            ;;
        *)
            send "$line"
            ;;
    esac
    echo "  ${answer:0:300}"
    if [[ $answer == error:* ]]; then
        exit 1
    fi
done <"$script"
quit_app
