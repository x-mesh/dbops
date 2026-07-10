#!/usr/bin/env bash
# dbops release build: builds this project's shipped targets (the native
# host + 2 static-linked musl Linux targets), gates each build with the
# same static-linking/crypto-backend checks that must never regress (see
# docs/build-spike.md), and packages the results into scp-ready artifacts
# under dist/.
#
# Usage: scripts/release-build.sh [--target TRIPLE] [--skip-docker-gate]
#
#   --target TRIPLE      Build (and gate/package) only this one target
#                         instead of all three. Must be the host's own
#                         triple (native `cargo build`) or one of
#                         x86_64-unknown-linux-musl /
#                         aarch64-unknown-linux-musl (cross via
#                         cargo-zigbuild). This is what the release CI
#                         matrix uses to reuse this script per-job; a
#                         local one-command run omits it and gets all
#                         three.
#   --skip-docker-gate    Skip the Alpine-container ldd/--version proof for
#                         the musl binaries. The cargo-tree crypto-backend
#                         gate still runs. Use this only when Docker isn't
#                         available locally; the release CI job must not
#                         set this.
#
# Requires: cargo, cargo-zigbuild (+ zig) for the musl targets, the
# x86_64-unknown-linux-musl and aarch64-unknown-linux-musl rustup targets,
# and (unless skipped) Docker with multi-arch emulation for the Alpine
# gate.

set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

log() { printf '[release-build] %s\n' "$1"; }
fail() {
  printf '[release-build] FAIL: %s\n' "$1" >&2
  exit 1
}

SKIP_DOCKER_GATE=0
TARGET_FILTER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-docker-gate)
      SKIP_DOCKER_GATE=1
      shift
      ;;
    --target)
      [ $# -ge 2 ] || fail "--target requires a value"
      TARGET_FILTER="$2"
      shift 2
      ;;
    --target=*)
      TARGET_FILTER="${1#--target=}"
      shift
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

VERSION=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
HOST_TRIPLE=$(rustc -vV | awk '/^host:/{print $2}')
MUSL_TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
DIST_DIR="dist"

is_musl_target() {
  case "$1" in
    x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) return 0 ;;
    *) return 1 ;;
  esac
}

# Which targets this run builds/gates/packages: everything by default (the
# local "one command, all three artifacts" path), or a single target when
# --target narrows it (the CI matrix path -- one job per target, each
# reusing this same script instead of duplicating its gate logic in YAML).
BUILD_TARGETS=()
if [ -n "$TARGET_FILTER" ]; then
  if [ "$TARGET_FILTER" = "$HOST_TRIPLE" ] || is_musl_target "$TARGET_FILTER"; then
    BUILD_TARGETS=("$TARGET_FILTER")
  else
    fail "unsupported --target '$TARGET_FILTER' (expected $HOST_TRIPLE, x86_64-unknown-linux-musl, or aarch64-unknown-linux-musl)"
  fi
else
  BUILD_TARGETS=("$HOST_TRIPLE" "${MUSL_TARGETS[@]}")
fi

bin_path_for() {
  local target="$1"
  if [ "$target" = "$HOST_TRIPLE" ]; then
    echo "target/release/dbops"
  else
    echo "target/$target/release/dbops"
  fi
}

# Alpine needs the matching --platform to run a foreign-arch binary under
# QEMU emulation; kept as a case statement instead of an associative array
# -- a stock macOS /bin/bash (3.2) has none, and this script only assumes
# POSIX-ish bash, not a specific major version.
docker_platform_for() {
  case "$1" in
    x86_64-unknown-linux-musl) echo "linux/amd64" ;;
    aarch64-unknown-linux-musl) echo "linux/arm64" ;;
    *) fail "no docker platform mapping for target: $1" ;;
  esac
}

sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$@"
  else
    sha256sum "$@"
  fi
}

log "version $VERSION, host target $HOST_TRIPLE, building: ${BUILD_TARGETS[*]}"

# --- build --------------------------------------------------------------

for target in "${BUILD_TARGETS[@]}"; do
  if [ "$target" = "$HOST_TRIPLE" ]; then
    log "building host target ($target)..."
    cargo build --release
  else
    log "building $target via cargo zigbuild..."
    cargo zigbuild --release --target "$target"
  fi
done

# --- gate 1: no openssl-sys / aws-lc-sys anywhere in the dependency graph -
#
# rustls is pinned to the ring crypto backend everywhere in Cargo.toml
# specifically so neither of these ever enters the graph (aws-lc-sys has a
# documented musl cross-compile regression; openssl-sys would break the
# "no external library links" static-binary requirement outright). `cargo
# tree -i <pkg>` exits non-zero with "did not match any packages" when the
# package is absent -- that failure is the pass case here.

log "gate: crypto backend (no openssl-sys / aws-lc-sys)..."
check_absent() {
  local target_flag="$1" pkg="$2"
  # $target_flag is intentionally either empty or a single "--target
  # <triple>" pair, not a value needing quoting -- word-splitting it here
  # is what lets an empty string vanish instead of passing cargo an empty
  # argument.
  # shellcheck disable=SC2086
  if cargo tree $target_flag -i "$pkg" >/dev/null 2>&1; then
    fail "$pkg found in dependency graph ($target_flag) -- see docs/build-spike.md"
  fi
}
for target in "${BUILD_TARGETS[@]}"; do
  if [ "$target" = "$HOST_TRIPLE" ]; then
    check_absent "" openssl-sys
    check_absent "" aws-lc-sys
  else
    check_absent "--target $target" openssl-sys
    check_absent "--target $target" aws-lc-sys
  fi
done
log "gate passed: neither crate is in the dependency graph on any built target"

# --- gate 2: static linking proof under Alpine (musl targets only) -------
#
# A dynamically-linked binary would make `ldd` print a list of .so
# dependencies; Alpine's musl ldd instead refuses a static binary outright
# ("not a valid dynamic program" and similar wording across versions) --
# that refusal is the proof this gate is checking for, not an error. The
# host binary has no equivalent proof (it isn't statically linked, nor
# meant to be -- only the linux musl artifacts ship to servers).

if [ "$SKIP_DOCKER_GATE" -eq 1 ]; then
  log "skipping Alpine static-link gate (--skip-docker-gate)"
else
  log "gate: static linking proof under Alpine..."
  for target in "${BUILD_TARGETS[@]}"; do
    is_musl_target "$target" || continue
    bin=$(bin_path_for "$target")
    platform=$(docker_platform_for "$target")
    out=$(docker run --rm --platform "$platform" -v "$(pwd)/$bin:/dbops:ro" alpine \
      sh -c '(ldd /dbops 2>&1 || true); /dbops --version') \
      || fail "$target: alpine container run failed"
    echo "$out" | grep -qiE 'not a (valid )?dynamic (program|executable)' \
      || fail "$target: ldd did not report static linking -- got: $out"
    echo "$out" | grep -q "dbops $VERSION" \
      || fail "$target: --version did not report 'dbops $VERSION' -- got: $out"
    log "  $target: static (ldd refuses it), --version reports $VERSION"
  done
fi

# --- package artifacts ----------------------------------------------------

log "packaging dist/ artifacts..."
# Only wipe dist/ on a full (no --target) run -- a single-target CI matrix
# leg must not delete artifacts a sibling job (or a prior local invocation
# simulating the matrix) already placed there.
if [ -z "$TARGET_FILTER" ]; then
  rm -rf "$DIST_DIR"
fi
mkdir -p "$DIST_DIR"

package() {
  local target="$1" bin_path="$2"
  local out="$DIST_DIR/dbops-$VERSION-$target"
  cp "$bin_path" "$out"
  chmod +x "$out"
  log "  $out ($(du -h "$out" | cut -f1 | tr -d ' '))"
}

for target in "${BUILD_TARGETS[@]}"; do
  package "$target" "$(bin_path_for "$target")"
done

( cd "$DIST_DIR" && sha256 dbops-"$VERSION"-* > SHA256SUMS )

log "done -- ${#BUILD_TARGETS[@]} artifact(s) in $DIST_DIR/ (+ SHA256SUMS)"
