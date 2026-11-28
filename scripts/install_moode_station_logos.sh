#!/usr/bin/env bash
#
# install_moode_station_logos.sh - Fetch moOde's curated radio station logos
# and install them to /var/lib/speaker/station-logos/ (and optionally regenerate
# /etc/speaker/stations.toml) without keeping third-party JPEGs in git history.
#
# Usage:
#   scripts/install_moode_station_logos.sh                # install logos on local host
#   scripts/install_moode_station_logos.sh --with-toml    # also regenerate /etc/speaker/stations.toml

set -euo pipefail

LOGO_DEST="${LOGO_DEST:-/var/lib/speaker/station-logos}"
TOML_DEST="${TOML_DEST:-/etc/speaker/stations.toml}"
WITH_TOML=0

if [[ "${1:-}" == "--with-toml" ]]; then
  WITH_TOML=1
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

echo "[moode-logos] Sparse-cloning moode-player/moode into ${TMP_DIR}..."
git clone --depth 1 --filter=blob:none --sparse \
  https://github.com/moode-player/moode.git "${TMP_DIR}/moode" >/dev/null
git -C "${TMP_DIR}/moode" sparse-checkout set --no-cone \
  var/local/www/imagesw/radio-logos \
  var/local/www/db/moode-sqlite3.db.sql >/dev/null

mkdir -p "${LOGO_DEST}"
cp -f "${TMP_DIR}/moode/var/local/www/imagesw/radio-logos/"*.jpg "${LOGO_DEST}/"
chmod 644 "${LOGO_DEST}/"*.jpg
echo "[moode-logos] Installed $(ls -1 "${LOGO_DEST}/"*.jpg | wc -l) station logos to ${LOGO_DEST}"

if [[ "${WITH_TOML}" -eq 1 ]]; then
  mkdir -p "$(dirname "${TOML_DEST}")"
  python3 - "${TMP_DIR}/moode/var/local/www/imagesw/radio-logos" \
    "${TMP_DIR}/moode/var/local/www/db/moode-sqlite3.db.sql" \
    "${LOGO_DEST}" "${TOML_DEST}" <<'PY'
import json, os, re, sys

logo_dir, sql_path, logo_dest, toml_dest = sys.argv[1:5]
logos = {f for f in os.listdir(logo_dir) if f.endswith(".jpg")}
pat = re.compile(r"INSERT INTO cfg_radio .*? VALUES \(\d+, '([^']*)', '([^']*)',")
rules = []
with open(sql_path, encoding="utf-8", errors="replace") as f:
    for line in f:
        m = pat.search(line)
        if m:
            url, name = m.group(1), m.group(2)
            fname = f"{name}.jpg"
            if fname in logos:
                rules.append((url, name, fname))

with open(toml_dest, "w", encoding="utf-8") as out:
    out.write("# Auto-generated from moOde cfg_radio + radio-logos\n")
    out.write("enable_track_art_lookup = true\n\n")
    for url, name, fname in rules:
        out.write("[[station]]\n")
        out.write("name_match = " + json.dumps(re.escape(name)) + "\n")
        if url:
            out.write("url_match = " + json.dumps(re.escape(url)) + "\n")
        out.write("logo = " + json.dumps(f"{logo_dest}/{fname}") + "\n\n")

print(f"[moode-logos] Wrote {len(rules)} station rules to {toml_dest}")
PY
fi
