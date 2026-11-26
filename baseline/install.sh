#!/usr/bin/env bash
# ==============================================================================
# Baseline Clean-Install Provisioning Script for Raspberry Pi Audio Server
# Target Architecture: aarch64 (Debian 13 Trixie / DietPi)
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Verify root privileges
if [ "$(id -u)" -ne 0 ]; then
    echo "[ERROR] This installation script must be run as root (or via sudo)." >&2
    exit 1
fi

echo "================================================================================"
echo " Starting Full Baseline Installation & Provisioning"
echo " Source Directory: ${SCRIPT_DIR}"
echo "================================================================================"

# ------------------------------------------------------------------------------
# 1. System Package Updates & Dependencies
# ------------------------------------------------------------------------------
echo "==> [1/10] Installing system packages and build dependencies..."
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y \
    cmake git build-essential autoconf automake libtool pkg-config \
    device-tree-compiler \
    alsa-utils libasound2-dev libasound2-plugins \
    libbluetooth-dev bluez bluez-tools bluez-alsa-utils \
    libglib2.0-dev libsbc-dev libdbus-1-dev \
    libfreeaptx-dev libfdk-aac-dev libopus-dev libmp3lame-dev libmpg123-dev \
    libspandsp-dev liblc3-dev systemd-dev \
    python3-pip python3-dbus python3-gi python3-pil python3-spidev python3-rpi.gpio python3-smbus2

# Install pycamilladsp Python API client
pip3 install --break-system-packages git+https://github.com/HEnquist/pycamilladsp.git

# ------------------------------------------------------------------------------
# 2. Kernel Modules Configuration
# ------------------------------------------------------------------------------
echo "==> [2/10] Configuring kernel modules (snd-aloop, i2c-dev)..."
cp "${SCRIPT_DIR}/etc/modules-load.d/snd-aloop.conf" /etc/modules-load.d/
cp "${SCRIPT_DIR}/etc/modules-load.d/i2c-dev.conf" /etc/modules-load.d/

modprobe snd-aloop || true
modprobe i2c-dev || true

# ------------------------------------------------------------------------------
# 3. Boot Config, Command Line & Device Tree Overlays
# ------------------------------------------------------------------------------
echo "==> [3/10] Installing boot firmware configs and device tree overlay..."
# Overlays
mkdir -p /boot/firmware/overlays
cp "${SCRIPT_DIR}/boot/overlays/merus-amp-custom.dtbo" /boot/firmware/overlays/

# Config & cmdline
cp "${SCRIPT_DIR}/boot/config.txt" /boot/firmware/config.txt

# Ensure console cursor is hidden if not already present
if ! grep -q "vt.global_cursor_default=0" /boot/firmware/cmdline.txt 2>/dev/null; then
    sed -i 's/$/ vt.global_cursor_default=0/' /boot/firmware/cmdline.txt
fi

# Ensure 4M atomic DMA coherent pool and disable 64-bit emulated dwc_otg FIQ/FSM
# (prevents RTL8761BU command tx timeout wedges under 60 fps visualizer load)
if [ -f /boot/firmware/cmdline.txt ]; then
    if grep -qE '(^|[[:space:]])coherent_pool=[^[:space:]]+' /boot/firmware/cmdline.txt; then
        sed -i -E 's/(^|[[:space:]])coherent_pool=[^[:space:]]+/\1coherent_pool=4M/' /boot/firmware/cmdline.txt
    else
        sed -i 's/$/ coherent_pool=4M/' /boot/firmware/cmdline.txt
    fi
    for param in dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0; do
        key="${param%%=*}"
        if grep -qE "(^|[[:space:]])${key}=[^[:space:]]+" /boot/firmware/cmdline.txt; then
            sed -i -E "s/(^|[[:space:]])${key}=[^[:space:]]+/\\1${param}/" /boot/firmware/cmdline.txt
        else
            sed -i "s/$/ ${param}/" /boot/firmware/cmdline.txt
        fi
    done
fi

# ------------------------------------------------------------------------------
# 4. ALSA Configuration & Sound Routing
# ------------------------------------------------------------------------------
echo "==> [4/10] Configuring ALSA routing and loopback definitions..."
cp "${SCRIPT_DIR}/etc/asound.conf" /etc/asound.conf
if [ -f "${SCRIPT_DIR}/etc/modprobe.d/snd-aloop.conf" ]; then
    mkdir -p /etc/modprobe.d
    cp "${SCRIPT_DIR}/etc/modprobe.d/snd-aloop.conf" /etc/modprobe.d/
fi
if [ -f "${SCRIPT_DIR}/etc/alsa/20-bluealsa.conf" ]; then
    mkdir -p /etc/alsa/conf.d
    cp "${SCRIPT_DIR}/etc/alsa/20-bluealsa.conf" /etc/alsa/conf.d/
fi

# Restore mixer state if state file exists
mkdir -p /var/lib/alsa
cp "${SCRIPT_DIR}/alsa/asound.state" /var/lib/alsa/asound.state
alsactl restore || true

# ------------------------------------------------------------------------------
# 5. CamillaDSP Installation (v5.0.0-beta.2)
# ------------------------------------------------------------------------------
echo "==> [5/10] Setting up CamillaDSP binary, configuration, and service..."
mkdir -p /etc/camilladsp /var/log/camilladsp /usr/local/bin

if [ ! -f /usr/local/bin/camilladsp ] || ! /usr/local/bin/camilladsp --version 2>/dev/null | grep -q '5\.0\.0-beta\.2'; then
    if [ -f /usr/local/bin/camilladsp ] && [ ! -f /usr/local/bin/camilladsp-4.1.3 ]; then
        cp -a /usr/local/bin/camilladsp /usr/local/bin/camilladsp-4.1.3
    fi
    CAMILLADSP_SHA256="f8e8e0b1dee43083eb61ab3f05a43984b41415854dbd7616d890b5a57719ecee"
    echo "Downloading CamillaDSP v5.0.0-beta.2 aarch64 release (or build from source via scripts/build_camilladsp_pi.sh)..."
    wget -q https://github.com/HEnquist/camilladsp/releases/download/v5.0.0-beta.2/camilladsp-linux-aarch64.tar.gz -O /tmp/camilladsp.tar.gz
    echo "${CAMILLADSP_SHA256}  /tmp/camilladsp.tar.gz" | sha256sum -c -
    tar -xzf /tmp/camilladsp.tar.gz -C /usr/local/bin/
    chmod +x /usr/local/bin/camilladsp
    rm -f /tmp/camilladsp.tar.gz
fi

cp "${SCRIPT_DIR}/etc/camilladsp/crossover.yml" /etc/camilladsp/
cp "${SCRIPT_DIR}/etc/systemd/system/camilladsp.service" /etc/systemd/system/

# ------------------------------------------------------------------------------
# 6. Build Sony LDAC Library from Source
# ------------------------------------------------------------------------------
echo "==> [6/10] Building and installing ldacBT from source..."
mkdir -p /usr/src
if [ ! -f /usr/lib/aarch64-linux-gnu/libldacBT_enc.so ] && [ ! -f /usr/lib/libldacBT_enc.so ]; then
    cd /usr/src
    rm -rf ldacBT
    git clone --recurse-submodules https://github.com/EHfive/ldacBT.git
    cd ldacBT
    mkdir -p build && cd build
    cmake -DCMAKE_INSTALL_PREFIX=/usr -DLDAC_SOFT_FLOAT=OFF -DINSTALL_LIBDIR=/usr/lib/aarch64-linux-gnu ..
    make -j"$(nproc)"
    make install
    ldconfig
fi

# ------------------------------------------------------------------------------
# 7. BlueALSA — bootstrap only (private v5 stack is built separately)
# ------------------------------------------------------------------------------
# BlueZ precedent: this script only bootstraps the distro bluez-alsa-utils
# (already installed in step 1) so audio works on a fresh image. The
# authoritative, A53-tuned BlueALSA v5.0.0 stack (private binaries + codec
# libraries under /usr/local, apt package removed) is built by the host-side
# driver `scripts/build_bluealsa_pi.sh` against the pinned commits in
# `scripts/codecs/versions.lock`. Run that after this baseline install to
# replace the bootstrap with the private stack. Keeping the build in one
# place avoids two divergent recipes (this file previously built v4.3.1).
echo "==> [7/10] BlueALSA bootstrap (distro package); private v5 via scripts/build_bluealsa_pi.sh"

# BlueALSA runtime state path (survives the later private install)
mkdir -p /var/lib/bluealsa
mkdir -p /usr/var/lib
ln -sfn /var/lib/bluealsa /usr/var/lib/bluealsa
chmod 700 /var/lib/bluealsa

# ------------------------------------------------------------------------------
# 8. Bluetooth & Auto-Trust Headless Agent
# ------------------------------------------------------------------------------
echo "==> [8/10] Configuring Bluetooth stack & auto-trust agent..."
cp "${SCRIPT_DIR}/etc/bluetooth/main.conf" /etc/bluetooth/
cp "${SCRIPT_DIR}/bin/bt-auto-trust-agent.py" /usr/local/bin/
chmod +x /usr/local/bin/bt-auto-trust-agent.py

cp "${SCRIPT_DIR}/etc/systemd/system/bluealsa.service" /etc/systemd/system/
cp "${SCRIPT_DIR}/etc/systemd/system/bluealsa-aplay.service" /etc/systemd/system/
cp "${SCRIPT_DIR}/etc/systemd/system/bt-agent.service" /etc/systemd/system/

mkdir -p /etc/dbus-1/system.d
cp "${SCRIPT_DIR}/etc/dbus-1/system.d/"*.conf /etc/dbus-1/system.d/
systemctl reload dbus || true

# ------------------------------------------------------------------------------
# 9. Hardware Diagnostic & Monitoring Scripts
# ------------------------------------------------------------------------------
echo "==> [9/10] Deploying hardware test and monitoring scripts to /root/..."
cp "${SCRIPT_DIR}/scripts/read_temp.py" /root/
cp "${SCRIPT_DIR}/scripts/read_light_level.py" /root/
cp "${SCRIPT_DIR}/scripts/test_fan.py" /root/
cp "${SCRIPT_DIR}/scripts/test_all_encoder.py" /root/
if [ -f "${SCRIPT_DIR}/scripts/render_ui.py" ]; then
    cp "${SCRIPT_DIR}/scripts/render_ui.py" /root/
fi

mkdir -p /root/amp /root/freenove
cp -r "${SCRIPT_DIR}/scripts/amp/"* /root/amp/
cp -r "${SCRIPT_DIR}/scripts/freenove/"* /root/freenove/
chmod +x /root/*.py /root/amp/*.py /root/freenove/*.py 2>/dev/null || true

# ------------------------------------------------------------------------------
# 10. Enable & Activate Services
# ------------------------------------------------------------------------------
echo "==> [10/10] Reloading systemd daemons and enabling audio services..."
systemctl daemon-reload

systemctl enable bluetooth.service
systemctl enable bt-agent.service
systemctl enable bluealsa.service
systemctl enable bluealsa-aplay.service
systemctl enable camilladsp.service

systemctl restart bluetooth.service || true
systemctl restart bt-agent.service || true
systemctl restart bluealsa.service || true
systemctl restart camilladsp.service || true
systemctl restart bluealsa-aplay.service || true

# ------------------------------------------------------------------------------
# 11. rmpd User Service & Streaming Config (optional binary via build_rmpd_pi.sh)
# ------------------------------------------------------------------------------
if id -u dietpi >/dev/null 2>&1; then
    echo "==> [11/11] Configuring rmpd user service & template for dietpi..."
    usermod -aG audio dietpi || true
    systemctl unmask systemd-logind.service || true
    systemctl start systemd-logind.service || true
    loginctl enable-linger dietpi || true
    systemctl start user@1000.service || true
    install -d -o dietpi -g dietpi -m 755 \
        /home/dietpi/.config \
        /home/dietpi/.config/systemd \
        /home/dietpi/.config/systemd/user \
        /home/dietpi/.config/rmpd \
        /home/dietpi/.config/rmpd/playlists
    install -m 644 -o dietpi -g dietpi "${SCRIPT_DIR}/etc/systemd/user/rmpd.service" /home/dietpi/.config/systemd/user/rmpd.service
    if [ ! -f /home/dietpi/.config/rmpd/rmpd.toml ]; then
        install -m 600 -o dietpi -g dietpi "${SCRIPT_DIR}/etc/rmpd/rmpd.toml.example" /home/dietpi/.config/rmpd/rmpd.toml
    fi
    if [ -x /usr/local/bin/rmpd ] && [ -S /run/user/1000/bus ]; then
        sudo -u dietpi XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus systemctl --user daemon-reload || true
        sudo -u dietpi XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus systemctl --user enable --now rmpd.service || true
    fi
fi

echo "================================================================================"
echo " [SUCCESS] Baseline installation and provisioning completed successfully!"
echo " A reboot is recommended to apply device tree overlays and boot parameters."
echo "================================================================================"
