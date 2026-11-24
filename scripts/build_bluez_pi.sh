#!/usr/bin/env bash
#
# build_bluez_pi.sh - Patch, build, and install BlueZ 5.88 on the Raspberry Pi 3.
#
# Applies baseline/patches/bluez-5.88-mediaplayer-refresh-track.patch (which adds
# the experimental org.bluez.MediaPlayer1.RefreshTrack D-Bus method) and
# baseline/patches/bluez-5.88-track-duration.patch (which drops a stale track
# Duration when the track changes so a new track without duration metadata does
# not inherit the previous track's length) to /root/build/bluez-5.88 on the
# target Pi, builds BlueZ, installs bluetoothd, and restarts bluetooth.service.
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
BLUEZ_SHA256="6952c39eb5f8b0ee712cc50ab3bcff03be17c410dd97ca640f4849d9a1653ba3"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PATCH_FILE="${REPO_ROOT}/baseline/patches/bluez-5.88-mediaplayer-refresh-track.patch"
TRACK_DURATION_PATCH_FILE="${REPO_ROOT}/baseline/patches/bluez-5.88-track-duration.patch"

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

if [[ ! -f "${TRACK_DURATION_PATCH_FILE}" ]]; then
  echo "Patch file not found: ${TRACK_DURATION_PATCH_FILE}" >&2
  exit 1
fi

REMOTE_PATCH="/tmp/bluez-5.88-mediaplayer-refresh-track.patch"
REMOTE_TRACK_DURATION_PATCH="/tmp/bluez-5.88-track-duration.patch"
scp "${PATCH_FILE}" "${REMOTE_HOST}:${REMOTE_PATCH}" || { echo "Failed to copy RefreshTrack patch" >&2; exit 1; }
scp "${TRACK_DURATION_PATCH_FILE}" "${REMOTE_HOST}:${REMOTE_TRACK_DURATION_PATCH}" || { echo "Failed to copy track-duration patch" >&2; exit 1; }

ssh "${REMOTE_HOST}" bash -s -- "${BLUEZ_VERSION}" "${BLUEZ_TARBALL}" "${BLUEZ_URL}" "${BLUEZ_SHA256}" "${REMOTE_PATCH}" "${BLUETOOTH_ONLY}" "${REMOTE_TRACK_DURATION_PATCH}" <<'EOF'
set -euo pipefail

BLUEZ_VERSION="$1"
BLUEZ_TARBALL="$2"
BLUEZ_URL="$3"
BLUEZ_SHA256="$4"
REMOTE_PATCH="$5"
BLUETOOTH_ONLY="$6"
REMOTE_TRACK_DURATION_PATCH="$7"
BUILD_ROOT="/root/build"
SRC_DIR="${BUILD_ROOT}/bluez-${BLUEZ_VERSION}"

mkdir -p "${BUILD_ROOT}"
cd "${BUILD_ROOT}"

if [[ ! -d "${SRC_DIR}" ]]; then
  if [[ ! -f "${BLUEZ_TARBALL}" ]]; then
    wget -O "${BLUEZ_TARBALL}" "${BLUEZ_URL}"
  fi
  echo "${BLUEZ_SHA256}  ${BLUEZ_TARBALL}" | sha256sum -c -
  tar xf "${BLUEZ_TARBALL}"
fi

cd "${SRC_DIR}"

# GCC 14 (C23 default) fix in bt_ad_has_data() in src/shared/ad.c
sed -i '/^struct bt_ad_data \*bt_ad_has_data/,/^}/ s/return false;/return NULL;/' src/shared/ad.c

# Apply RefreshTrack patch idempotently (dry-run probe)
if patch --dry-run -p1 -R < "${REMOTE_PATCH}" >/dev/null 2>&1; then
  echo "[bluez] RefreshTrack patch already applied."
else
  patch -p1 < "${REMOTE_PATCH}"
  echo "[bluez] Applied RefreshTrack patch."
fi

# Apply track-duration patch idempotently (dry-run probe)
if patch --dry-run -p1 -R < "${REMOTE_TRACK_DURATION_PATCH}" >/dev/null 2>&1; then
  echo "[bluez] track-duration patch already applied."
else
  patch -p1 < "${REMOTE_TRACK_DURATION_PATCH}"
  echo "[bluez] Applied track-duration patch."
fi

rm -f "${REMOTE_PATCH}" "${REMOTE_TRACK_DURATION_PATCH}"

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

restart_bluetooth() {
  # bluetooth.service restart propagates a stop to bluealsa (BindsTo=bluetooth.service),
  # bluealsa-aplay (BindsTo=bluealsa.service) and bt-manager (After=bluealsa.service),
  # leaving no A2DP endpoints registered ("Protocol not available") and the appliance
  # unable to accept connections. Bring the whole dependent stack back up, in order.
  systemctl start bluetooth.service
  systemctl start bluealsa.service || true
  systemctl start bt-manager.service || true
  systemctl start bluealsa-aplay.service || true
}

if [[ "${BLUETOOTH_ONLY}" == "1" ]]; then
  make -j"$(nproc)" src/bluetoothd
  trap restart_bluetooth EXIT
  systemctl stop bluetooth.service || true
  if [[ -f /usr/libexec/bluetooth/bluetoothd && ! -f /usr/libexec/bluetooth/bluetoothd.bak ]]; then
    cp -a /usr/libexec/bluetooth/bluetoothd /usr/libexec/bluetooth/bluetoothd.bak
  fi
  install -m 755 src/bluetoothd /usr/libexec/bluetooth/bluetoothd
else
  make -j"$(nproc)"
  trap restart_bluetooth EXIT
  systemctl stop bluetooth.service || true
  if [[ -f /usr/libexec/bluetooth/bluetoothd && ! -f /usr/libexec/bluetooth/bluetoothd.bak ]]; then
    cp -a /usr/libexec/bluetooth/bluetoothd /usr/libexec/bluetooth/bluetoothd.bak
  fi
  make install
  cp -a /usr/lib/libbluetooth.so* /usr/lib/aarch64-linux-gnu/
  cp -a /usr/lib/pkgconfig/bluez.pc /usr/lib/aarch64-linux-gnu/pkgconfig/
  ldconfig
fi

trap - EXIT
restart_bluetooth
systemctl status bluetooth.service --no-pager -l | head -n 12

echo ""
echo "[bluez] Dependent BT stack status:"
for svc in bluealsa.service bluealsa-aplay.service bt-manager.service; do
  printf '  %-24s %s\n' "${svc}" "$(systemctl is-active "${svc}" 2>/dev/null || true)"
done

if ! ps -eo args | grep -q '[b]luetoothd.*--experimental' && \
   ! grep -Eq '^[[:space:]]*Experimental[[:space:]]*=[[:space:]]*true' /etc/bluetooth/main.conf 2>/dev/null; then
  echo "[bluez] WARNING: bluetoothd experimental mode is not enabled (--experimental or Experimental = true); MediaPlayer1.RefreshTrack will not be exposed." >&2
fi
EOF