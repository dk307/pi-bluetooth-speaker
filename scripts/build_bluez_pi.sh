#!/usr/bin/env bash
#
# build_bluez_pi.sh - Patch, build, and install BlueZ 5.88 on the Raspberry Pi 3.
#
# Applies baseline/patches/bluez-5.88-mediaplayer-refresh-track.patch (which adds
# the experimental org.bluez.MediaPlayer1.RefreshTrack D-Bus method) to
# /root/build/bluez-5.88 on the target Pi, builds BlueZ, installs bluetoothd,
# and restarts bluetooth.service.
#
# Usage:
#   scripts/build_bluez_pi.sh                  # full build & install
#   scripts/build_bluez_pi.sh --bluetoothd-only # rebuild/install src/bluetoothd only
#
# Environment overrides:
#   REMOTE_HOST (default: root@192.168.2.120)

set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.2.120}"
BLUEZ_VERSION="5.88"
BLUEZ_TARBALL="bluez-${BLUEZ_VERSION}.tar.xz"
BLUEZ_URL="https://www.kernel.org/pub/linux/bluetooth/${BLUEZ_TARBALL}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PATCH_FILE="${REPO_ROOT}/baseline/patches/bluez-5.88-mediaplayer-refresh-track.patch"

BLUETOOTH_ONLY=0
if [[ "${1:-}" == "--bluetoothd-only" ]]; then
  BLUETOOTH_ONLY=1
elif [[ -n "${1:-}" ]]; then
  echo "Usage: $0 [--bluetoothd-only]" >&2
  exit 1
fi

if [[ ! -f "${PATCH_FILE}" ]]; then
  echo "Patch file not found: ${PATCH_FILE}" >&2
  exit 1
fi

REMOTE_PATCH="/tmp/bluez-5.88-mediaplayer-refresh-track.patch"
scp "${PATCH_FILE}" "${REMOTE_HOST}:${REMOTE_PATCH}"

ssh "${REMOTE_HOST}" bash -s -- "${BLUEZ_VERSION}" "${BLUEZ_TARBALL}" "${BLUEZ_URL}" "${REMOTE_PATCH}" "${BLUETOOTH_ONLY}" <<'EOF'
set -euo pipefail

BLUEZ_VERSION="$1"
BLUEZ_TARBALL="$2"
BLUEZ_URL="$3"
REMOTE_PATCH="$4"
BLUETOOTH_ONLY="$5"
BUILD_ROOT="/root/build"
SRC_DIR="${BUILD_ROOT}/bluez-${BLUEZ_VERSION}"

mkdir -p "${BUILD_ROOT}"
cd "${BUILD_ROOT}"

if [[ ! -d "${SRC_DIR}" ]]; then
  if [[ ! -f "${BLUEZ_TARBALL}" ]]; then
    wget -O "${BLUEZ_TARBALL}" "${BLUEZ_URL}"
  fi
  tar xf "${BLUEZ_TARBALL}"
fi

cd "${SRC_DIR}"

# GCC 14 (C23 default) fix in src/shared/ad.c
if grep -q 'return false;' src/shared/ad.c; then
  sed -i 's/return false;/return NULL;/' src/shared/ad.c
fi

# Apply RefreshTrack patch idempotently
if grep -q 'media_player_refresh_track' profiles/audio/player.c; then
  echo "[bluez] RefreshTrack patch already applied."
else
  patch -p1 < "${REMOTE_PATCH}"
  echo "[bluez] Applied RefreshTrack patch."
fi
rm -f "${REMOTE_PATCH}"

if [[ ! -f Makefile ]]; then
  ./configure \
    --prefix=/usr \
    --mandir=/usr/share/man \
    --sysconfdir=/etc \
    --localstatedir=/var \
    --libexecdir=/usr/libexec \
    --enable-library \
    --enable-deprecated \
    --enable-pie \
    --enable-threads \
    --enable-systemd \
    --with-systemdsystemunitdir=/usr/lib/systemd/system \
    --with-systemduserunitdir=/usr/lib/systemd/user \
    --with-udevdir=/usr/lib/udev \
    --disable-cups \
    --disable-mesh \
    --disable-manpages
fi

if [[ "${BLUETOOTH_ONLY}" == "1" ]]; then
  make -j"$(nproc)" src/bluetoothd
  systemctl stop bluetooth.service || true
  if [[ -f /usr/libexec/bluetooth/bluetoothd && ! -f /usr/libexec/bluetooth/bluetoothd.bak ]]; then
    cp -a /usr/libexec/bluetooth/bluetoothd /usr/libexec/bluetooth/bluetoothd.bak
  fi
  install -m 755 src/bluetoothd /usr/libexec/bluetooth/bluetoothd
else
  make -j"$(nproc)"
  systemctl stop bluetooth.service || true
  if [[ -f /usr/libexec/bluetooth/bluetoothd && ! -f /usr/libexec/bluetooth/bluetoothd.bak ]]; then
    cp -a /usr/libexec/bluetooth/bluetoothd /usr/libexec/bluetooth/bluetoothd.bak
  fi
  make install
  cp -a /usr/lib/libbluetooth.so* /usr/lib/aarch64-linux-gnu/
  cp -a /usr/lib/pkgconfig/bluez.pc /usr/lib/aarch64-linux-gnu/pkgconfig/
  ldconfig
fi

systemctl start bluetooth.service
systemctl status bluetooth.service --no-pager -l | head -n 12
EOF
