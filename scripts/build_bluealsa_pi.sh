#!/usr/bin/env bash
#
# build_bluealsa_pi.sh - Build/install a PRIVATE, Cortex-A53-tuned BlueALSA v5.0.0
#                        stack on the Raspberry Pi 3, driven entirely from this repo.
#
# Mirrors scripts/build_bluez_pi.sh / build_camilladsp_pi.sh: host-side driver,
# all compilation happens on the Pi over SSH, versions are pinned in
# scripts/codecs/versions.lock and verified before use (zero-trust rule).
#
# Phases (default: codecs, bluealsa, verify):
#   --phase codecs    build tuned shared codec libs into /usr/local/lib
#   --phase bluealsa  build/install all four binaries + ALSA plugins into /usr/local
#   --phase verify    read-only path/optimization checks
#   --phase apply     destructive rollout: D-Bus policy, ALSA symlinks, unit
#                     ExecStart edits, apt removal, config restore, service restart
#                     (REQUIRES --i-have-approval)
#   --all             default phases plus apply (still requires --i-have-approval)
#
# Options:
#   --force           rebuild codecs even if already installed
#   --with=sbc        also build a private libsbc (not needed: distro build has NEON)
#   --i-have-approval explicit authorization for the --phase apply mutations
#   -h                show this help
#
# Default phases write ONLY under /usr/local, /usr/src/private-bluealsa and
# /tmp on the Pi. Nothing in /etc, no service and no apt package is touched
# unless --phase apply --i-have-approval is given.
#
# Environment overrides:
#   REMOTE_HOST (default: root@192.168.2.120)

set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.2.120}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
LOCK_FILE="${SCRIPT_DIR}/codecs/versions.lock"

PHASE="default"
FORCE=0
WITH_SBC=0
APPROVAL=0

usage() { sed -n '2,33p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --phase)
      [[ $# -ge 2 ]] || { echo "--phase needs a value" >&2; exit 1; }
      PHASE="$2"; shift 2 ;;
    --all) PHASE="all"; shift ;;
    --force) FORCE=1; shift ;;
    --with=sbc) WITH_SBC=1; shift ;;
    --i-have-approval) APPROVAL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

case "$PHASE" in
  default|codecs|bluealsa|verify|apply|all) ;;
  *) echo "Invalid phase: $PHASE" >&2; exit 1 ;;
esac

if [[ ! -f "${LOCK_FILE}" ]]; then
  echo "[bluealsa] Missing pin file: ${LOCK_FILE}" >&2
  exit 1
fi

# Extract only the bluez-alsa rev/url here; the remote parses the rest from
# the shipped lock file (avoids ssh arg re-parsing of '|'/newline content).
if [[ "${WITH_SBC}" == 1 ]] && ! grep -q '^sbc[[:space:]]' "${LOCK_FILE}"; then
  echo "[bluealsa] --with=sbc given but no 'sbc' entry in ${LOCK_FILE}." >&2
  echo "Add the libsbc pin first or drop the flag. No Pi changes made." >&2
  exit 1
fi
BLUEZ_REV=""
BLUEZ_URL=""
while read -r name version url sha _rest; do
  [[ -z "${name}" || "${name}" == \#* ]] && continue
  case "${name}" in
    bluez-alsa)
      BLUEZ_REV="${sha}"; BLUEZ_URL="${url}" ;;
    sbc|freeaptx|fdk-aac) ;;  # parsed remotely
    *) echo "[bluealsa] Unknown lock entry: ${name}" >&2; exit 1 ;;
  esac
done < "${LOCK_FILE}"

if [[ -z "${BLUEZ_REV}" || -z "${BLUEZ_URL}" ]]; then
  echo "[bluealsa] bluez-alsa pin missing from ${LOCK_FILE}" >&2
  exit 1
fi

if [[ "${PHASE}" == "apply" || "${PHASE}" == "all" ]] && [[ "${APPROVAL}" != 1 ]]; then
  echo "[bluealsa] Refusing apply phase without --i-have-approval. No Pi changes made." >&2
  exit 1
fi

# Baseline config is needed only by the apply phase (apt removal deletes it).
if [[ "${PHASE}" == "apply" || "${PHASE}" == "all" ]]; then
  scp "${REPO_ROOT}/baseline/etc/alsa/20-bluealsa.conf" \
    "${REMOTE_HOST}:/tmp/private-bluealsa-20-bluealsa.conf"
fi

# Ship the lock file so the remote can parse pins itself. Passing the codec
# spec as an ssh positional arg would be re-parsed by the remote shell (the
# '|'/newline content would be word-split and pipe-parsed), so we use a file.
scp "${LOCK_FILE}" "${REMOTE_HOST}:/tmp/private-bluealsa-versions.lock"

echo "[bluealsa] Driving phase '${PHASE}' on ${REMOTE_HOST}"
ssh "${REMOTE_HOST}" bash -s -- \
  "${PHASE}" "${FORCE}" "${APPROVAL}" "${BLUEZ_REV}" "${BLUEZ_URL}" "${WITH_SBC}" <<'REMOTE_EOF'
set -euo pipefail

PHASE="$1"; FORCE="$2"; APPROVAL="$3"; BLUEZ_REV="$4"; BLUEZ_URL="$5"; WITH_SBC="$6"
LOCK=/tmp/private-bluealsa-versions.lock

# Build the codec spec (name|version|url|sha) from the shipped lock file.
CODEC_SPEC=""
while read -r name version url sha _rest; do
  [[ -z "${name}" || "${name}" == \#* ]] && continue
  case "${name}" in
    freeaptx|fdk-aac) CODEC_SPEC+="${name}|${version}|${url}|${sha}"$'\n' ;;
    sbc) [[ "${WITH_SBC}" == 1 ]] && CODEC_SPEC+="${name}|${version}|${url}|${sha}"$'\n' ;;
    bluez-alsa) ;;  # pinned rev/url passed as args above
  esac
done < "${LOCK}"
PREFIX=/usr/local
BUILD_ROOT=/usr/src/private-bluealsa
LOG_DIR=/tmp/private-bluealsa-logs
ALSA_DIR=/usr/lib/aarch64-linux-gnu/alsa-lib
# Canonical A53 tuning (GCC 14 on this Pi); -mno-aes/-mno-sha2 are LLVM-only
# options and are rejected by GCC. -frecord-gcc-switches makes flags auditable.
TUNE="-O3 -mcpu=cortex-a53+crc+simd -ftree-vectorize -fPIC -frecord-gcc-switches"

mkdir -p "${LOG_DIR}" "${BUILD_ROOT}/codecs"
log() { printf '[%s] %s\n' "${PHASE}" "$*"; }
die() { echo "[bluealsa] ERROR: $*" >&2; exit 1; }

build_codec() {
  local name="$1" version="$2" url="$3" sha="$4" target tarball src
  case "${name}" in
    freeaptx) soname=libfreeaptx.so.0 ;;
    fdk-aac)  soname=libfdk-aac.so.2 ;;
    sbc)      soname=libsbc.so.1 ;;
    *) die "No recipe for codec ${name}" ;;
  esac
  target="${PREFIX}/lib/${soname}"
  if [[ -e "${target}" && "${FORCE}" != 1 ]]; then
    log "codec ${name}: ${target} already installed (use --force to rebuild)"
    return
  fi

  tarball="${BUILD_ROOT}/codecs/${name}-${version}.tar.gz"
  if [[ ! -f "${tarball}" ]]; then
    log "downloading ${name} ${version}"
    wget -q -O "${tarball}" "${url}"
  fi
  echo "${sha}  ${tarball}" | sha256sum -c - >/dev/null \
    || die "SHA-256 mismatch for ${name} ${version} - refusing to build"
  # GitHub archive tarballs extract to <repo>-<tag> (e.g. libfreeaptx-0.2.2),
  # not <name>-<version>; record the marker then discover the actual top dir.
  marker="${BUILD_ROOT}/codecs/.${name}-${version}.extracted"
  if [[ ! -f "${marker}" ]]; then
    tar xf "${tarball}" -C "${BUILD_ROOT}/codecs"
    # Extract exactly one new directory and remember its name.
    src="$(find "${BUILD_ROOT}/codecs" -maxdepth 1 -mindepth 1 -type d \
      -name "*${version}*" | head -n1)"
    [[ -n "${src}" && -d "${src}" ]] \
      || die "could not locate extracted source dir for ${name} ${version}"
    echo "${src}" > "${marker}"
  else
    src="$(cat "${marker}")"
  fi

  case "${name}" in
    freeaptx)
      cd "${src}"
      # shellcheck disable=SC2086
      gcc ${TUNE} -c freeaptx.c
      gcc -shared -Wl,-soname,"${soname}" -o "${soname}.${version}" freeaptx.o
      install -m 644 freeaptx.h "${PREFIX}/include/"
      install -m 755 "${soname}.${version}" "${PREFIX}/lib/"
      ln -sfn "${soname}.${version}" "${PREFIX}/lib/${soname}"
      ln -sfn "${soname}" "${PREFIX}/lib/libfreeaptx.so"
      ;;
    fdk-aac)
      cmake -S "${src}" -B "${src}/build" -DCMAKE_INSTALL_PREFIX="${PREFIX}" \
        -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
        -DCMAKE_INSTALL_LIBDIR=lib \
        -DCMAKE_CXX_FLAGS="${TUNE}" -DCMAKE_C_FLAGS="${TUNE}"
      cmake --build "${src}/build" --verbose -j"$(nproc)" 2>&1 | tee "${LOG_DIR}/fdk-aac.log"
      cmake --install "${src}/build"
      ;;
    sbc)
      ./configure --prefix="${PREFIX}" --enable-shared --disable-static CFLAGS="${TUNE}"
      make -j"$(nproc)" && make install
      ;;
  esac
  readelf -d "${target}" | grep -q "SONAME.*${soname}" \
    || die "${target} has wrong/missing SONAME (expected ${soname})"
  log "built ${name}: ${target}"
}

phase_codecs() {
  while IFS='|' read -r name version url sha; do
    [[ -z "${name}" ]] && continue
    build_codec "${name}" "${version}" "${url}" "${sha}"
  done <<< "${CODEC_SPEC}"
}

phase_bluealsa() {
  local src="${BUILD_ROOT}/bluez-alsa"
  if [[ ! -d "${src}/.git" ]]; then
    log "cloning bluez-alsa pinned commit ${BLUEZ_REV}"
    git clone --branch v5.0.0 --depth=1 "${BLUEZ_URL}" "${src}"
  fi
  (cd "${src}" && [[ "$(git rev-parse HEAD)" == "${BLUEZ_REV}" ]]) \
    || die "bluez-alsa HEAD does not match pinned ${BLUEZ_REV}"
  (cd "${src}" && autoreconf --install --force)
  mkdir -p "${src}/build"
  cd "${src}/build"
  make clean >/dev/null 2>&1 || true

  log "configuring v5.0.0 with A53 tuning + private codec paths"
  CFLAGS="${TUNE}" CXXFLAGS="${TUNE}" \
  CPPFLAGS="-I${PREFIX}/include" \
  LDFLAGS="-L${PREFIX}/lib -Wl,-rpath,${PREFIX}/lib" \
  APTX_CFLAGS="-I${PREFIX}/include" APTX_LIBS="-L${PREFIX}/lib -lfreeaptx" \
  APTX_HD_CFLAGS="-I${PREFIX}/include" APTX_HD_LIBS="-L${PREFIX}/lib -lfreeaptx" \
  AAC_CFLAGS="-I${PREFIX}/include" AAC_LIBS="-L${PREFIX}/lib -lfdk-aac" \
  ../configure \
    --prefix="${PREFIX}" \
    --sysconfdir=/etc \
    --with-alsaconfdir="${PREFIX}/etc/alsa/conf.d" \
    --with-alsaplugindir="${PREFIX}/lib/alsa-lib" \
    --with-dbusconfdir="${PREFIX}/etc/dbus-1/system.d" \
    --with-dbus-iface-xml="${PREFIX}/share/dbus-1/interfaces" \
    --with-systemdsystemunitdir="${PREFIX}/lib/systemd/system" \
    --enable-systemd --enable-aplay --enable-rfcomm \
    --with-libsamplerate \
    --disable-asha --disable-ldac --enable-lc3plus --enable-faststream \
    --enable-opus --enable-mp3lame --enable-mpg123 \
    --enable-aptx --enable-aptx-hd --with-libfreeaptx --enable-aac \
    --enable-lc3-swb --enable-msbc --enable-midi

  make V=1 -j"$(nproc)" 2>&1 | tee "${LOG_DIR}/bluez-alsa.log"
  make install
  ln -sfn "${PREFIX}/bin/bluealsad" "${PREFIX}/bin/bluealsa"
  ln -sfn "${PREFIX}/bin/bluealsactl" "${PREFIX}/bin/bluealsa-cli"
  log "installed private binaries, plugins and D-Bus policy under ${PREFIX}"
}

phase_verify() {
  local artifacts=(
    "${PREFIX}/bin/bluealsad" "${PREFIX}/bin/bluealsa-aplay"
    "${PREFIX}/bin/bluealsactl" "${PREFIX}/bin/bluealsa-rfcomm"
    "${PREFIX}/lib/libfreeaptx.so.0" "${PREFIX}/lib/libfdk-aac.so.2"
    "${PREFIX}/lib/alsa-lib/libasound_module_pcm_bluealsa.so"
    "${PREFIX}/lib/alsa-lib/libasound_module_ctl_bluealsa.so"
  )
  for f in "${artifacts[@]}"; do
    [[ -e "${f}" ]] || die "missing artifact: ${f}"
    readelf -p .GCC.command.line "${f}" | grep -q 'cortex-a53' \
      || die "NOT A53-TUNED: ${f}"
    readelf -p .GCC.command.line "${f}" | grep -q -- '-O3' \
      || die "NOT -O3: ${f}"
  done
  ldd "${PREFIX}/bin/bluealsad" | grep -E 'freeaptx|fdk-aac' \
    | grep -q "${PREFIX}/lib" || die "bluealsad is not using private codec libs"
  "${PREFIX}/bin/bluealsad" -h 2>&1 | grep -qi aptx \
    || die "aptX codecs not registered in private bluealsad"
  log "all path + optimization checks passed (CPU gates still require a live stream)"
}

phase_apply() {
  [[ "${APPROVAL}" == 1 ]] || die "apply requires --i-have-approval"
  local stamp backup
  stamp="$(date +%Y%m%d-%H%M%S)"
  backup="${PREFIX}/share/private-bluealsa/backups/${stamp}"
  mkdir -p "${backup}"
  cp -a /etc/systemd/system/bluealsa.service /etc/systemd/system/bluealsa-aplay.service "${backup}/"
  [[ ! -e /etc/alsa/conf.d/20-bluealsa.conf ]] \
    || cp -a /etc/alsa/conf.d/20-bluealsa.conf "${backup}/"
  log "backed up units/config to ${backup}"

  apt-get -s remove bluez-alsa-utils | tee "${LOG_DIR}/apt-remove-dryrun.log"
  log "apt dry-run above; see ${LOG_DIR}/apt-remove-dryrun.log"

  sed -i 's#^ExecStart=/usr/bin/bluealsad #ExecStart=/usr/local/bin/bluealsad #' \
    /etc/systemd/system/bluealsa.service
  sed -i 's#^ExecStart=/usr/bin/bluealsa-aplay #ExecStart=/usr/local/bin/bluealsa-aplay #' \
    /etc/systemd/system/bluealsa-aplay.service
  grep -q '^ExecStart=/usr/local/bin/bluealsad ' /etc/systemd/system/bluealsa.service \
    || die "failed to update bluealsa.service ExecStart"
  grep -q '^ExecStart=/usr/local/bin/bluealsa-aplay ' /etc/systemd/system/bluealsa-aplay.service \
    || die "failed to update bluealsa-aplay.service ExecStart"

  cp -a "${PREFIX}/etc/dbus-1/system.d/org.bluealsa.conf" /etc/dbus-1/system.d/org.bluealsa.conf
  ln -sfn "${PREFIX}/lib/alsa-lib/libasound_module_pcm_bluealsa.so" \
    "${ALSA_DIR}/libasound_module_pcm_bluealsa.so"
  ln -sfn "${PREFIX}/lib/alsa-lib/libasound_module_ctl_bluealsa.so" \
    "${ALSA_DIR}/libasound_module_ctl_bluealsa.so"
  ldconfig

  apt-get remove -y bluez-alsa-utils
  install -m 644 /tmp/private-bluealsa-20-bluealsa.conf /etc/alsa/conf.d/20-bluealsa.conf
  systemctl daemon-reload
  systemctl restart bluealsa.service bluealsa-aplay.service
  rm -f /tmp/private-bluealsa-20-bluealsa.conf
  log "applied private stack; run --phase verify and the live CPU gates now"
  log "rollback: reinstall bluez-alsa-utils=4.3.1-3, restore units/config from ${backup}"
}

case "${PHASE}" in
  default|all)
    phase_codecs; phase_bluealsa; phase_verify
    if [[ "${PHASE}" == "all" ]]; then phase_apply; fi
    ;;
  codecs)   phase_codecs ;;
  bluealsa) phase_bluealsa ;;
  verify)   phase_verify ;;
  apply)    phase_apply ;;
esac
REMOTE_EOF
