#!/usr/bin/env bash
#
# build_camilladsp_pi.sh - Cross-compile and install CamillaDSP 5.0.0-beta.2 for Raspberry Pi 3.
#
# Clones HEnquist/camilladsp at the pinned v5.0.0-beta.2 tag, applies the
# GLIBC_2.17 log10f symbol-versioning directive (matching crates/common/src/glibc.rs)
# so the binary runs on Debian 13 Trixie (glibc 2.41), cross-compiles for
# aarch64-unknown-linux-gnu (Cortex-A53, NEON, LTO), validates the target config
# on the Pi, backs up any previous binary, installs to /usr/local/bin/camilladsp,
# and restarts camilladsp.service.
#
# Usage:
#   scripts/build_camilladsp_pi.sh               # build, validate, install & restart
#   scripts/build_camilladsp_pi.sh --build-only  # cross-compile only (no SSH deploy)
#
# Environment overrides:
#   REMOTE_HOST (default: root@192.168.2.120)
#   BUILD_DIR   (default: /tmp/camilladsp-5b2)

set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.2.120}"
CAMILLADSP_TAG="v5.0.0-beta.2"
CAMILLADSP_REV="1ea8b9c5518bf9df7ae612d3b51bd791ceb46269"
CAMILLADSP_REPO="https://github.com/HEnquist/camilladsp.git"
BUILD_DIR="${BUILD_DIR:-/tmp/camilladsp-5b2}"

BUILD_ONLY=0
if [[ "${1:-}" == "--build-only" ]]; then
  BUILD_ONLY=1
elif [[ -n "${1:-}" ]]; then
  echo "Usage: $0 [--build-only]" >&2
  exit 1
fi

if [[ ! -d "${BUILD_DIR}/.git" ]]; then
  echo "[camilladsp] Cloning ${CAMILLADSP_REPO} (${CAMILLADSP_TAG}) -> ${BUILD_DIR}"
  git clone --depth 1 --branch "${CAMILLADSP_TAG}" "${CAMILLADSP_REPO}" "${BUILD_DIR}"
fi

cd "${BUILD_DIR}"
HEAD_REV="$(git rev-parse HEAD)"
if [[ "${HEAD_REV}" != "${CAMILLADSP_REV}" ]]; then
  echo "[camilladsp] Unexpected revision ${HEAD_REV} in ${BUILD_DIR} (expected ${CAMILLADSP_REV})" >&2
  exit 1
fi

# Enforce GLIBC_2.17 symbol versioning for log10f so the binary runs on Debian 13 (glibc 2.41).
if ! grep -q 'log10f@GLIBC_2.17' src/bin.rs; then
  sed -i '/#\[macro_use\]/i #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]\ncore::arch::global_asm!(".symver log10f, log10f@GLIBC_2.17");\n' src/bin.rs
  echo "[camilladsp] Applied GLIBC_2.17 log10f symver to src/bin.rs."
fi

echo "[camilladsp] Building ${CAMILLADSP_TAG} for aarch64-unknown-linux-gnu (Cortex-A53)..."
CARGO_PROFILE_RELEASE_LTO=fat \
CARGO_PROFILE_RELEASE_STRIP=true \
PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu=/ \
PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
RUSTFLAGS="-C target-cpu=cortex-a53 -C target-feature=+neon,-aes,-sha2 -C link-arg=-Wl,--as-needed -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,-O1" \
  cargo build --target aarch64-unknown-linux-gnu --release

BIN_PATH="${PWD}/target/aarch64-unknown-linux-gnu/release/camilladsp"
if [[ ! -f "${BIN_PATH}" ]]; then
  echo "[camilladsp] Build artifact not found: ${BIN_PATH}" >&2
  exit 1
fi

OBJDUMP_OUT="$(aarch64-linux-gnu-objdump -T "${BIN_PATH}")"
if grep -qE 'GLIBC_2\.(4[2-9]|[5-9][0-9])' <<<"${OBJDUMP_OUT}"; then
  echo "[camilladsp] ERROR: binary references GLIBC > 2.41 symbols:" >&2
  grep -E 'GLIBC_2\.(4[2-9]|[5-9][0-9])' <<<"${OBJDUMP_OUT}" >&2
  exit 1
fi

echo "[camilladsp] Built OK: ${BIN_PATH}"

if [[ "${BUILD_ONLY}" == "1" ]]; then
  exit 0
fi

REMOTE_STAGE_DIR="$(ssh "${REMOTE_HOST}" mktemp -d)"
REMOTE_TMP="${REMOTE_STAGE_DIR}/camilladsp"
scp "${BIN_PATH}" "${REMOTE_HOST}:${REMOTE_TMP}"

ssh "${REMOTE_HOST}" bash -s -- "${REMOTE_TMP}" "${REMOTE_STAGE_DIR}" <<'EOF'
set -euo pipefail

REMOTE_TMP="$1"
REMOTE_STAGE_DIR="$2"
chmod +x "${REMOTE_TMP}"

"${REMOTE_TMP}" --version
"${REMOTE_TMP}" -c /etc/camilladsp/crossover.yml

cleanup_and_restart() {
  rm -rf "${REMOTE_STAGE_DIR}" || true
  systemctl start camilladsp.service || true
}
trap cleanup_and_restart EXIT

systemctl stop camilladsp.service || true
if [[ -f /usr/local/bin/camilladsp && ! -f /usr/local/bin/camilladsp-4.1.3 ]]; then
  if /usr/local/bin/camilladsp --version 2>/dev/null | grep -q '4\.1\.3'; then
    cp -a /usr/local/bin/camilladsp /usr/local/bin/camilladsp-4.1.3
  fi
fi

mv "${REMOTE_TMP}" /usr/local/bin/camilladsp
rm -rf "${REMOTE_STAGE_DIR}"
trap - EXIT
systemctl start camilladsp.service
systemctl status camilladsp.service --no-pager -l | head -n 12
EOF
