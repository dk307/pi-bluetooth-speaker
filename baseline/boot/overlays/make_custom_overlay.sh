#!/usr/bin/env bash
# ==============================================================================
# Generate merus-amp-custom.dtbo by stripping pinctrl from stock overlay
# ==============================================================================
set -euo pipefail

OVERLAY_DIR="/boot/firmware/overlays"
STOCK_DTBO="${OVERLAY_DIR}/merus-amp.dtbo"
CUSTOM_DTBO="${OVERLAY_DIR}/merus-amp-custom.dtbo"
TMP_DTS="/tmp/merus-amp.dts"
TMP_CUSTOM_DTS="/tmp/merus-amp-custom.dts"

if [ ! -f "${STOCK_DTBO}" ]; then
    echo "[ERROR] Stock overlay not found at: ${STOCK_DTBO}" >&2
    exit 1
fi

if ! command -v dtc >/dev/null 2>&1; then
    echo "[INFO] Installing device tree compiler (device-tree-compiler)..."
    apt-get update && apt-get install -y device-tree-compiler
fi

echo "[1/3] Decompiling stock ${STOCK_DTBO} to DTS..."
dtc -I dtb -O dts "${STOCK_DTBO}" -o "${TMP_DTS}"

echo "[2/3] Stripping 'pinctrl' properties to avoid GPIO conflicts..."
grep -v "pinctrl" "${TMP_DTS}" > "${TMP_CUSTOM_DTS}"

echo "[3/3] Compiling patched DTS into ${CUSTOM_DTBO}..."
dtc -I dts -O dtb -o "${CUSTOM_DTBO}" "${TMP_CUSTOM_DTS}"

# Cleanup
rm -f "${TMP_DTS}" "${TMP_CUSTOM_DTS}"

echo "[SUCCESS] Generated ${CUSTOM_DTBO} successfully."
