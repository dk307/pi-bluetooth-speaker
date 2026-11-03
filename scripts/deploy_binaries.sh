#!/usr/bin/env bash
#
# deploy_binaries.sh - Deploy release-built binaries to the Raspberry Pi 3.
#
# Copies binaries built for aarch64-unknown-linux-gnu from the local release
# build output to /usr/local/bin on the target host (root@192.168.2.120),
# verifying sha256 integrity on both ends and setting executable permissions.
#
# Usage:
#   scripts/deploy_binaries.sh            # deploy all known binaries
#   scripts/deploy_binaries.sh <name>...  # deploy selected binaries only
#
# This script is intentionally the only sanctioned way to install release
# binaries to the live device (see AGENTS.md). It never touches configs,
# running services, or anything outside /usr/local/bin on the remote host.

set -euo pipefail

REMOTE_HOST="root@192.168.2.120"
REMOTE_DIR="/usr/local/bin"
REMOTE_TMP="/tmp"
TARGET_TRIPLE="aarch64-unknown-linux-gnu"
RELEASE_DIR="target/${TARGET_TRIPLE}/release"

# All known deployable binaries produced by the workspace. Extend this list
# when new binaries are added to services/core.
KNOWN_BINARIES=(
  core
  bt-manager
  temp-read
  light-read
  amp-info
  bt-info
)

# Prefer git worktrees: binaries are built per worktree, so resolve paths
# relative to the directory this script lives in rather than $PWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASE_PATH="${SCRIPT_DIR}/../${RELEASE_DIR}"

log() { echo "[deploy] $*"; }
die() { echo "[deploy] ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Validate build environment and binaries before touching the remote host.
# ---------------------------------------------------------------------------

[[ -d "${RELEASE_PATH}" ]] || die "Release directory not found: ${RELEASE_PATH}.
Run: cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target ${TARGET_TRIPLE} --release --bins"

select_binaries() {
  if [[ $# -eq 0 ]]; then
    printf '%s\n' "${KNOWN_BINARIES[@]}"
  else
    for requested in "$@"; do
      known=0
      for candidate in "${KNOWN_BINARIES[@]}"; do
        [[ "${requested}" == "${candidate}" ]] && known=1
      done
      [[ "${known}" -eq 1 ]] || die "Unknown binary '${requested}'. Known binaries: ${KNOWN_BINARIES[*]}"
    done
    printf '%s\n' "$@"
  fi
}

# ---------------------------------------------------------------------------
# Per-binary deployment: local hash -> scp -> remote sha256 -> chmod -> verify.
# ---------------------------------------------------------------------------

deploy_binary() {
  local name="$1"
  local local_path="${RELEASE_PATH}/${name}"
  local local_hash remote_hash

  [[ -f "${local_path}" ]] || die "Binary not found in release output: ${local_path}"
  [[ -x "${local_path}" ]] || die "Binary is not executable: ${local_path}"

  local_hash="$(sha256sum "${local_path}" | awk '{print $1}')"
  log "Deploying '${name}' (sha256 ${local_hash:0:12}...) -> ${REMOTE_HOST}:${REMOTE_DIR}/${name}"

  # Stage into remote /tmp first (non-destructive), verify integrity there,
  # then move into /usr/local/bin atomically.
  scp -q "${local_path}" "${REMOTE_HOST}:${REMOTE_TMP}/.deploy_${name}.$$"

  remote_hash="$(ssh "${REMOTE_HOST}" "sha256sum ${REMOTE_TMP}/.deploy_${name}.$$ | awk '{print \$1}'")"
  [[ "${local_hash}" == "${remote_hash}" ]] || die "sha256 mismatch for '${name}' after transfer (local=${local_hash} remote=${remote_hash})"

  ssh "${REMOTE_HOST}" "mv ${REMOTE_TMP}/.deploy_${name}.$$ ${REMOTE_DIR}/${name} && chmod 755 ${REMOTE_DIR}/${name} && sha256sum ${REMOTE_DIR}/${name}" \
    | awk '{print $1}' > /dev/null

  # Final end-to-end verification of the installed file.
  remote_hash="$(ssh "${REMOTE_HOST}" "sha256sum ${REMOTE_DIR}/${name} | awk '{print \$1}'")"
  [[ "${local_hash}" == "${remote_hash}" ]] || die "sha256 mismatch for installed '${name}' (local=${local_hash} remote=${remote_hash})"

  log "OK: '${name}' deployed and verified."
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

log "Target: ${REMOTE_HOST}:${REMOTE_DIR}"
BINARIES="$(select_binaries "$@")"

for name in ${BINARIES}; do
  deploy_binary "${name}"
done

log "All requested binaries deployed successfully."
