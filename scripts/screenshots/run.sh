#!/usr/bin/env bash
# Regenerates the documentation's screenshots of review-gui and review-tui from the built-in
# demo (`task screenshots`), or a gallery of every theme in every diff style
# (`task screenshots:gallery`).
#
# Usage: scripts/screenshots/run.sh [--gallery] [--front-end gui|tui]
#
# Both front ends play scripts/screenshots/scenes.qa through scripts/qa/run.sh, the QA harness
# the smoke scripts use, once for each theme in THEMES below. review-gui renders in a headless
# sway at a fixed size and grim shoots it; review-tui draws off screen and writes each screen as
# ANSI text, which freeze lays out as a terminal window in SVG and resvg renders, with fonts
# downloaded at pinned versions, so the images do not depend on the fonts installed here.
#
# Every scene lands in target/screenshots/all/gui/ and target/screenshots/all/tui/, as
# <scene>.png in the dark theme and <scene>-light.png in the light one. Only the images README.md
# shows are committed: they are copied to docs/screenshots/ and compressed there, and any other
# image in docs/screenshots/ is deleted, so README.md alone decides what is committed. The
# gallery goes to target/screenshots/gallery/, which is not committed either.
#
# Needs sway and grim for review-gui, freeze, resvg and uv for review-tui, and pngquant and
# oxipng for both; mise.toml lists all but sway and grim.

set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
work="$root/target/screenshots"
mkdir -p "$work"

gallery=false
front_ends=(gui tui)
while (($#)); do
    case $1 in
        --gallery) gallery=true ;;
        --front-end)
            front_ends=("${2:?--front-end takes gui or tui}")
            shift
            ;;
        *)
            echo "screenshots: unknown argument $1" >&2
            exit 2
            ;;
    esac
    shift
done

# The committed images' themes, as `<theme>:<suffix>`: the default dark theme names the plain
# images, and GitHub Light, chosen for the clearest contrast and the most familiar diff colors,
# the `-light` ones.
THEMES=(carbonfox: github-light:-light)
# The diff styles the gallery shows each theme in.
STYLES=(plain bold soft)
# review-tui's screen in cells: wide enough for the files and a diff's longest lines, and tall
# enough for the demo's tree.
TUI_SIZE=150x44
# review-gui's window in logical pixels, shot at twice its pixels for sharp text.
GUI_SIZE=1600x1000
GUI_SCALE=2

# The fonts the terminal shots are drawn in, as `<url> <sha256>`.
JETBRAINS_MONO="https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip 6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf"
DEJAVU="https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2 fa9ca4d13871dd122f61258a80d01751d603b4d3ee14095d65453b4e846e17d7"
# The symbols review-tui draws come from these blocks of code points, which JetBrains Mono does
# not cover whole: arrows, technical symbols, box drawing, blocks, shapes, symbols and dingbats.
SYMBOLS="2190-21FF,2300-23FF,2500-27BF"

need() {
    local tool
    for tool in "$@"; do
        if ! command -v "$tool" >/dev/null; then
            echo "screenshots: $tool not found; run mise install, or install it" >&2
            exit 1
        fi
    done
}

for front_end in "${front_ends[@]}"; do
    case $front_end in
        gui) need sway grim pngquant oxipng ;;
        tui) need freeze resvg uv curl unzip tar sha256sum pngquant oxipng ;;
        *)
            echo "screenshots: --front-end is gui or tui, not $front_end" >&2
            exit 2
            ;;
    esac
done

# Downloads `$1` (`<url> <sha256>`) into target/screenshots/downloads/ unless it is there, and
# prints its path.
download() {
    local url=${1% *} sum=${1#* }
    local file="$work/downloads/${url##*/}"
    if [[ ! -f $file ]]; then
        mkdir -p "$work/downloads"
        timeout 300 curl -fsSL -o "$file.partial" "$url"
        mv "$file.partial" "$file"
    fi
    if ! echo "$sum  $file" | sha256sum --check --quiet; then
        rm -f "$file"
        echo "screenshots: $url does not have the expected checksum" >&2
        exit 1
    fi
    printf '%s\n' "$file"
}

# Fills target/screenshots/fonts/ with JetBrains Mono NL, which has no ligatures, in its four
# styles, each with the missing glyphs from the DejaVu Sans Mono of its weight.
prepare_fonts() {
    local fonts="$work/fonts" style donor
    # The fonts are made again whenever their sources or the merge change.
    local stamp
    stamp=$(printf '%s\n' "$JETBRAINS_MONO" "$DEJAVU" "$SYMBOLS" | cat - scripts/screenshots/fonts.py |
        sha256sum)
    if [[ -f $fonts/stamp && $(<"$fonts/stamp") == "$stamp" ]]; then
        return
    fi
    local jetbrains dejavu
    jetbrains=$(download "$JETBRAINS_MONO")
    dejavu=$(download "$DEJAVU")
    local unpacked="$work/fonts.unpacked"
    rm -rf "$unpacked" "$fonts"
    mkdir -p "$unpacked" "$fonts"
    unzip -q -j "$jetbrains" 'fonts/ttf/JetBrainsMonoNL-*.ttf' -d "$unpacked"
    tar -xjf "$dejavu" -C "$unpacked" --strip-components=2 \
        dejavu-fonts-ttf-2.37/ttf/DejaVuSansMono.ttf dejavu-fonts-ttf-2.37/ttf/DejaVuSansMono-Bold.ttf
    for style in Regular Bold Italic BoldItalic; do
        case $style in
            Bold*) donor="$unpacked/DejaVuSansMono-Bold.ttf" ;;
            *) donor="$unpacked/DejaVuSansMono.ttf" ;;
        esac
        timeout 300 uv run --quiet --no-project --with fonttools==4.68.0 python -I \
            scripts/screenshots/fonts.py "$unpacked/JetBrainsMonoNL-$style.ttf" "$donor" \
            "$fonts/JetBrainsMonoNL-$style.ttf" "$SYMBOLS"
    done
    rm -rf "$unpacked"
    # Written last, so an interrupted run prepares the fonts again.
    printf '%s\n' "$stamp" >"$fonts/stamp"
}

# Renders review-tui's screen `$1` (.ansi) as the PNG `$2`, in a window filled with the color of
# the screen's first cell, which is the theme's panel color.
render_tui() {
    local ansi=$1 png=$2
    local background
    background=$(head -n 1 "$ansi" | grep -oE '48;2;[0-9]+;[0-9]+;[0-9]+' | head -n 1 | {
        IFS=';' read -r _ _ red green blue
        printf '#%02x%02x%02x' "$red" "$green" "$blue"
    })
    local svg="${png%.png}.svg"
    # freeze's window settings match micromux's screenshots. freeze reads its input from
    # standard input when that is not a terminal, and would wait forever.
    freeze --language text --font.family "JetBrains Mono NL" --font.size 14 \
        --background "$background" --window --shadow.blur 20 --border.radius 8 --padding 20 \
        --output "$svg" "$ansi" </dev/null >/dev/null
    local warnings
    warnings=$(resvg --skip-system-fonts \
        --use-font-file "$work/fonts/JetBrainsMonoNL-Regular.ttf" \
        --use-font-file "$work/fonts/JetBrainsMonoNL-Bold.ttf" \
        --use-font-file "$work/fonts/JetBrainsMonoNL-Italic.ttf" \
        --use-font-file "$work/fonts/JetBrainsMonoNL-BoldItalic.ttf" \
        --zoom 2 "$svg" "$png" 2>&1)
    rm -f "$svg"
    # A glyph no font has draws as a box, and a fallback font draws its whole line in another
    # face; either needs the glyph's block in SYMBOLS. freeze clips to a mask it never defines,
    # which resvg warns about and ignores.
    if grep -E 'No fonts with|Fallback from' <<<"$warnings" | sort -u >&2; then
        echo "screenshots: ${png#"$root"/} draws a glyph the fonts lack" >&2
        exit 1
    fi
}

# Writes the PNG `$1` to `$2` with its colors reduced to a palette, which halves the size of these
# flat-colored screens with no difference visible even zoomed in, and then compresses it
# losslessly. Both steps give the same bytes for the same input. An image pngquant cannot
# reduce at that quality is copied whole.
compress() {
    local status=0
    pngquant --quality 90-100 --speed 1 --strip --force --output "$2" "$1" || status=$?
    case $status in
        0) ;;
        99) cp "$1" "$2" ;;
        *) exit "$status" ;;
    esac
    oxipng --quiet --opt 4 --strip safe "$2"
}

# Writes the configuration `$1` with the theme `$2`, the diff style `$3`, and the approved hunks
# and layout `$4` and `$5`.
write_config() {
    mkdir -p "$(dirname "$1")"
    cat >"$1" <<EOF
# Written by scripts/screenshots/run.sh for one run; the app may remember settings in it.
[theme]
name = "$2"

[diff]
style = "$3"
approved = "$4"
layout = "$5"
EOF
}

# Plays the QA script `$2` in front end `$1`, keeping its files under `$3`.
play() {
    local front_end=$1 script=$2 out=$3
    local -a env=(QA_FRONT_END="$front_end" QA_OUT="$out")
    if [[ $front_end == gui ]]; then
        env+=(QA_SIZE="$GUI_SIZE" QA_SCALE="$GUI_SCALE")
    else
        env+=(QA_RESIZE="$TUI_SIZE")
    fi
    rm -rf "$out"
    if ! env "${env[@]}" scripts/qa/run.sh "$script" >"$out.log" 2>&1; then
        tail -n 20 "$out.log" >&2
        exit 1
    fi
}

# Copies every shot under `$2`, from front end `$1`, into the directory `$3` as PNG images named
# after the shot with the suffix `$4`.
collect() {
    local front_end=$1 shots=$2 into=$3 suffix=$4
    local shot name
    mkdir -p "$into"
    if [[ $front_end == gui ]]; then
        for shot in "$shots"/*.png; do
            name=$(basename "$shot" .png)
            cp "$shot" "$into/$name$suffix.png"
            echo "wrote ${into#"$root"/}/$name$suffix.png"
        done
    else
        for shot in "$shots"/*.ansi; do
            name=$(basename "$shot" .ansi)
            render_tui "$shot" "$into/$name$suffix.png"
            echo "wrote ${into#"$root"/}/$name$suffix.png"
        done
    fi
}

# Every scene, in every theme of THEMES, then the ones README.md shows into docs/screenshots/.
screenshots() {
    local front_end=$1 entry theme suffix
    local into="$work/all/$front_end"
    rm -rf "$into"
    for entry in "${THEMES[@]}"; do
        theme=${entry%%:*}
        suffix=${entry#*:}
        # scenes.qa reads these paths.
        write_config "$work/config/folded.toml" "$theme" bold folded unified
        write_config "$work/config/shown.toml" "$theme" bold shown unified
        write_config "$work/config/side-by-side.toml" "$theme" bold folded side-by-side
        play "$front_end" scripts/screenshots/scenes.qa "$work/$front_end-$theme"
        collect "$front_end" "$work/$front_end-$theme" "$into" "$suffix"
    done
    publish "$front_end"
}

# Copies the images of front end `$1` that README.md shows from target/screenshots/all/ into
# docs/screenshots/, compressed, and deletes the others there.
publish() {
    local front_end=$1 path
    local into="$root/docs/screenshots/$front_end"
    local -a shown=()
    mapfile -t shown < <(grep -oE "docs/screenshots/$front_end/[A-Za-z0-9._-]+\.png" README.md | sort -u)
    for path in "${shown[@]}"; do
        if [[ ! -f $work/all/$front_end/${path##*/} ]]; then
            echo "screenshots: README.md shows $path, which no scene in scripts/screenshots/scenes.qa renders" >&2
            exit 1
        fi
    done
    mkdir -p "$into"
    for path in "$into"/*.png; do
        if [[ -f $path ]] && ! printf '%s\n' "${shown[@]}" | grep -qxF "${path#"$root"/}"; then
            rm "$path"
            echo "deleted ${path#"$root"/}"
        fi
    done
    for path in "${shown[@]}"; do
        compress "$work/all/$front_end/${path##*/}" "$root/$path"
        echo "wrote $path"
    done
}

# One scene, a diff with an approved hunk, its note and a reformatted hunk shown, in every theme
# and diff style.
gallery() {
    local front_end=$1 file theme style config
    local script="$work/gallery.qa" launch="--demo"
    : >"$script"
    for file in crates/review-ui/themes/*.toml; do
        theme=$(basename "$file" .toml)
        for style in "${STYLES[@]}"; do
            config="$work/gallery-config/$theme-$style.toml"
            write_config "$config" "$theme" "$style" shown unified
            # Only the first launch builds the demo; the others reopen it.
            printf '%s\n' "launch $launch --config $config" "file src/engine.rs" "key 0" \
                "key k k" "shot $theme-$style" >>"$script"
            launch="--demo-revision 1"
        done
    done
    play "$front_end" "$script" "$work/gallery-$front_end"
    rm -rf "$work/gallery/$front_end"
    collect "$front_end" "$work/gallery-$front_end" "$work/gallery/$front_end" ""
}

if [[ " ${front_ends[*]} " == *" tui "* ]]; then
    prepare_fonts
fi
for front_end in "${front_ends[@]}"; do
    if $gallery; then
        gallery "$front_end"
    else
        screenshots "$front_end"
    fi
done
