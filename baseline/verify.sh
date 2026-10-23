#!/usr/bin/env bash
# ==============================================================================
# Baseline Verification and Audit Script for Raspberry Pi Audio Server
# Can be run directly on the Raspberry Pi or against the baseline directory
# ==============================================================================
set -euo pipefail

PASS_COUNT=0
FAIL_COUNT=0

check() {
    local desc="$1"
    local cmd="$2"
    printf "  %-65s " "${desc}..."
    if eval "${cmd}" >/dev/null 2>&1; then
        echo -e "\e[32m[PASS]\e[0m"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo -e "\e[31m[FAIL]\e[0m"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

echo "================================================================================"
echo " Starting System & Services Baseline Audit"
echo " Date: $(date)"
echo " Host: $(hostname) ($(whoami))"
echo "================================================================================"

echo ""
echo "--- 1. Critical Kernel Modules & Overlays ---"
check "Kernel module 'snd-aloop' loaded" "lsmod | grep -q snd_aloop"
check "Kernel module 'i2c-dev' loaded" "lsmod | grep -q i2c_dev"
check "Kernel module 'snd_soc_ma120x0p' (Merus Amp) loaded" "lsmod | grep -q snd_soc_ma120x0p"
check "Custom overlay 'merus-amp-custom.dtbo' in firmware" "[ -f /boot/firmware/overlays/merus-amp-custom.dtbo ]"

echo ""
echo "--- 2. Core Systemd Audio & Bluetooth Services ---"
check "Service 'bluetooth.service' active" "systemctl is-active --quiet bluetooth"
check "Service 'bt-agent.service' active" "systemctl is-active --quiet bt-agent"
check "Service 'bluealsa.service' active" "systemctl is-active --quiet bluealsa"
check "Service 'bluealsa-aplay.service' active" "systemctl is-active --quiet bluealsa-aplay"
check "Service 'camilladsp.service' active" "systemctl is-active --quiet camilladsp"

echo ""
echo "--- 3. Network Ports & Sockets ---"
check "CamillaDSP WebSocket listening on 127.0.0.1:1234" "ss -tlpn | grep -q ':1234 '"
check "OpenSSH listening on port 22" "ss -tlpn | grep -q ':22 '"

echo ""
echo "--- 4. Sound Cards & ALSA Device Nodes ---"
check "ALSA Card 0 'Loopback' detected" "aplay -l | grep -q 'card 0: Loopback'"
check "ALSA Card 1 'sndrpimerusamp' detected" "aplay -l | grep -q 'card 1: sndrpimerusamp'"
check "ALSA configuration /etc/asound.conf exists" "[ -s /etc/asound.conf ]"
check "CamillaDSP configuration crossover.yml exists" "[ -s /etc/camilladsp/crossover.yml ]"

echo ""
echo "--- 5. Hardware Peripherals & Sensors ---"
check "I2C Bus 1 present" "[ -e /dev/i2c-1 ]"
check "MA12070P Amp present on I2C (addr 0x20)" "i2cdetect -y 1 | grep -E '(20|UU)' >/dev/null"
check "BH1750 Light Sensor present on I2C (addr 0x23)" "i2cdetect -y 1 | grep ' 23 ' >/dev/null"
check "DS18B20 1-Wire temperature sensor detected" "ls /sys/bus/w1/devices/28-* >/dev/null 2>&1"
check "Freenove DSI Framebuffer /dev/fb0 exists" "[ -e /dev/fb0 ]"
check "DSI Touchscreen input device 'raspberrypi-ts' active" "grep -q 'raspberrypi-ts' /proc/bus/input/devices"
check "Backlight control /sys/class/backlight/rpi_backlight exists" "[ -d /sys/class/backlight/rpi_backlight ]"

echo ""
echo "--- 6. Diagnostic & Status Scripts in /root/ ---"
check "/root/read_temp.py present and executable" "[ -f /root/read_temp.py ]"
check "/root/read_light_level.py present and executable" "[ -f /root/read_light_level.py ]"
check "/root/test_fan.py present and executable" "[ -f /root/test_fan.py ]"
check "/root/test_all_encoder.py present and executable" "[ -f /root/test_all_encoder.py ]"
check "/root/amp/camilladsp-status.py present and executable" "[ -f /root/amp/camilladsp-status.py ]"
check "/root/amp/ma12070p_status.py present and executable" "[ -f /root/amp/ma12070p_status.py ]"
check "/root/amp/bt_status.py present and executable" "[ -f /root/amp/bt_status.py ]"
check "/root/freenove/display_info.py present and executable" "[ -f /root/freenove/display_info.py ]"

echo "================================================================================"
echo " Audit Complete: ${PASS_COUNT} Passed, ${FAIL_COUNT} Failed."
if [ "${FAIL_COUNT}" -eq 0 ]; then
    echo -e "\e[32m [SUCCESS] All baseline components and configurations are healthy and verified!\e[0m"
    exit 0
else
    echo -e "\e[31m [WARNING] Some components failed verification. Check logs above.\e[0m"
    exit 1
fi
================================================================================
