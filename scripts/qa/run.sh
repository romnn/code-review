#!/usr/bin/env bash
# Drives review-gui inside a headless sway session and takes screenshots, or review-tui off
# screen and has it write its screen as text.
#
# Usage: scripts/qa/run.sh <script> [binary]
#
# The script has one command per line; blank lines and lines starting with `#` are skipped.
#   include <script>  run another script's lines here, its path from the repository's root, as
#                     scripts/qa/smoke-common.qa holds what both front ends check alike
#   launch <args...>  start the app with `--qa <args...>`, quitting a running one first
#   shot <name>       save a screenshot to target/qa/<name>.png; review-tui writes its screen to
#                     target/qa/tui/<name>.txt and, with its colors, <name>.ansi
#   expect <regex>    fail unless the previous answer matches the extended regex
#   exited            wait for the app to quit by itself, as after `key q`, and fail if it does
#                     not within a minute; `launch` starts the next one
#   anything else     a QA command for the app (see crates/review-gui/src/app/qa.rs and
#                     crates/review-tui/src/qa.rs); the runner waits for its one-line answer and
#                     fails on `error: ...`
# Every command and its full answer are also logged to target/qa/answers.log.
#
# QA_FRONT_END=tui drives review-tui instead (`task qa:tui`), without sway, keeping its
# configuration, demo, logs and shots under target/qa/tui/ apart from review-gui's.
# The binary defaults to target/debug/review-gui (or review-tui), built first.
# QA_VULKAN=lavapipe makes review-gui render with Mesa's software Vulkan driver.
# QA_OUT=<dir> keeps the configuration, demo, logs and shots there instead, so a run of its own,
# as scripts/screenshots/run.sh starts, leaves the smoke runs' files alone.
# QA_SIZE=<width>x<height> (1600x1000 by default) sets sway's output, and with it review-gui's
# window, in logical pixels, and QA_SCALE=<factor> (1 by default) its scale: 2 renders and
# shoots the same layout at twice the pixels.
# QA_RESIZE=<size> sends `resize <size>` after every launch, as review-tui starts at 160x50;
# an `expect` right after `launch` still checks the launch's state.
# review-gui needs sway and grim; sway and the app always shut down, even on failure.

set -euo pipefail

script=${1:?usage: scripts/qa/run.sh <script> [binary]}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
binary=${2:-}
front_end=${QA_FRONT_END:-gui}
case $front_end in
    gui) out=${QA_OUT:-$root/target/qa} ;;
    tui) out=${QA_OUT:-$root/target/qa/tui} ;;
    *)
        echo "qa: QA_FRONT_END is gui or tui, not $front_end" >&2
        exit 2
        ;;
esac
mkdir -p "$out"
log="$out/app.log"
answers="$out/answers.log"
: >"$log"
: >"$answers"
# Every run starts from the default settings; launches within it share what they remember.
rm -f "$out/smoke-config.toml"

if [[ -z $binary ]]; then
    cargo build --quiet -p "review-$front_end" --manifest-path "$root/Cargo.toml"
    binary="$root/target/debug/review-$front_end"
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
    local size=${QA_SIZE:-1600x1000} scale=${QA_SCALE:-1}
    # Client-side decorations, as on GNOME, so screenshots show the app's own title bar.
    # The resolution is in physical pixels.
    printf 'output HEADLESS-1 resolution %.0fx%.0f scale %s\ndefault_border none\nfor_window [app_id="code-review"] border csd\n' \
        "$(bc <<<"${size%x*} * $scale")" "$(bc <<<"${size#*x} * $scale")" "$scale" >"$config"
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
    printf '%s\t%s\n' "$1" "$answer" >>"$answers"
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
    local -a env=(CODE_REVIEW_CONFIG="$out/smoke-config.toml" CODE_REVIEW_DEMO_DIR="$out/smoke-demo")
    if [[ $front_end == gui ]]; then
        env=(-u DISPLAY WAYLAND_DISPLAY="$socket" "${env[@]}")
    fi
    if [[ ${QA_VULKAN:-} == lavapipe ]]; then
        env+=(VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json)
    fi
    coproc APP { exec env "${env[@]}" timeout 600 "$binary" --qa "$@" 2>>"$log"; }
    app_pid=$APP_PID
    # The first answer also proves that the window has rendered a frame.
    send state
    if [[ -n ${QA_RESIZE:-} ]]; then
        local launched=$answer
        send "resize $QA_RESIZE"
        if [[ $answer == error:* ]]; then
            echo "qa: resize $QA_RESIZE: $answer" >&2
            exit 1
        fi
        answer=$launched
    fi
}

# Prints the lines of the script `$1` with each `include <script>` line replaced by that
# script's own lines, and a line naming a script that does not exist, which fails the run.
expand() {
    local line
    if [[ ! -f $1 ]]; then
        printf 'include-missing %s\n' "$1"
        return
    fi
    while IFS= read -r line || [[ -n $line ]]; do
        if [[ $line == include\ * ]]; then
            expand "$root/${line#include }"
        else
            printf '%s\n' "$line"
        fi
    done <"$1"
}

if [[ $front_end == gui ]]; then
    start_sway
fi
answer=""
while IFS= read -r line || [[ -n $line ]]; do
    [[ -z ${line// /} || $line == \#* ]] && continue
    echo "> $line"
    case $line in
        include-missing\ *)
            echo "qa: no script ${line#include-missing }" >&2
            exit 1
            ;;
        launch*)
            read -r -a words <<<"${line#launch}"
            launch "${words[@]}"
            ;;
        shot\ *)
            if [[ $front_end == tui ]]; then
                # The app writes its own screen, off screen as it is; an `expect` after the
                # shot still checks the answer before it.
                checked=$answer
                send "shot $out/${line#shot }"
                if [[ $answer != ok ]]; then
                    echo "qa: $answer" >&2
                    exit 1
                fi
                answer=$checked
                echo "  saved $out/${line#shot }.txt and .ansi"
            else
                WAYLAND_DISPLAY=$socket timeout 30 grim "$out/${line#shot }.png"
                echo "  saved $out/${line#shot }.png"
            fi
            continue
            ;;
        expect\ *)
            if [[ ! $answer =~ ${line#expect } ]]; then
                echo "qa: expected /${line#expect }/ in: $answer" >&2
                exit 1
            fi
            continue
            ;;
        exited)
            if [[ -z $app_pid ]]; then
                echo "qa: 'exited' needs a running app; start one with launch" >&2
                exit 1
            fi
            if ! timeout 60 tail --pid="$app_pid" -f /dev/null; then
                echo "qa: the app did not quit" >&2
                exit 1
            fi
            wait "$app_pid" || true
            app_pid=""
            echo "  exited"
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
done < <(expand "$script")
quit_app
