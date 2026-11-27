#!/usr/bin/env bash
#
# build_mesa_vc4_pi.sh - Cross-build a minimal vc4-only, LLVM-free Mesa stack
#                        on the host (via debian:trixie-slim Podman) and deploy
#                        it privately for screen-manager on the Raspberry Pi 3.
#
# Eliminates Debian's monolithic libgallium -> libLLVM.so.19.1 dependency
# (~42 MB RSS + ~5 MB libgallium delta) while keeping hardware VC4 OpenGL ES.
# Zero compilation happens on the Raspberry Pi.
#
# Phases (default: build):
#   --phase build     cross-compile vc4-only Mesa in debian:trixie-slim podman
#   --phase deploy    copy built .so files to /usr/local/lib/mesa-vc4 on the Pi
#   --phase apply     install systemd drop-in for screen-manager.service and
#                     restart it (REQUIRES --i-have-approval)
#   --phase verify    verify VC4 GLES renderer & absence of libLLVM on the Pi
#   --all             build, deploy, apply, verify (requires --i-have-approval)
#
# Options:
#   --out-dir <path>  host output directory (default: /tmp/mesa-vc4-out)
#   --i-have-approval explicit authorization for --phase apply mutations
#   -h                show this help
#
# Environment overrides:
#   REMOTE_HOST (default: root@192.168.2.120)

set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.2.120}"
OUT_DIR="/tmp/mesa-vc4-out"
PHASE="build"
APPROVAL=0

usage() { sed -n '2,28p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --phase) PHASE="${2:?--phase requires an argument}"; shift 2 ;;
    --all) PHASE="all"; shift ;;
    --out-dir) OUT_DIR="${2:?--out-dir requires an argument}"; shift 2 ;;
    --i-have-approval) APPROVAL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

log() { echo "[mesa-vc4] $*"; }
die() { echo "[mesa-vc4] ERROR: $*" >&2; exit 1; }

LIB_SUBDIR="usr/local/lib/aarch64-linux-gnu"
REMOTE_LIB_DIR="/usr/local/lib/mesa-vc4"

do_build() {
  command -v podman >/dev/null || die "podman is required on the host"
  mkdir -p "${OUT_DIR}"
  log "Cross-building vc4-only Mesa (LLVM disabled) in debian:trixie-slim -> ${OUT_DIR}"

  podman run --rm --network=host -v "${OUT_DIR}:/out" docker.io/library/debian:trixie-slim bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    sed -i "s/^Types: deb$/Types: deb deb-src/" /etc/apt/sources.list.d/debian.sources
    dpkg --add-architecture arm64
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      ca-certificates gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
      meson ninja-build pkg-config python3-mako python3-yaml python3-packaging \
      flex bison glslang-tools dpkg-dev \
      libdrm-dev:arm64 libexpat1-dev:arm64 zlib1g-dev:arm64 libglvnd-dev:arm64

    mkdir -p /src && cd /src
    apt-get source -qq mesa
    cd mesa-*

    cat > cross-aarch64.txt <<EOF
[binaries]
c = '\''aarch64-linux-gnu-gcc'\''
cpp = '\''aarch64-linux-gnu-g++'\''
ar = '\''aarch64-linux-gnu-ar'\''
strip = '\''aarch64-linux-gnu-strip'\''
pkg-config = '\''pkg-config'\''

[properties]
pkg_config_libdir = '\''/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig'\''
sys_root = '\''/'\''

[host_machine]
system = '\''linux'\''
cpu_family = '\''aarch64'\''
cpu = '\''cortex-a53'\''
endian = '\''little'\''
EOF

    PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
    meson setup build --cross-file cross-aarch64.txt \
      --prefix=/usr/local \
      --libdir=lib/aarch64-linux-gnu \
      -Dbuildtype=release \
      -Db_ndebug=true \
      -Dc_args="-mcpu=cortex-a53" \
      -Dcpp_args="-mcpu=cortex-a53" \
      -Dgallium-drivers=vc4 \
      -Dvulkan-drivers= \
      -Dllvm=disabled \
      -Dglx=disabled \
      -Dplatforms= \
      -Degl=enabled \
      -Dgbm=enabled \
      -Dgles1=disabled \
      -Dgles2=enabled \
      -Dopengl=true \
      -Dglvnd=enabled \
      -Dgallium-va=disabled \
      -Dgallium-vdpau=disabled \
      -Dgallium-xa=disabled \
      -Dvalgrind=disabled \
      -Dlibunwind=disabled \
      -Dlmsensors=disabled \
      -Dzstd=disabled

    ninja -C build -j"$(nproc)"
    DESTDIR=/out ninja -C build install
    find /out -type f -name "*.so*" -exec aarch64-linux-gnu-strip --strip-unneeded {} +

    shopt -s nullglob
    gallium_sos=(/out/usr/local/lib/aarch64-linux-gnu/libgallium-*.so)
    (( ${#gallium_sos[@]} > 0 )) || { echo "ERROR: libgallium-*.so not found" >&2; exit 1; }
    for so in "${gallium_sos[@]}"; do
      if aarch64-linux-gnu-readelf -d "${so}" | grep -qi llvm; then
        echo "ERROR: ${so} still references LLVM!" >&2
        exit 1
      fi
    done
  '

  log "OK: vc4-only Mesa built cleanly without LLVM."
}

do_deploy() {
  local src="${OUT_DIR}/${LIB_SUBDIR}"
  [[ -d "${src}" ]] || die "Build output not found at ${src}; run --phase build first"

  log "Deploying vc4-only Mesa (preserving symlinks) to ${REMOTE_HOST}:${REMOTE_LIB_DIR}"
  local staging="${REMOTE_LIB_DIR}.new.$$"
  ssh "${REMOTE_HOST}" "rm -rf '${staging}' && mkdir -p '${staging}'"
  # Stream via tar so .so symlinks are preserved instead of dereferenced.
  (
    cd "${src}"
    tar -cf - libgallium-*.so libEGL_mesa.so* libgbm.so* gbm/dri_gbm.so
  ) | ssh "${REMOTE_HOST}" "tar -C '${staging}' -xf - && rm -rf '${REMOTE_LIB_DIR}' && mv '${staging}' '${REMOTE_LIB_DIR}'"
  log "OK: deployed to ${REMOTE_LIB_DIR}"
}

do_apply() {
  [[ "${APPROVAL}" -eq 1 ]] || die "--phase apply requires --i-have-approval"
  log "Installing systemd drop-in for screen-manager.service and restarting"
  ssh "${REMOTE_HOST}" "mkdir -p /etc/systemd/system/screen-manager.service.d && cat > /etc/systemd/system/screen-manager.service.d/mesa-vc4.conf <<'EOF'
[Service]
Environment=\"LD_LIBRARY_PATH=${REMOTE_LIB_DIR}\"
Environment=\"GBM_BACKENDS_PATH=${REMOTE_LIB_DIR}/gbm\"
Environment=\"MALLOC_ARENA_MAX=2\"
Environment=\"MALLOC_MMAP_THRESHOLD_=65536\"
EOF
systemctl daemon-reload
systemctl restart screen-manager.service"
  log "OK: screen-manager.service restarted with vc4-only Mesa."
}

do_verify() {
  log "Verifying VC4 GLES renderer and smaps on ${REMOTE_HOST}"
  ssh "${REMOTE_HOST}" "
    set -euo pipefail
    info=\$(LD_LIBRARY_PATH=${REMOTE_LIB_DIR} GBM_BACKENDS_PATH=${REMOTE_LIB_DIR}/gbm eglinfo -B 2>&1)
    echo \"\${info}\" | grep -E 'vendor|renderer|version' | sed -n '1,10p'
    if ! grep -q 'VC4 V3D' <<<\"\${info}\"; then
      echo 'ERROR: VC4 V3D hardware renderer not detected!' >&2
      exit 1
    fi
    pid=\$(pgrep -x screen-manager || true)
    if [[ -n \"\${pid}\" ]]; then
      if grep -q LLVM \"/proc/\${pid}/maps\"; then
        echo 'ERROR: libLLVM is still mapped in screen-manager!' >&2
        exit 1
      fi
      grep -E '^(Rss|Pss|Pss_Anon|Pss_File):' \"/proc/\${pid}/smaps_rollup\"
    fi
  "
}

case "${PHASE}" in
  build) do_build ;;
  deploy) do_deploy ;;
  apply) do_apply ;;
  verify) do_verify ;;
  all) do_build; do_deploy; do_apply; do_verify ;;
  *) die "Unknown phase: ${PHASE}" ;;
esac
