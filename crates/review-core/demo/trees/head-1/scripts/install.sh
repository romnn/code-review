#!/usr/bin/env bash
# Installs the ledger CLI from a release archive into a prefix.
set -euo pipefail

readonly REPO="example/ledger"
readonly DEFAULT_PREFIX="${HOME}/.local"
VERSION="${LEDGER_VERSION:-latest}"
PREFIX="${PREFIX:-$DEFAULT_PREFIX}"
declare -a CLEANUP=()

log() { printf '\033[1m==>\033[0m %s\n' "$*" >&2; }
die() {
  log "error: $*"
  exit 1
}

cleanup() {
  local path
  for path in "${CLEANUP[@]}"; do
    rm -rf -- "$path"
  done
}
trap cleanup EXIT
trap 'die "interrupted"' INT TERM

detect_platform() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
  esac
  [[ "$os" == "darwin" || "$os" == "linux" ]] || die "unsupported system: $os"
  echo "${arch}-${os}"
}

install_binary() {
  local platform="$1" workdir archive url file
  workdir="$(mktemp -d)"
  CLEANUP+=("$workdir")
  archive="ledger_${VERSION#v}_${platform}.tar.gz"
  url="https://example.com/${REPO}/releases/download/${VERSION}"
  log "downloading ${archive}"
  for file in "$archive" checksums.txt; do
    curl --fail --silent --location --output "${workdir}/${file}" "${url}/${file}"
  done
  (cd "$workdir" && grep -F "$archive" checksums.txt | sha256sum --check --quiet) ||
    die "checksum mismatch for ${archive}"
  tar -xzf "${workdir}/${archive}" -C "$workdir"
  install -Dm755 "${workdir}/ledger" "${PREFIX}/bin/ledger"
}

write_config() {
  local config="${XDG_CONFIG_HOME:-$HOME/.config}/ledger/config.toml"
  [[ -f "$config" ]] && return 0
  mkdir -p "${config%/*}"
  cat >"$config" <<EOF
# Written by install.sh on $(date +%F).
currency = "${LEDGER_CURRENCY:-EUR}"
precision = 2
EOF
  log "wrote ${config}"
}

main() {
  while (($#)); do
    case "$1" in
      --prefix)
        PREFIX="$2"
        shift 2
        ;;
      --version)
        VERSION="$2"
        shift 2
        ;;
      -h | --help)
        echo "usage: install.sh [--prefix DIR] [--version TAG]"
        exit 0
        ;;
      *) die "unknown option: $1" ;;
    esac
  done
  local platform
  platform="$(detect_platform)"
  install_binary "$platform"
  write_config
  log "installed ledger ${VERSION} to ${PREFIX}/bin"
}

main "$@"
