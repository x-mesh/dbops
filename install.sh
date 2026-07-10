#!/bin/sh
# dbops installer: detects this host's platform, downloads the matching
# artifact from the newest GitHub release, verifies its SHA256, and installs
# it as a `dbops` binary.
#
#   curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh
#
# Once installed, `dbops update` does the same job from inside the binary --
# this script exists only to get the first copy onto a machine.
#
# Environment:
#   DBOPS_VERSION       Release tag to install (e.g. v0.2.0). Default: latest.
#   DBOPS_INSTALL_DIR   Where to put the binary. Default: /usr/local/bin if
#                       writable, else ~/.local/bin.
#   DBOPS_REPO          owner/name to install from. Default: x-mesh/dbops.
#   DBOPS_GITHUB_TOKEN  GitHub token. GITHUB_TOKEN / GH_TOKEN are also read,
#   GITHUB_TOKEN        as is `gh auth token`. Required while the repository
#   GH_TOKEN            is private; optional (but rate-limit-friendly) once
#                       it is public.
#
# POSIX sh on purpose: this is the one thing that has to run on a box before
# anything else is known to be there.

set -eu

DEFAULT_REPO="x-mesh/dbops"
API_BASE="https://api.github.com"
BIN_NAME="dbops"
CHECKSUM_ASSET="SHA256SUMS"

REPO="${DBOPS_REPO:-$DEFAULT_REPO}"
VERSION="${DBOPS_VERSION:-}"
INSTALL_DIR="${DBOPS_INSTALL_DIR:-}"

# Populated by main(). WORK_DIR is cleaned up by the EXIT trap.
WORK_DIR=""
TOKEN=""

# --- output ----------------------------------------------------------------

# Progress goes to stderr so `install.sh | ...` still pipes cleanly and a
# `curl | sh` run shows its progress even when stdout is captured.
info() { printf '==> %s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

cleanup() {
  [ -n "$WORK_DIR" ] && rm -rf "$WORK_DIR"
  return 0
}

usage() {
  cat <<EOF
Usage: install.sh [--version TAG] [--dir PATH]

  --version TAG   Install this release tag (default: the newest release)
  --dir PATH      Install into PATH (default: /usr/local/bin, else ~/.local/bin)
  -h, --help      Show this message

Every flag also has an environment variable (DBOPS_VERSION, DBOPS_INSTALL_DIR).
EOF
}

# --- platform detection -----------------------------------------------------

# Print the release artifact triple for this host.
#
# The release workflow publishes exactly three artifacts; anything else is a
# hard stop rather than a wrong-architecture download that only fails at exec
# time with "cannot execute binary file".
detect_target() {
  os=$(uname -s)
  arch=$(uname -m)

  case "$os" in
    Darwin)
      # A shell running under Rosetta 2 on Apple silicon reports x86_64, but
      # the hardware -- and the artifact that should be installed -- is arm64.
      # sysctl.proc_translated is 1 exactly in that case.
      if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
        arch="arm64"
      fi
      case "$arch" in
        arm64 | aarch64) printf 'aarch64-apple-darwin\n' ;;
        x86_64) die "no published artifact for Intel macOS; build from source with \`cargo build --release\`" ;;
        *) die "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    Linux)
      # The Linux artifacts are statically linked against musl, so one build
      # per architecture covers glibc distros and Alpine alike.
      case "$arch" in
        x86_64 | amd64) printf 'x86_64-unknown-linux-musl\n' ;;
        aarch64 | arm64) printf 'aarch64-unknown-linux-musl\n' ;;
        *) die "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    *)
      die "unsupported operating system: $os (dbops ships macOS and Linux builds)"
      ;;
  esac
}

# --- http -------------------------------------------------------------------

resolve_token() {
  for candidate in "${DBOPS_GITHUB_TOKEN:-}" "${GITHUB_TOKEN:-}" "${GH_TOKEN:-}"; do
    if [ -n "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  # Last resort: the `gh` CLI already holds a token for anyone who has ever
  # run `gh auth login`, which is most people who can read a private repo.
  if have gh; then
    gh auth token 2>/dev/null || true
  fi
}

# fetch <url> <accept> <outfile>
#
# Both curl and wget are accepted because a minimal Debian or Alpine image
# usually ships exactly one of them.
#
# The assets API answers a download with a redirect to a signed URL on
# another host. `--location` follows it; neither client forwards the
# Authorization header across that origin, which is what keeps the GitHub
# token off the asset CDN.
fetch() {
  _url=$1
  _accept=$2
  _out=$3

  if have curl; then
    set -- --fail --silent --show-error --location --retry 3 --retry-delay 1 \
      --header "Accept: $_accept" --header "User-Agent: dbops-install"
    if [ -n "$TOKEN" ]; then
      set -- "$@" --header "Authorization: Bearer $TOKEN"
    fi
    curl "$@" --output "$_out" -- "$_url"
  elif have wget; then
    set -- --quiet --tries=3 --header="Accept: $_accept" --header="User-Agent: dbops-install"
    if [ -n "$TOKEN" ]; then
      set -- "$@" --header="Authorization: Bearer $TOKEN"
    fi
    wget "$@" -O "$_out" -- "$_url"
  else
    die "neither curl nor wget is installed"
  fi
}

api_failed() {
  if [ -n "$TOKEN" ]; then
    die "GitHub API request failed for $REPO -- check the token can read that repository (needs \`contents: read\`), and that release '${VERSION:-latest}' exists"
  fi
  die "GitHub API request failed for $REPO -- if it is private, export GITHUB_TOKEN with read access; unauthenticated calls are also capped at 60/hour"
}

# --- release metadata --------------------------------------------------------

# Read the release JSON for $VERSION (or the newest release) into $1.
fetch_release_json() {
  if [ -n "$VERSION" ]; then
    _url="$API_BASE/repos/$REPO/releases/tags/$VERSION"
  else
    _url="$API_BASE/repos/$REPO/releases/latest"
  fi
  fetch "$_url" "application/vnd.github+json" "$1" || api_failed
}

# Print the release's tag_name, given the release JSON file.
#
# tag_name appears near the top of the release object, well before the
# free-form `body` -- and a `body` that happens to contain the text
# "tag_name" would have its quotes backslash-escaped, so it cannot match.
release_tag() {
  tr ',' '\n' <"$1" |
    grep -E '"tag_name"[[:space:]]*:' |
    head -n 1 |
    sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/'
}

# Print the assets-API URL of the asset named $2, given the release JSON in $1.
#
# The assets API (`/releases/assets/<id>` + `Accept: application/octet-stream`)
# is used rather than each asset's browser_download_url because it is the only
# form that works for a private repository -- and it works unauthenticated for
# a public one too, so there is one download path to reason about.
#
# GitHub serializes an asset as {"url":".../releases/assets/<id>","id":...,
# "name":"..."}, and every nested object opens a fresh `{`, so splitting the
# payload on `{` puts one asset's url and name together on a single line.
asset_url() {
  tr '{' '\n' <"$1" |
    grep -E "\"name\"[[:space:]]*:[[:space:]]*\"$2\"" |
    grep -oE "$API_BASE/repos/[^\"]+/releases/assets/[0-9]+" |
    head -n 1
}

# --- checksum ----------------------------------------------------------------

sha256_of() {
  if have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif have shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif have openssl; then
    openssl dgst -sha256 "$1" | awk '{print $NF}'
  else
    die "no sha256 tool found (need one of: sha256sum, shasum, openssl)"
  fi
}

# Print the expected digest of asset $2 from the SHA256SUMS file $1.
#
# Two producers write that file with different path spellings: the local
# scripts/release-build.sh emits bare names, while the release workflow's
# `find | xargs shasum` emits `./`-prefixed ones. Compare only the final path
# component so both parse. A leading `*` (sha256sum's binary-mode marker) is
# stripped for the same reason.
expected_digest() {
  awk -v want="$2" '
    { path = $NF; sub(/^\.\//, "", path); sub(/^\*/, "", path)
      if (path == want) { print $1; exit } }
  ' "$1"
}

# --- install -----------------------------------------------------------------

choose_install_dir() {
  if [ -n "$INSTALL_DIR" ]; then
    printf '%s\n' "$INSTALL_DIR"
  elif [ -w /usr/local/bin ]; then
    printf '%s\n' /usr/local/bin
  else
    printf '%s\n' "$HOME/.local/bin"
  fi
}

# Move $1 into place as $2/dbops.
#
# The binary is staged inside the destination directory and renamed into
# place, so the swap is a same-filesystem atomic rename. That also makes it
# safe to overwrite a `dbops` that is currently running: rename replaces the
# directory entry, and a running process keeps its already-open inode.
install_binary() {
  _src=$1
  _dir=$2
  _dest="$_dir/$BIN_NAME"
  _staged="$_dir/.$BIN_NAME.install.$$"

  mkdir -p "$_dir" 2>/dev/null || die "cannot create $_dir"
  if ! cp "$_src" "$_staged" 2>/dev/null; then
    die "cannot write into $_dir -- re-run with sudo, or set DBOPS_INSTALL_DIR to a directory you own (e.g. \$HOME/.local/bin)"
  fi
  chmod 0755 "$_staged"
  mv -f "$_staged" "$_dest" || {
    rm -f "$_staged"
    die "cannot replace $_dest"
  }
  printf '%s\n' "$_dest"
}

warn_if_not_on_path() {
  case ":${PATH:-}:" in
    *":$1:"*) ;;
    *) warn "$1 is not on your PATH -- add it, e.g. \`export PATH=\"$1:\$PATH\"\`" ;;
  esac
}

# --- main --------------------------------------------------------------------

main() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --version)
        [ $# -ge 2 ] || die "--version requires a value"
        VERSION=$2
        shift 2
        ;;
      --version=*)
        VERSION=${1#--version=}
        shift
        ;;
      --dir)
        [ $# -ge 2 ] || die "--dir requires a value"
        INSTALL_DIR=$2
        shift 2
        ;;
      --dir=*)
        INSTALL_DIR=${1#--dir=}
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1 (try --help)"
        ;;
    esac
  done

  TOKEN=$(resolve_token)
  target=$(detect_target)

  trap cleanup EXIT INT TERM
  WORK_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t dbops-install)

  info "resolving ${VERSION:-latest} release of $REPO"
  release_json="$WORK_DIR/release.json"
  fetch_release_json "$release_json"

  tag=$(release_tag "$release_json")
  [ -n "$tag" ] || die "could not read a tag_name out of the release metadata"

  # Artifacts are named `dbops-<cargo version>-<triple>` by
  # scripts/release-build.sh, and tags are that version with a `v` prefix.
  version=${tag#v}
  asset="$BIN_NAME-$version-$target"
  info "installing $tag ($asset)"

  asset_api_url=$(asset_url "$release_json" "$asset")
  [ -n "$asset_api_url" ] || die "release $tag has no asset named $asset"
  sums_api_url=$(asset_url "$release_json" "$CHECKSUM_ASSET")
  [ -n "$sums_api_url" ] || die "release $tag has no $CHECKSUM_ASSET asset"

  fetch "$sums_api_url" "application/octet-stream" "$WORK_DIR/$CHECKSUM_ASSET" || api_failed
  fetch "$asset_api_url" "application/octet-stream" "$WORK_DIR/$asset" || api_failed

  expected=$(expected_digest "$WORK_DIR/$CHECKSUM_ASSET" "$asset")
  [ -n "$expected" ] || die "no $CHECKSUM_ASSET entry for $asset"
  actual=$(sha256_of "$WORK_DIR/$asset")
  [ "$expected" = "$actual" ] || die "checksum mismatch for $asset: expected $expected, got $actual"
  info "checksum ok"

  dir=$(choose_install_dir)
  dest=$(install_binary "$WORK_DIR/$asset" "$dir")
  info "installed $("$dest" --version) to $dest"
  warn_if_not_on_path "$dir"
  info "run \`$BIN_NAME update\` to upgrade in place from now on"
}

main "$@"
