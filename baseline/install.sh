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

# ------------------------------------------------------------------------------
# 4. ALSA Configuration & Sound Routing
# ------------------------------------------------------------------------------
echo "==> [4/10] Configuring ALSA routing and loopback definitions..."
cp "${SCRIPT_DIR}/etc/asound.conf" /etc/asound.conf
if [ -f "${SCRIPT_DIR}/etc/alsa/20-bluealsa.conf" ]; then
    mkdir -p /etc/alsa/conf.d
    cp "${SCRIPT_DIR}/etc/alsa/20-bluealsa.conf" /etc/alsa/conf.d/
fi

# Restore mixer state if state file exists
mkdir -p /var/lib/alsa
cp "${SCRIPT_DIR}/alsa/asound.state" /var/lib/alsa/asound.state
alsactl restore || true

# ------------------------------------------------------------------------------
# 5. CamillaDSP Installation (v4.1.3)
# ------------------------------------------------------------------------------
echo "==> [5/10] Setting up CamillaDSP binary, crossover filters, and service..."
mkdir -p /etc/camilladsp /var/log/camilladsp /usr/local/bin

if [ ! -f /usr/local/bin/camilladsp ]; then
    echo "Downloading CamillaDSP v4.1.3 aarch64 release..."
    wget -q https://github.com/HEnquist/camilladsp/releases/download/v4.1.3/camilladsp-linux-aarch64.tar.gz -O /tmp/camilladsp.tar.gz
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
# 7. Build BlueALSA v4.3.1 from Source with Full Codec Support
# ------------------------------------------------------------------------------
echo "==> [7/10] Building and installing BlueALSA v4.3.1 from source..."
cd /usr/src
if [ ! -d bluez-alsa ]; then
    git clone https://github.com/arkq/bluez-alsa.git
fi
cd bluez-alsa
git checkout v4.3.1
autoreconf --install --force

mkdir -p build && cd build
make clean || true

../configure \
    --prefix=/usr \
    --sysconfdir=/etc \
    --localstatedir=/var \
    --with-alsaconfdir=/etc/alsa/conf.d \
    --enable-systemd \
    --enable-aplay \
    --enable-faststream \
    --enable-opus \
    --enable-mp3lame \
    --enable-mpg123 \
    --enable-aptx \
    --enable-aptx-hd \
    --with-libfreeaptx \
    --enable-aac \
    --enable-ldac \
    --enable-lc3-swb \
    --enable-msbc \
    --enable-midi

make -j"$(nproc)"
make install
ldconfig

# BlueALSA runtime state paths & compatibility symlinks
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

echo "================================================================================"
echo " [SUCCESS] Baseline installation and provisioning completed successfully!"
echo " A reboot is recommended to apply device tree overlays and boot parameters."
echo "================================================================================"
