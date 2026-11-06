#!/usr/bin/env bash
# Keeps a review app running in headless sway between calls, so an agent can drive it one
# command at a time and take screenshots whenever it wants.
#
# Usage:
#   scripts/qa/session.sh start [app args...]   build, start sway and launch the app with --qa
#   scripts/qa/session.sh cmd <qa command...>   send one QA command and print its answer
#   scripts/qa/session.sh shot <name>           save target/qa/<name>.png and print its path
#   scripts/qa/session.sh log                   print the end of the app's stderr
#   scripts/qa/session.sh status                say whether a session is running
#   scripts/qa/session.sh stop                  quit the app and sway
#
# QA commands are those of scripts/qa/run.sh, without launch, shot and expect: key, action,
# click, drag, wheel, scroll, resize, find, clipboard, latency, sample, state and quit
# (see crates/review-gui/src/app/qa.rs).
#
# QA_SIZE=WxH sets the output size (default 1600x1000).
# QA_BINARY replaces target/debug/review-gui, which is otherwise built first;
# QA_BINARY=scripts/zed/launch.sh drives the Zed fork, which speaks the same protocol.
# QA_VULKAN=lavapipe renders with Mesa's software Vulkan driver.
# QA_SESSION=<name> keeps a separate session under target/qa/<name>, for agents working in parallel.
# QA_TTL bounds the session in seconds (default 3600); every process is started under timeout.
# The app reads and remembers settings in target/qa/<session>-config.toml, never the user's, and
# keeps its demo under target/qa/<session>-demo, apart from other front ends' demos.

set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
out="$root/target/qa"
# QA_SESSION names the session, so several agents can each drive their own app at once.
dir="$out/${QA_SESSION:-session}"
runtime=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}
ttl=${QA_TTL:-3600}

alive() {
    [[ -f $dir/$1.pid ]] && kill -0 "$(cat "$dir/$1.pid")" 2>/dev/null
}

require_session() {
    if ! alive app; then
        echo "qa: no running session; start one with: scripts/qa/session.sh start --demo" >&2
        exit 1
    fi
}

stop() {
    if alive app; then
        printf 'quit\n' >"$dir/in" || true
        for _ in $(seq 50); do
            alive app || break
            sleep 0.1
        done
    fi
    local name
    for name in app holder sway; do
        if alive "$name"; then
            kill "$(cat "$dir/$name.pid")" 2>/dev/null || true
        fi
    done
    rm -rf "$dir"
}

start() {
    stop
    mkdir -p "$dir"
    local binary=${QA_BINARY:-}
    if [[ -z $binary ]]; then
        cargo build --quiet -p review-gui --manifest-path "$root/Cargo.toml"
        binary="$root/target/debug/review-gui"
    fi

    # Client-side decorations, as on GNOME, so screenshots show the app's own title bar.
    printf 'output HEADLESS-1 resolution %s\ndefault_border none\nfor_window [app_id="code-review"] border csd\nfor_window [app_id="dev.zed.Zed-Dev"] border csd\n' \
        "${QA_SIZE:-1600x1000}" >"$dir/sway.conf"
    local -A before=()
    local path
    for path in "$runtime"/wayland-*; do
        before[$path]=1
    done
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
        setsid timeout "$ttl" sway -c "$dir/sway.conf" >"$dir/sway.log" 2>&1 &
    echo $! >"$dir/sway.pid"
    local socket=""
    for _ in $(seq 100); do
        for path in "$runtime"/wayland-*; do
            if [[ $path != *.lock && -S $path && -z ${before[$path]:-} ]]; then
                socket=$(basename "$path")
                break 2
            fi
        done
        alive sway || break
        sleep 0.1
    done
    if [[ -z $socket ]]; then
        echo "qa: sway did not start; see $dir/sway.log" >&2
        stop
        exit 1
    fi
    echo "$socket" >"$dir/socket"

    # The holder keeps the FIFO open, so each `cmd` can open, write and close it without the
    # app seeing end of input.
    mkfifo "$dir/in"
    setsid timeout "$ttl" sleep "$ttl" >"$dir/in" &
    echo $! >"$dir/holder.pid"
    # The session's own configuration file outlives restarts, so remembered pane widths and
    # diff styles can be checked across launches; delete it for the defaults.
    local -a env=(-u DISPLAY WAYLAND_DISPLAY="$socket" CODE_REVIEW_CONFIG="$out/${QA_SESSION:-session}-config.toml"
        CODE_REVIEW_DEMO_DIR="$out/${QA_SESSION:-session}-demo")
    if [[ ${QA_VULKAN:-} == lavapipe ]]; then
        env+=(VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json)
    fi
    : >"$dir/out"
    setsid env "${env[@]}" timeout "$ttl" "$binary" --qa "$@" <"$dir/in" >"$dir/out" 2>"$dir/app.log" &
    echo $! >"$dir/app.pid"
    # The first answer proves that the window has rendered a frame.
    cmd state
}

# Sends one command and prints the one-line answer it produces.
cmd() {
    require_session
    local before
    before=$(wc -l <"$dir/out")
    printf '%s\n' "$*" >"$dir/in"
    local _
    for _ in $(seq 1800); do
        if (($(wc -l <"$dir/out") > before)); then
            local answer
            answer=$(sed -n "$((before + 1))p" "$dir/out")
            printf '%s\n' "$answer"
            [[ $answer != error:* ]]
            return
        fi
        if ! alive app; then
            echo "qa: the app exited; see scripts/qa/session.sh log" >&2
            exit 1
        fi
        sleep 0.1
    done
    echo "qa: no answer to '$*' within 180 s" >&2
    exit 1
}

shot() {
    require_session
    local name=${1:?usage: scripts/qa/session.sh shot <name>}
    mkdir -p "$out"
    WAYLAND_DISPLAY=$(cat "$dir/socket") timeout 30 grim "$out/$name.png"
    echo "$out/$name.png"
}

case ${1:-} in
    start) shift; start "$@" ;;
    cmd) shift; cmd "$@" ;;
    shot) shift; shot "$@" ;;
    log) tail -n "${2:-40}" "$dir/app.log" ;;
    status) if alive app; then echo "running on $(cat "$dir/socket")"; else echo "stopped"; fi ;;
    stop) stop; echo "stopped" ;;
    *) sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
