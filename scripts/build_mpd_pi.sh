#!/usr/bin/env bash
#
# build_mpd_pi.sh - Cross-compile and deploy MPD v0.25 for Raspberry Pi 3.
#
# Clones MusicPlayerDaemon/MPD at pinned v0.25, fetches a pinned Debian 13
# Trixie arm64 sysroot, cross-compiles with Cortex-A53 tuning via Meson + Ninja
# (streaming/radio-only, ALSA, systemd notify; no D-Bus, no mDNS, no local DB),
# verifies ELF/GLIBC/GLIBCXX/DT_NEEDED ABI compatibility, and (with
# --phase apply --i-have-approval) installs strictly to /usr/local/bin/mpd,
# installs missing runtime libraries + mpc via apt (never apt-installing mpd),
# removes all legacy rmpd traces, and starts mpd.service.
#
# Phases (default: preflight, sysroot, build, verify):
#   preflight  verify host tools, sysroot list, and baseline templates
#   sysroot    fetch, SHA-256-verify, and extract pinned Trixie arm64 .debs
#   build      clone + pin-verify MPD v0.25, cross-compile via Meson + Ninja
#   verify     ABI gate: aarch64 ELF, GLIBC <= 2.41, GLIBCXX <= 3.4.33, NEEDEDs
#   apply      install runtime deps + mpc, remove all rmpd traces, install
#              /usr/local/bin/mpd + config + unit, start mpd.service
#              (REQUIRES --i-have-approval)
#   all        preflight + sysroot + build + verify, then apply
#
# Usage:
#   scripts/build_mpd_pi.sh
#   scripts/build_mpd_pi.sh --phase build
#   scripts/build_mpd_pi.sh --phase apply --i-have-approval
#   scripts/build_mpd_pi.sh --all --i-have-approval
#
# Environment overrides:
#   REMOTE_HOST (default: root@192.168.2.120)
#   BUILD_DIR   (default: /tmp/mpd-build-pi)
#   FORCE       (default: 0; set 1 to re-download sysroot .debs)

set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.2.120}"
BUILD_DIR="${BUILD_DIR:-/tmp/mpd-build-pi}"
SYSROOT="${BUILD_DIR}/sysroot"
DEB_DIR="${BUILD_DIR}/debs"
SRC_DIR="${BUILD_DIR}/mpd"
MESON_BUILD="${BUILD_DIR}/build"
CROSS_FILE="${BUILD_DIR}/aarch64-trixie-cross.ini"
BIN_PATH="${MESON_BUILD}/mpd"
FORCE="${FORCE:-0}"

MPD_REPO="https://github.com/MusicPlayerDaemon/MPD.git"
MPD_TAG="v0.25"
MPD_REV="e411a30052b1417523f8d9032ac9a69596ed098b"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SYSROOT_LIST="${SCRIPT_DIR}/sysroot_mpd_trixie_arm64.list"
CONF_FILE="${REPO_ROOT}/baseline/etc/mpd.conf"
UNIT_FILE="${REPO_ROOT}/baseline/etc/systemd/system/mpd.service"

PHASE="default"
APPROVAL=0

usage() { sed -n '2,28p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --phase)
      [[ $# -ge 2 ]] || { echo "--phase needs a value" >&2; exit 1; }
      PHASE="$2"; shift 2 ;;
    --all) PHASE="all"; shift ;;
    --force) FORCE=1; shift ;;
    --i-have-approval) APPROVAL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

case "${PHASE}" in
  default|preflight|sysroot|build|verify|apply|all) ;;
  *) echo "Invalid phase: ${PHASE}" >&2; exit 1 ;;
esac

if [[ "${PHASE}" == "apply" || "${PHASE}" == "all" ]] && [[ "${APPROVAL}" != 1 ]]; then
  echo "[mpd] Refusing apply phase without --i-have-approval. No Pi changes made." >&2
  exit 1
fi

log() { printf '[mpd] %s\n' "$*"; }
die() { echo "[mpd] ERROR: $*" >&2; exit 1; }

verify_sysroot_pc() {
  local pc missing=()
  for pc in alsa libcurl libavformat libavcodec libavutil expat libmpg123 \
            flac opus vorbis ogg soxr libsystemd fmt; do
    [[ -f "${SYSROOT}/usr/lib/aarch64-linux-gnu/pkgconfig/${pc}.pc" ]] \
      || missing+=("${pc}.pc")
  done
  if [[ ${#missing[@]} -gt 0 ]]; then
    die "sysroot pkg-config files missing (run --phase sysroot first): ${missing[*]}"
  fi
}

phase_preflight() {
  local tool
  for tool in git meson ninja aarch64-linux-gnu-gcc aarch64-linux-gnu-g++ \
              aarch64-linux-gnu-ar aarch64-linux-gnu-strip \
              aarch64-linux-gnu-objdump pkg-config dpkg-deb curl; do
    command -v "${tool}" >/dev/null 2>&1 || die "missing host tool: ${tool}"
  done

  [[ -f "${SYSROOT_LIST}" ]] || die "missing sysroot list: ${SYSROOT_LIST}"
  [[ -f "${CONF_FILE}" ]] || die "missing mpd.conf template: ${CONF_FILE}"
  [[ -f "${UNIT_FILE}" ]] || die "missing mpd.service unit: ${UNIT_FILE}"
  log "preflight OK"
}

phase_sysroot() {
  mkdir -p "${SYSROOT}" "${DEB_DIR}"
  local name version url sha deb
  while IFS=$'\t' read -r name version url sha; do
    [[ -z "${name}" || "${name}" == \#* ]] && continue
    deb="${DEB_DIR}/${name}_${version}_arm64.deb"
    if [[ -f "${deb}" && "${FORCE}" != 1 ]]; then
      log "deb ${name} ${version}: cached"
    else
      log "fetching ${name} ${version}"
      curl -fsSL -o "${deb}" "${url}" || die "download failed: ${url}"
      echo "${sha}  ${deb}" | sha256sum -c - >/dev/null \
        || die "SHA-256 mismatch for ${name} ${version}"
    fi
    dpkg-deb -x "${deb}" "${SYSROOT}"
  done < "${SYSROOT_LIST}"

  # Merged-usr: sysroot libc.so/libm.so scripts reference /lib/... paths.
  if [[ ! -e "${SYSROOT}/lib" ]]; then
    ln -s usr/lib "${SYSROOT}/lib"
  fi
  # Strip static-only Requires.private/Libs.private from libcurl.pc so pkg-config
  # resolves dynamic -lcurl without requiring static transitive .pc files (libnghttp3, etc.).
  local curl_pc="${SYSROOT}/usr/lib/aarch64-linux-gnu/pkgconfig/libcurl.pc"
  if [[ -f "${curl_pc}" ]]; then
    sed -i -E '/^(Requires|Libs)\.private:/d' "${curl_pc}"
  fi
  verify_sysroot_pc
  PKG_CONFIG_SYSROOT_DIR="${SYSROOT}" \
  PKG_CONFIG_LIBDIR="${SYSROOT}/usr/lib/aarch64-linux-gnu/pkgconfig:${SYSROOT}/usr/share/pkgconfig" \
    pkg-config --modversion libcurl >/dev/null || die "pkg-config failed for libcurl in sysroot"
  log "sysroot ready at ${SYSROOT}"
}

write_cross_file() {
  local pc_libdir="${SYSROOT}/usr/lib/aarch64-linux-gnu/pkgconfig:${SYSROOT}/usr/share/pkgconfig"
  cat > "${CROSS_FILE}" <<EOF
[binaries]
c = 'aarch64-linux-gnu-gcc'
cpp = 'aarch64-linux-gnu-g++'
ar = 'aarch64-linux-gnu-ar'
strip = 'aarch64-linux-gnu-strip'
pkg-config = 'pkg-config'

[built-in options]
c_args = ['-O3', '-mcpu=cortex-a53+crc+simd', '-ftree-vectorize', '--sysroot=${SYSROOT}']
cpp_args = ['-O3', '-mcpu=cortex-a53+crc+simd', '-ftree-vectorize', '--sysroot=${SYSROOT}']
c_link_args = ['--sysroot=${SYSROOT}', '-Wl,--as-needed', '-Wl,--gc-sections', '-Wl,-O1', '-Wl,--allow-shlib-undefined', '-Wl,--wrap=log10f', '-Wl,-rpath-link,${SYSROOT}/usr/lib/aarch64-linux-gnu', '-Wl,-rpath-link,${SYSROOT}/lib/aarch64-linux-gnu']
cpp_link_args = ['--sysroot=${SYSROOT}', '-Wl,--as-needed', '-Wl,--gc-sections', '-Wl,-O1', '-Wl,--allow-shlib-undefined', '-Wl,--wrap=log10f', '-Wl,-rpath-link,${SYSROOT}/usr/lib/aarch64-linux-gnu', '-Wl,-rpath-link,${SYSROOT}/lib/aarch64-linux-gnu']

[properties]
sys_root = '${SYSROOT}'
pkg_config_libdir = '${pc_libdir}'

[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'cortex-a53'
endian = 'little'
EOF
}

phase_build() {
  verify_sysroot_pc
  if [[ ! -d "${SRC_DIR}/.git" ]]; then
    log "Cloning ${MPD_REPO} (${MPD_TAG}) -> ${SRC_DIR}"
    git clone --depth 1 --branch "${MPD_TAG}" "${MPD_REPO}" "${SRC_DIR}"
  fi

  local head_rev
  head_rev="$(git -C "${SRC_DIR}" rev-parse HEAD)"
  if [[ "${head_rev}" != "${MPD_REV}" ]]; then
    git -C "${SRC_DIR}" fetch --depth 1 --tags origin
    git -C "${SRC_DIR}" checkout --quiet "${MPD_REV}"
    head_rev="$(git -C "${SRC_DIR}" rev-parse HEAD)"
  fi
  [[ "${head_rev}" == "${MPD_REV}" ]] || die "Unexpected revision ${head_rev} in ${SRC_DIR} (expected ${MPD_REV})"

  # Redirect all log10f calls to log10f@GLIBC_2.17 via -Wl,--wrap=log10f so the
  # binary runs on Debian 13 Trixie (glibc 2.41) when built on glibc 2.43 hosts.
  local main_cxx="${SRC_DIR}/src/Main.cxx"
  if ! grep -q '__wrap_log10f' "${main_cxx}"; then
    local tmp_main="${main_cxx}.tmp"
    {
      printf '__asm__(\n'
      printf '    ".symver __glibc217_log10f, log10f@GLIBC_2.17\\n"\n'
      printf '    ".global __wrap_log10f\\n"\n'
      printf '    ".type __wrap_log10f, %%function\\n"\n'
      printf '    "__wrap_log10f:\\n"\n'
      printf '    "b __glibc217_log10f\\n"\n'
      printf ');\n'
      grep -v 'log10f@GLIBC_2.17' "${main_cxx}"
    } > "${tmp_main}"
    mv "${tmp_main}" "${main_cxx}"
    log "Applied __wrap_log10f -> log10f@GLIBC_2.17 trampoline to src/Main.cxx."
  fi
  grep -q '__wrap_log10f' "${main_cxx}" \
    || die "Expected __wrap_log10f trampoline in ${main_cxx}"

  write_cross_file

  local setup_args=(
    "${MESON_BUILD}" "${SRC_DIR}"
    --cross-file "${CROSS_FILE}"
    --wrap-mode=nodownload
    --buildtype=release
    -Dstrip=true
    -Dauto_features=disabled
    -Dalsa=enabled
    -Dcurl=enabled
    -Dffmpeg=enabled
    -Dexpat=enabled
    -Dfaad=enabled
    -Dmpg123=enabled
    -Dflac=enabled
    -Dopus=enabled
    -Dvorbis=enabled
    -Dsoxr=enabled
    -Dsystemd=enabled
    -Dtcp=true
    -Dlocal_socket=true
    -Ddatabase=false
    -Dinotify=false
    -Dcue=false
    -Dneighbor=false
    -Ddsd=false
    -Dsqlite=disabled
    -Dpcre=disabled
    -Ddbus=disabled
    -Dzeroconf=disabled
    -Ddocumentation=disabled
    -Dmanpages=false
    -Dhtml_manual=false
    -Dtest=false
  )
  if [[ -d "${MESON_BUILD}" ]]; then
    setup_args=(--wipe "${setup_args[@]}")
  fi

  log "Configuring MPD ${MPD_TAG} via Meson..."
  PKG_CONFIG_SYSROOT_DIR="${SYSROOT}" \
  PKG_CONFIG_LIBDIR="${SYSROOT}/usr/lib/aarch64-linux-gnu/pkgconfig:${SYSROOT}/usr/share/pkgconfig" \
    meson setup "${setup_args[@]}"

  log "Compiling mpd via Ninja..."
  ninja -C "${MESON_BUILD}" mpd
  [[ -f "${BIN_PATH}" ]] || die "Build artifact not found: ${BIN_PATH}"
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
  if grep -qE 'GLIBCXX_3\.4\.(3[4-9]|[4-9][0-9])' <<<"${objdump_out}"; then
    grep -E 'GLIBCXX_3\.4\.(3[4-9]|[4-9][0-9])' <<<"${objdump_out}" >&2
    die "Binary references GLIBCXX > 3.4.33 symbols (Pi libstdc++6 max is GLIBCXX_3.4.33)"
  fi

  local -a needed=()
  mapfile -t needed < <(aarch64-linux-gnu-objdump -p "${BIN_PATH}" | awk '/NEEDED/ {print $2}')
  [[ ${#needed[@]} -gt 0 ]] || die "No DT_NEEDED entries found in ${BIN_PATH}"
  printf '%s\n' "${needed[@]}" | grep -qx 'libcurl.so.4' \
    || die "Expected dynamic libcurl.so.4 in DT_NEEDED (got: ${needed[*]})"
  log "DT_NEEDED libraries: ${needed[*]}"

  ssh -o BatchMode=yes -o ConnectTimeout=5 "${REMOTE_HOST}" bash -s -- "${needed[@]}" <<'EOF'
set -euo pipefail
cache="$(ldconfig -p)"
missing=()
pending_apply=()
for lib in "$@"; do
  if ! grep -Fq "${lib}" <<<"${cache}"; then
    case "${lib}" in
      libavformat.so.61|libfaad.so.2) pending_apply+=("${lib}") ;;
      *) missing+=("${lib}") ;;
    esac
  fi
done
if [[ ${#pending_apply[@]} -gt 0 ]]; then
  echo "[mpd] Pending apt runtime libs (installed in apply): ${pending_apply[*]}"
fi
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "Unexpected missing runtime libraries on Pi: ${missing[*]}" >&2
  exit 1
fi
EOF

  log "ABI verification passed (aarch64, GLIBC <= 2.41, GLIBCXX <= 3.4.33)"
}

phase_apply() {
  [[ -f "${BIN_PATH}" ]] || die "Binary not found: ${BIN_PATH}"

  local remote_stage remote_bin remote_conf remote_unit
  remote_stage="$(ssh "${REMOTE_HOST}" mktemp -d)"
  trap 'ssh -o BatchMode=yes -o ConnectTimeout=5 "${REMOTE_HOST}" rm -rf "${remote_stage}" >/dev/null 2>&1 || true' RETURN
  remote_bin="${remote_stage}/mpd"
  remote_conf="${remote_stage}/mpd.conf"
  remote_unit="${remote_stage}/mpd.service"

  scp "${BIN_PATH}" "${REMOTE_HOST}:${remote_bin}"
  scp "${CONF_FILE}" "${REMOTE_HOST}:${remote_conf}"
  scp "${UNIT_FILE}" "${REMOTE_HOST}:${remote_unit}"

  ssh "${REMOTE_HOST}" bash -s -- \
    "${remote_stage}" "${remote_bin}" "${remote_conf}" "${remote_unit}" <<'EOF'
set -euo pipefail
STAGE="$1"; RBIN="$2"; RCONF="$3"; RUNIT="$4"
trap 'rm -rf "${STAGE}"' EXIT

# 1. Never allow distro mpd package to be installed or conflict with /usr/local/bin/mpd
if dpkg-query -W -f='${Status}' mpd 2>/dev/null | grep -q 'install ok installed'; then
  echo "ERROR: distro mpd package is installed on Pi; refusing to overwrite." >&2
  exit 1
fi

# 2. Install only missing runtime shared libraries + mpc (never mpd)
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y libavformat61 libfaad2 mpc

chmod +x "${RBIN}"
"${RBIN}" --version | head -n 5

# 3. Remove all legacy rmpd traces cleanly (preserving any playlists if present)
BACKUP_PL="/tmp/rmpd-playlists-backup"
rm -rf "${BACKUP_PL}"
if [[ -d /home/dietpi/.config/rmpd/playlists ]] && compgen -G "/home/dietpi/.config/rmpd/playlists/*" >/dev/null; then
  mkdir -p "${BACKUP_PL}"
  cp -a /home/dietpi/.config/rmpd/playlists/* "${BACKUP_PL}/"
fi

if id -u dietpi >/dev/null 2>&1; then
  DIETPI_UID="$(id -u dietpi)"
  if [[ -S "/run/user/${DIETPI_UID}/bus" ]]; then
    sudo -u dietpi \
      XDG_RUNTIME_DIR="/run/user/${DIETPI_UID}" \
      DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${DIETPI_UID}/bus" \
      systemctl --user disable --now rmpd.service || true
    sudo -u dietpi \
      XDG_RUNTIME_DIR="/run/user/${DIETPI_UID}" \
      DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${DIETPI_UID}/bus" \
      systemctl --user daemon-reload || true
  fi
fi

pkill -x rmpd || true
rm -f /usr/local/bin/rmpd
rm -f /home/dietpi/.config/systemd/user/rmpd.service \
      /home/dietpi/.config/systemd/user/default.target.wants/rmpd.service
rm -rf /home/dietpi/.config/rmpd /home/dietpi/.local/state/rmpd
rmdir /home/dietpi/.config/systemd/user/default.target.wants 2>/dev/null || true
rmdir /home/dietpi/.config/systemd/user 2>/dev/null || true

if id -u dietpi >/dev/null 2>&1; then
  DIETPI_UID="$(id -u dietpi)"
  if [[ ! -d /home/dietpi/.config/systemd/user ]] || [[ -z "$(ls -A /home/dietpi/.config/systemd/user 2>/dev/null)" ]]; then
    loginctl disable-linger dietpi || true
    rm -f /var/lib/systemd/linger/dietpi
    systemctl stop "user@${DIETPI_UID}.service" || true
  fi
fi

# 4. Provision mpd system user and state/playlists directory
if ! id -u mpd >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/mpd --groups audio --shell /usr/sbin/nologin mpd
fi
if ! id -nG mpd | grep -qw audio; then
  usermod -aG audio mpd
fi
install -d -o mpd -g audio -m 755 /var/lib/mpd /var/lib/mpd/playlists
if [[ -d "${BACKUP_PL}" ]]; then
  cp -a "${BACKUP_PL}/"* /var/lib/mpd/playlists/
  chown -R mpd:audio /var/lib/mpd/playlists
  rm -rf "${BACKUP_PL}"
fi

# 5. Install binary, config, and system unit
install -Dm755 "${RBIN}" /usr/local/bin/mpd
install -Dm644 "${RCONF}" /etc/mpd.conf
install -Dm644 "${RUNIT}" /etc/systemd/system/mpd.service

systemctl daemon-reload
systemctl enable --now mpd.service
systemctl restart mpd.service
systemctl status mpd.service --no-pager -l | head -n 15
EOF
  log "Deploy complete."
}

case "${PHASE}" in
  default)
    phase_preflight
    phase_sysroot
    phase_build
    phase_verify
    ;;
  preflight) phase_preflight ;;
  sysroot)   phase_sysroot ;;
  build)     phase_build ;;
  verify)    phase_verify ;;
  apply)     phase_apply ;;
  all)
    phase_preflight
    phase_sysroot
    phase_build
    phase_verify
    phase_apply
    ;;
esac
