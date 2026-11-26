#!/usr/bin/env bash
#
# build_rmpd_pi.sh - Cross-compile and deploy rmpd (0.19.0) for Raspberry Pi 3.
#
# Clones M0Rf30/rmpd at the pinned 0.19.0 tag (SHA-verified), checks for any
# GLIBC > 2.41 symbol bindings (applying .symver *@GLIBC_2.17 if needed),
# cross-compiles for aarch64-unknown-linux-gnu (Cortex-A53, NEON, LTO) with
# streaming features (http-api,jellyfin,podcast,radio), and verifies the ELF
# ABI against the Debian 13 Trixie (glibc 2.41) Pi target.
#
# Phases (default: preflight, build, verify):
#   preflight  verify host Rust/cross-compile toolchain and arm64 alsa.pc
#   build      clone + pin-verify rmpd, cross-compile release binary
#   verify     ABI gate: aarch64 ELF, GLIBC <= 2.41, NEEDED sonames present on Pi
#   apply      install /usr/local/bin/rmpd, add dietpi to audio group, enable
#              linger, install user unit & seed rmpd.toml if missing, start
#              rmpd.service (REQUIRES --i-have-approval)
#   all        preflight + build + verify, then apply (requires --i-have-approval)
#
# Default phases write ONLY under ${BUILD_DIR} locally and perform read-only
# checks on the Pi. Nothing on the Pi is modified unless --phase apply or --all
# is passed with --i-have-approval.
#
# Usage:
#   scripts/build_rmpd_pi.sh
#   scripts/build_rmpd_pi.sh --phase build
#   scripts/build_rmpd_pi.sh --phase apply --i-have-approval
#   scripts/build_rmpd_pi.sh --all --i-have-approval
#
# Environment overrides:
#   REMOTE_HOST (default: root@192.168.2.120)
#   BUILD_DIR   (default: /tmp/rmpd-build)
#   PI_USER     (default: dietpi)

set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.2.120}"
BUILD_DIR="${BUILD_DIR:-/tmp/rmpd-build}"
PI_USER="${PI_USER:-dietpi}"

RMPD_REPO="https://github.com/M0Rf30/rmpd.git"
RMPD_TAG="0.19.0"
RMPD_REV="08ef16eefca450ee49a57432051bb176a383e21a"
RMPD_FEATURES="http-api,jellyfin,podcast,radio"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
UNIT_FILE="${REPO_ROOT}/baseline/etc/systemd/user/rmpd.service"
CONFIG_TEMPLATE="${REPO_ROOT}/baseline/etc/rmpd/rmpd.toml.example"
BIN_PATH="${BUILD_DIR}/target/aarch64-unknown-linux-gnu/release/rmpd"

PHASE="default"
APPROVAL=0

usage() { sed -n '2,32p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --phase)
      [[ $# -ge 2 ]] || { echo "--phase needs a value" >&2; exit 1; }
      PHASE="$2"; shift 2 ;;
    --all) PHASE="all"; shift ;;
    --i-have-approval) APPROVAL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

case "${PHASE}" in
  default|preflight|build|verify|apply|all) ;;
  *) echo "Invalid phase: ${PHASE}" >&2; exit 1 ;;
esac

if [[ "${PHASE}" == "apply" || "${PHASE}" == "all" ]] && [[ "${APPROVAL}" != 1 ]]; then
  echo "[rmpd] Refusing apply phase without --i-have-approval. No Pi changes made." >&2
  exit 1
fi

log() { printf '[rmpd] %s\n' "$*"; }
die() { echo "[rmpd] ERROR: $*" >&2; exit 1; }

phase_preflight() {
  local tool
  for tool in git cargo rustup aarch64-linux-gnu-gcc aarch64-linux-gnu-objdump pkg-config; do
    command -v "${tool}" >/dev/null 2>&1 || die "missing host tool: ${tool}"
  done

  if ! rustup target list --installed | grep -qx 'aarch64-unknown-linux-gnu'; then
    die "Rust target aarch64-unknown-linux-gnu not installed (run: rustup target add aarch64-unknown-linux-gnu)"
  fi

  local pc_libdir="/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
  PKG_CONFIG_ALLOW_CROSS=1 \
  PKG_CONFIG_SYSROOT_DIR=/ \
  PKG_CONFIG_LIBDIR="${pc_libdir}" \
    pkg-config --exists alsa || die "arm64 alsa.pc not found in ${pc_libdir}"

  [[ -f "${UNIT_FILE}" ]] || die "missing unit template: ${UNIT_FILE}"
  [[ -f "${CONFIG_TEMPLATE}" ]] || die "missing config template: ${CONFIG_TEMPLATE}"
  log "preflight OK (alsa $(PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR=/ PKG_CONFIG_LIBDIR="${pc_libdir}" pkg-config --modversion alsa))"
}

run_cargo_build() {
  CARGO_PROFILE_RELEASE_LTO=fat \
  CARGO_PROFILE_RELEASE_STRIP=true \
  CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  CFLAGS_aarch64_unknown_linux_gnu="-O3 -mcpu=cortex-a53+crc+simd" \
  PKG_CONFIG_ALLOW_CROSS=1 \
  PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu=/ \
  PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  RUSTFLAGS="-C target-cpu=cortex-a53 -C target-feature=+neon,-aes,-sha2 -C link-arg=-Wl,--as-needed -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,-O1" \
    cargo build --release --target aarch64-unknown-linux-gnu --features "${RMPD_FEATURES}"
}

phase_build() {
  if [[ ! -d "${BUILD_DIR}/.git" ]]; then
    log "Cloning ${RMPD_REPO} (${RMPD_TAG}) -> ${BUILD_DIR}"
    git clone --depth 1 --branch "${RMPD_TAG}" "${RMPD_REPO}" "${BUILD_DIR}"
  fi

  cd "${BUILD_DIR}"
  local head_rev
  head_rev="$(git rev-parse HEAD)"
  [[ "${head_rev}" == "${RMPD_REV}" ]] || die "Unexpected revision ${head_rev} in ${BUILD_DIR} (expected ${RMPD_REV})"

  # Ensure cpal ALSA opens hw:Loopback with a >=2048-frame buffer (BufferSize::Default
  # negotiates 512 frames, which is smaller than CamillaDSP's 1024-frame chunksize).
  local cpal_utils="${BUILD_DIR}/rmpd-player/src/cpal_utils.rs"
  if grep -q 'buffer_size: cpal::BufferSize::Default,' "${cpal_utils}"; then
    sed -i 's/buffer_size: cpal::BufferSize::Default,/buffer_size: cpal::BufferSize::Fixed(2048),/g' "${cpal_utils}"
    log "Applied cpal BufferSize::Fixed(2048) patch to rmpd-player/src/cpal_utils.rs."
  fi

  log "Building rmpd ${RMPD_TAG} (features: ${RMPD_FEATURES}) for aarch64-unknown-linux-gnu..."
  run_cargo_build

  [[ -f "${BIN_PATH}" ]] || die "Build artifact not found: ${BIN_PATH}"

  # If host libm headers caused any GLIBC > 2.41 math symbol references, pin them to GLIBC_2.17 and relink.
  local high_syms
  high_syms="$(aarch64-linux-gnu-objdump -T "${BIN_PATH}" | grep -E 'GLIBC_2\.(4[2-9]|[5-9][0-9])' || true)"
  if [[ -n "${high_syms}" ]]; then
    log "Detected GLIBC > 2.41 symbols; checking if GLIBC_2.17 .symver shim is needed..."
    local main_rs="${BUILD_DIR}/rmpd/src/main.rs"
    if ! grep -q 'GLIBC_2.17' "${main_rs}"; then
      local sym_lines=""
      local fn
      for fn in log10f acosf atan2f remainder expf logf powf; do
        if grep -qw "${fn}" <<<"${high_syms}"; then
          sym_lines+=".symver ${fn}, ${fn}@GLIBC_2.17\n"
        fi
      done
      if [[ -n "${sym_lines}" ]]; then
        log "Injecting GLIBC_2.17 .symver directives into rmpd/src/main.rs..."
        local tmp_main="${main_rs}.tmp"
        {
          printf '#[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]\n'
          printf 'core::arch::global_asm!("%b");\n' "${sym_lines%\\n}"
          cat "${main_rs}"
        } > "${tmp_main}"
        mv "${tmp_main}" "${main_rs}"
        run_cargo_build
      fi
    fi
  fi

  log "Built OK: ${BIN_PATH}"
}

phase_verify() {
  [[ -f "${BIN_PATH}" ]] || die "Binary not found: ${BIN_PATH} (run --phase build first)"

  local file_out
  file_out="$(file "${BIN_PATH}")"
  grep -q 'ARM aarch64' <<<"${file_out}" || die "Unexpected ELF arch: ${file_out}"

  local objdump_out
  objdump_out="$(aarch64-linux-gnu-objdump -T "${BIN_PATH}")"
  if grep -qE 'GLIBC_2\.(4[2-9]|[5-9][0-9])' <<<"${objdump_out}"; then
    grep -E 'GLIBC_2\.(4[2-9]|[5-9][0-9])' <<<"${objdump_out}" >&2
    die "Binary references GLIBC > 2.41 symbols (Pi runs Debian 13 Trixie glibc 2.41)"
  fi

  local needed
  needed="$(aarch64-linux-gnu-objdump -p "${BIN_PATH}" | awk '/NEEDED/ {print $2}' | tr '\n' ' ')"
  log "DT_NEEDED libraries: ${needed}"

  # shellcheck disable=SC2086
  ssh -o BatchMode=yes -o ConnectTimeout=5 "${REMOTE_HOST}" bash -s -- ${needed} <<'EOF'
set -euo pipefail
cache="$(ldconfig -p)"
for lib in "$@"; do
  if ! grep -Fq "${lib}" <<<"${cache}"; then
    echo "Missing runtime library on Pi: ${lib}" >&2
    exit 1
  fi
done
EOF

  log "ABI verification passed (aarch64, GLIBC <= 2.41, all DT_NEEDED present on Pi)"
}

phase_apply() {
  [[ -f "${BIN_PATH}" ]] || die "Binary not found: ${BIN_PATH}"

  local remote_stage remote_bin remote_unit remote_cfg
  remote_stage="$(ssh "${REMOTE_HOST}" mktemp -d)"
  remote_bin="${remote_stage}/rmpd"
  remote_unit="${remote_stage}/rmpd.service"
  remote_cfg="${remote_stage}/rmpd.toml.example"

  scp "${BIN_PATH}" "${REMOTE_HOST}:${remote_bin}"
  scp "${UNIT_FILE}" "${REMOTE_HOST}:${remote_unit}"
  scp "${CONFIG_TEMPLATE}" "${REMOTE_HOST}:${remote_cfg}"

  ssh "${REMOTE_HOST}" bash -s -- \
    "${remote_stage}" "${remote_bin}" "${remote_unit}" "${remote_cfg}" "${PI_USER}" <<'EOF'
set -euo pipefail
STAGE="$1"; RBIN="$2"; RUNIT="$3"; RCFG="$4"; PI_USER="$5"
trap 'rm -rf "${STAGE}"' EXIT

chmod +x "${RBIN}"
"${RBIN}" --version

PI_UID="$(id -u "${PI_USER}")"
PI_HOME="$(getent passwd "${PI_USER}" | cut -d: -f6)"
[[ -n "${PI_HOME}" ]] || { echo "Cannot resolve home for ${PI_USER}" >&2; exit 1; }

# 1. Ensure user is in audio group and systemd-logind / lingering is enabled for session D-Bus MPRIS
if ! id -nG "${PI_USER}" | grep -qw audio; then
  usermod -aG audio "${PI_USER}"
fi
systemctl unmask systemd-logind.service || true
systemctl start systemd-logind.service
loginctl enable-linger "${PI_USER}"
systemctl start "user@${PI_UID}.service" || true

# 2. Install binary atomically
install -Dm755 "${RBIN}" /usr/local/bin/rmpd

# 3. Install user systemd unit and seed config if absent
install -d -o "${PI_USER}" -g "${PI_USER}" -m 755 \
  "${PI_HOME}/.config" \
  "${PI_HOME}/.config/systemd" \
  "${PI_HOME}/.config/systemd/user" \
  "${PI_HOME}/.config/rmpd" \
  "${PI_HOME}/.config/rmpd/playlists"
install -m 644 -o "${PI_USER}" -g "${PI_USER}" "${RUNIT}" "${PI_HOME}/.config/systemd/user/rmpd.service"

if [[ ! -f "${PI_HOME}/.config/rmpd/rmpd.toml" ]]; then
  install -m 600 -o "${PI_USER}" -g "${PI_USER}" "${RCFG}" "${PI_HOME}/.config/rmpd/rmpd.toml"
  echo "[rmpd] Seeded ${PI_HOME}/.config/rmpd/rmpd.toml (update HTTP token / sources as needed)."
else
  echo "[rmpd] Preserved existing ${PI_HOME}/.config/rmpd/rmpd.toml."
fi

# 4. Reload and restart user service
for _ in $(seq 1 20); do
  [[ -S "/run/user/${PI_UID}/bus" ]] && break
  sleep 0.25
done

sudo -u "${PI_USER}" \
  XDG_RUNTIME_DIR="/run/user/${PI_UID}" \
  DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${PI_UID}/bus" \
  systemctl --user daemon-reload

sudo -u "${PI_USER}" \
  XDG_RUNTIME_DIR="/run/user/${PI_UID}" \
  DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${PI_UID}/bus" \
  systemctl --user enable --now rmpd.service

sudo -u "${PI_USER}" \
  XDG_RUNTIME_DIR="/run/user/${PI_UID}" \
  DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${PI_UID}/bus" \
  systemctl --user restart rmpd.service

sudo -u "${PI_USER}" \
  XDG_RUNTIME_DIR="/run/user/${PI_UID}" \
  DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${PI_UID}/bus" \
  systemctl --user status rmpd.service --no-pager -l | head -n 15
EOF
  log "Deploy complete."
}

case "${PHASE}" in
  default)
    phase_preflight
    phase_build
    phase_verify
    ;;
  preflight) phase_preflight ;;
  build)     phase_build ;;
  verify)    phase_verify ;;
  apply)     phase_apply ;;
  all)
    phase_preflight
    phase_build
    phase_verify
    phase_apply
    ;;
esac
