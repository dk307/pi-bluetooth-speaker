# MPD v0.25 (C++ Music Player Daemon) Build & Integration Plan

> Status: **executed & deployed** (2026-11-26) — cross-compiled `MPD v0.25` with
> Cortex-A53 tuning, verified ABI (`GLIBC <= 2.41`, `GLIBCXX <= 3.4.33`),
> deployed to `/usr/local/bin/mpd` on `192.168.2.120`, removed all `rmpd`
> traces, and verified live radio playback through `src_rmpd` → `hw:Loopback,0,1`
> → `camilladsp` → `audio-manager`.
> Scope: cross-compile latest upstream [`MusicPlayerDaemon/MPD`](https://github.com/MusicPlayerDaemon/MPD)
> **`v0.25`** (`e411a30052b1417523f8d9032ac9a69596ed098b`) with Cortex-A53 tuning
> for the Raspberry Pi 3, install strictly to `/usr/local/bin/mpd` (**never**
> `apt install mpd`), install required runtime libraries + `mpc` via `apt`,
> configure **internet radio & streaming** via `.m3u`/`.pls`/`.xspf`/`.asx`
> playlists and M.A.L.P., and route audio through the live `src_rmpd` ALSA
> Loopback subdevice (`hw:Loopback,0,1`) → CamillaDSP → Merus amp.
> **Out of scope for this PR:** D-Bus/MPRIS (`dbus=disabled`), mDNS/Zeroconf
> (`zeroconf=disabled`, deferred), local disk library (`database=false`), HTTP
> JSON-RPC sidecars, and modifying `/etc/asound.conf` or `audio-manager`.

---

## 1. Problem Statement

Debian 13 Trixie ships `mpd 0.24.4-1`, whereas upstream `MusicPlayerDaemon/MPD`
is at **`v0.25`** (C++23, Meson + Ninja). Building `v0.25` from source with
Cortex-A53 optimization (`-O3 -mcpu=cortex-a53+crc+simd`) and installing it to
`/usr/local/bin/mpd` gives us:

- **Latest upstream MPD (`v0.25`)** with native `libasound2` ALSA output
  (`buffer_time = 500000` µs default, avoiding `cpal` buffer-size issues).
- **Internet radio & streaming support:** `curl`, `ffmpeg` (HLS/containers),
  `expat` (`.asx`/`.xspf`/`.rss`), built-in `m3u`/`extm3u`/`pls`, plus `faad`,
  `mpg123`, `flac`, `opus`, `vorbis`, and `soxr`.
- **Ultra-lean footprint:** `auto_features=disabled` in Meson — local-library
  features (`database=false`, `inotify=false`, `sqlite=disabled`, `pcre=disabled`,
  `cue=false`, `neighbor=false`), `dbus=disabled`, and `zeroconf=disabled` are
  all turned off.
- **No distro `mpd` package conflict:** Debian's `mpd` package is never
  installed (no `/usr/bin/mpd`, no loopback-only `mpd.socket`). Only runtime
  shared libraries and `mpc` are installed via `apt`.
- **Drop-in compatibility with live `audio-manager`:** Matches live
  `/etc/asound.conf` (`device "src_rmpd"` → `hw:Loopback,0,1`) and MPD TCP port
  `6600` (`0.0.0.0:6600`).

---

## 2. Architecture Decisions

| Concern | Choice | Rationale |
| :--- | :--- | :--- |
| Upstream source | `MusicPlayerDaemon/MPD` **`v0.25`** (`e411a30052b1417523f8d9032ac9a69596ed098b`, annotated tag `22eafc09863ede452d9817a3367c822a614754d1`) | Latest stable upstream release. |
| Binary placement | `/usr/local/bin/mpd` (source build only; **never** `apt install mpd`) | Keeps custom Cortex-A53 binary isolated in `/usr/local/bin` with zero apt conflicts or `mpd.socket` overrides. |
| Build system | **Host cross-compile** (`meson` 1.10 + `ninja` 1.13 + `aarch64-linux-gnu-g++` 15.2.0, C++23) | Links against a pinned Debian 13 Trixie `arm64` sysroot (`--sysroot` + `-Wl,-rpath-link`) so all `DT_NEEDED` sonames and `GLIBC <= 2.41` match the Pi. |
| CPU tuning | `-O3 -mcpu=cortex-a53+crc+simd -ftree-vectorize` | Canonical Pi 3 Cortex-A53 tuning for GCC 15. |
| Meson features | `auto_features=disabled` + `alsa=enabled`, `curl=enabled`, `ffmpeg=enabled`, `expat=enabled`, `faad=enabled`, `mpg123=enabled`, `flac=enabled`, `opus=enabled`, `vorbis=enabled`, `soxr=enabled`, `systemd=enabled`, `tcp=true`, `local_socket=true`; **`database=false`**, `inotify=false`, `cue=false`, `neighbor=false`, `dsd=false`, `sqlite=disabled`, `pcre=disabled`, `dbus=disabled`, `zeroconf=disabled` | Ultra-lean radio/streaming-only build. Note: `database=false` disables `lsinfo`/`find`/`search` (in M.A.L.P., Playlists and direct stream URLs work; Library/Files tabs return `No database`). |
| CLI companion | `mpc` (`0.35-1+b2` via `apt`) | Lightweight CLI client for SSH control & smoke tests (independent of Debian's `mpd` package). |
| Service model | **System service** (`baseline/etc/systemd/system/mpd.service`, `Type=notify`, `User=mpd`, `Group=audio`, `SupplementaryGroups=audio`, `StateDirectory=mpd`, `ExecStart=/usr/local/bin/mpd --systemd /etc/mpd.conf`) | Without D-Bus/MPRIS, a system service needs no `systemd-logind` or user lingering. `user` is omitted from `/etc/mpd.conf` because systemd drops privileges directly. |
| Audio routing | ALSA `device "src_rmpd"` → `hw:Loopback,0,1` → CamillaDSP | Matches live `/etc/asound.conf` (`pcm.src_rmpd`) and live `audio-manager` multi-source routing (§3). |
| Volume / mixer | `mixer_type "none"` | Matches `bluealsa-aplay --volume=none`. `audio-manager` exclusively owns the Merus `A.Mstr Vol` mixer. |
| D-Bus / mDNS | **Disabled / deferred** | `dbus=disabled` and `zeroconf=disabled` (`avahi-daemon` not required). |

---

## 3. Audio Output Routing & Live `audio-manager` Integration

Verified live on `192.168.2.120`:
- `/etc/asound.conf` defines isolated per-source ALSA Loopback aliases:
  - `pcm.src_bluetooth` → `plug` → `hw:Loopback,0,0` (`bluealsa-aplay`)
  - `pcm.src_rmpd` → `plug` → `hw:Loopback,0,1` (`mpd`)
- Live `audio-manager` (`PID 15414`) dynamically switches CamillaDSP's capture
  device between `hw:Loopback,1,0` and `hw:Loopback,1,1` and connects over MPD
  TCP (`:6600`) to pause the MPD source when Bluetooth preempts.

```text
pcm.src_bluetooth  →  plug  →  hw:Loopback,0,0   (bluealsa-aplay)
pcm.src_rmpd       →  plug  →  hw:Loopback,0,1   (mpd v0.25)
        │
        ▼  (audio-manager dynamically routes capture_device + sample rate)
camilladsp.service (active 2-way crossover & limiters)
        │
        ▼
hw:sndrpihifiberry,0 (S32_LE, Merus MA12070P amp; volume owned by audio-manager)
```

`mpd` targets `device "src_rmpd"` with `mixer_type "none"`:

```text
audio_output {
    type        "alsa"
    name        "ALSA"
    device      "src_rmpd"
    mixer_type  "none"
}
```

> **Baseline note:** `baseline/etc/asound.conf` on `origin/main` (`2490ef8`)
> does not yet include the `pcm.src_rmpd` alias (that change lives on the
> separate `audio-manager` multi-source branch already deployed to the Pi). This
> PR intentionally does not modify `baseline/etc/asound.conf` or live
> `/etc/asound.conf`; `device "src_rmpd"` expects the multi-source `/etc/asound.conf`.

---

## 4. Runtime & Cross-Build Dependencies (Verified Against Live Pi)

### Pi Runtime Packages (`192.168.2.120`, Debian 13 Trixie `arm64`)

**Already installed on Pi (13 packages):**

| Package | Installed Version | Purpose |
| :--- | :--- | :--- |
| `libasound2t64` | `1.2.14-1+rpt1+deb13u1` | ALSA output |
| `libcurl4t64` | `8.14.1-2+deb13u4` | HTTP/HTTPS radio input & ICY metadata (`curl`) |
| `libavcodec61`, `libavutil59` | `8:7.1.5-0+deb13u1+rpt2` | FFmpeg codecs/utils |
| `libexpat1` | `2.9.0-1~deb13u1` | `.asx`, `.xspf`, `.rss` playlist plugins |
| `libmpg123-0t64` | `1.32.10-1+deb13u1` | MP3 decoder |
| `libflac14` | `1.5.0+ds-2` | FLAC / Ogg FLAC decoder |
| `libopus0` | `1.5.2-2` | Opus decoder |
| `libvorbis0a`, `libogg0` | `1.3.7-3`, `1.3.5-3+b2` | Ogg Vorbis decoder |
| `libsoxr0` | `0.1.3-4+b2` | Resampler (`soxr`) |
| `libsystemd0` | `257.13-1~deb13u1` | `Type=notify` (`--systemd`) |
| `libfmt10` | `10.1.1+ds1-4` | Required by `MPD v0.25` core (`libfmt >= 9`) |

**To install via `apt` in `--phase apply` (only 3 packages; never `mpd`):**
- `libavformat61` — FFmpeg HLS / container demuxer
- `libfaad2` — AAC / HE-AAC decoder
- `mpc` — MPD CLI client (`0.35-1+b2`)

### Host Build Tools & Trixie `arm64` Sysroot
- **Host tools (verified present):** `meson` (1.10), `ninja` (1.13),
  `aarch64-linux-gnu-gcc`, `aarch64-linux-gnu-g++` (15.2.0),
  `aarch64-linux-gnu-objdump`, `pkg-config`, `dpkg-deb`, `curl`.
- **Pinned Trixie `arm64` sysroot (`scripts/sysroot_mpd_trixie_arm64.list`):**
  Development (`-dev`) + runtime `.deb` packages resolved from Debian Trixie
  `main/binary-arm64` `Packages` with SHA-256 verification: `libasound2-dev`,
  `libcurl4-openssl-dev`, `libavformat-dev`, `libavcodec-dev`, `libavutil-dev`,
  `libexpat1-dev`, `libfaad-dev`, `libmpg123-dev`, `libflac-dev`, `libopus-dev`,
  `libvorbis-dev`, `libogg-dev`, `libsoxr-dev`, `libsystemd-dev`, `libfmt-dev`,
  `libc6-dev`, `linux-libc-dev`, plus their transitive runtime libraries.

---

## 5. Build & Deploy Script (`scripts/build_mpd_pi.sh`)

Follows the phased safety model of [`scripts/build_camilladsp_pi.sh`](../scripts/build_camilladsp_pi.sh)
and [`scripts/build_bluealsa_pi.sh`](../scripts/build_bluealsa_pi.sh). Default
invocation (`preflight`, `sysroot`, `build`, `verify`) is **100% non-destructive**
and never modifies persistent files on the Pi.

### Phases
1. **`preflight`** — Verify host tools (`meson`, `ninja`, `aarch64-linux-gnu-gcc`,
   `aarch64-linux-gnu-g++`, `aarch64-linux-gnu-objdump`, `pkg-config`, `dpkg-deb`,
   `curl`) and pinned list presence.
2. **`sysroot`** — Fetch, SHA-256-verify, and extract the pinned Debian 13
   Trixie `arm64` `.deb` set (`scripts/sysroot_mpd_trixie_arm64.list`) into
   `${BUILD_DIR}/sysroot`, create the merged-usr `lib -> usr/lib` symlink, and
   strip static-only `Requires.private`/`Libs.private` from `libcurl.pc` so
   `pkg-config` resolves dynamic `libcurl.so.4` (`8.14.1`) cleanly
   (idempotent).
3. **`build`** — Clone `MusicPlayerDaemon/MPD` at `v0.25`, verify `HEAD` commit
   (`e411a30052b1417523f8d9032ac9a69596ed098b`), inject the `__wrap_log10f` →
   `log10f@GLIBC_2.17` trampoline into `src/Main.cxx`, generate a Meson cross
   file for `aarch64-linux-gnu` specifying:
   - `c_args` / `cpp_args`: `-O3 -mcpu=cortex-a53+crc+simd -ftree-vectorize --sysroot=${SYSROOT}`
   - `c_link_args` / `cpp_link_args`: `--sysroot=${SYSROOT} -Wl,--as-needed -Wl,--gc-sections -Wl,-O1 -Wl,--allow-shlib-undefined -Wl,--wrap=log10f -Wl,-rpath-link,${SYSROOT}/usr/lib/aarch64-linux-gnu -Wl,-rpath-link,${SYSROOT}/lib/aarch64-linux-gnu`
   - `pkg_config_libdir`: `${SYSROOT}/usr/lib/aarch64-linux-gnu/pkgconfig:${SYSROOT}/usr/share/pkgconfig`
   and compile `mpd` via `meson setup --wrap-mode=nodownload` + `ninja -C build mpd` (stripped).
4. **`verify`** — ABI gate on `build/mpd`:
   - Confirm ELF `aarch64`.
   - Confirm no `GLIBC_2.42+` or `GLIBCXX_3.4.34+` symbols (`aarch64-linux-gnu-objdump -T`).
   - Confirm `libcurl.so.4` is in `DT_NEEDED` and verify all `DT_NEEDED` shared
     libraries exist on the Pi (or are in the 3-package `apt` set installed by
     `apply`).
5. **`apply` (requires `--i-have-approval`)** —
   - Install **only** missing runtime libraries + `mpc` on the Pi:
     `apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y libavformat61 libfaad2 mpc`
     (explicitly verifying `dpkg -s mpd` is NOT installed).
   - Migrate off `rmpd`:
     - Back up any `.m3u` files from `/home/dietpi/.config/rmpd/playlists/` (if any).
     - Stop/disable `rmpd.service` under `dietpi`, remove
       `/home/dietpi/.config/systemd/user/rmpd.service`, `/usr/local/bin/rmpd`,
       `/home/dietpi/.config/rmpd`, and `/home/dietpi/.local/state/rmpd`.
     - Disable lingering for `dietpi` (`loginctl disable-linger dietpi`) if no
       other user units remain enabled.
   - Stage & smoke-test `/tmp/<mktemp>/mpd --version` (`Music Player Daemon 0.25`)
     and atomically install to `/usr/local/bin/mpd`.
   - Create `mpd` system user (`useradd --system --home-dir /var/lib/mpd --groups audio --shell /usr/sbin/nologin mpd` if absent) and `/var/lib/mpd/playlists` (owned by `mpd:audio`).
   - Install `baseline/etc/mpd.conf` → `/etc/mpd.conf` (mode `0644`) and
     `baseline/etc/systemd/system/mpd.service` → `/etc/systemd/system/mpd.service`.
   - `systemctl daemon-reload && systemctl enable --now mpd.service` (and restart
     if already active).

---

## 6. Configuration & Systemd Unit

### `baseline/etc/mpd.conf`
```text
# Streaming & internet-radio MPD v0.25 configuration for Raspberry Pi 3 Audio Appliance.
# Privileges are dropped by systemd (User=mpd, SupplementaryGroups=audio).
# Audio routes through ALSA `src_rmpd` -> hw:Loopback,0,1 -> CamillaDSP.

playlist_directory  "/var/lib/mpd/playlists"
state_file          "/var/lib/mpd/state"

bind_to_address     "0.0.0.0"
port                "6600"
filesystem_charset  "UTF-8"
audio_buffer_size   "512"

input {
    plugin "curl"
}

input {
    plugin  "ffmpeg"
    enabled "no"
}

audio_output {
    type        "alsa"
    name        "ALSA"
    device      "src_rmpd"
    mixer_type  "none"
    buffer_time "100000"
}
```

*(Note: `zeroconf_enabled` is omitted because `zeroconf=disabled` at build time;
`user` is omitted because systemd sets `User=mpd`; `m3u`, `extm3u`, `pls`,
`asx`, `xspf`, and `rss` playlist plugins are enabled by default when built.)*

### `baseline/etc/systemd/system/mpd.service`
```ini
[Unit]
Description=Music Player Daemon (Cortex-A53 v0.25)
Documentation=https://www.musicpd.org/doc/
After=network-online.target sound.target camilladsp.service
Wants=network-online.target

[Service]
Type=notify
User=mpd
Group=audio
SupplementaryGroups=audio
StateDirectory=mpd
ExecStart=/usr/local/bin/mpd --systemd /etc/mpd.conf
Restart=on-failure
RestartSec=5
TimeoutStopSec=20

# Security hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true

[Install]
WantedBy=multi-user.target
```

---

## 7. Baseline Integration & Files to Create / Update (in `/root/pi-bluetooth-speaker-mpd`)

- [`docs/mpd_plan.md`](mpd_plan.md) — this plan document.
- `scripts/build_mpd_pi.sh` — phased cross-compile, ABI gate, and
  approval-gated deploy script for `MPD v0.25`.
- `scripts/sysroot_mpd_trixie_arm64.list` — pinned Debian 13 Trixie `arm64`
  `.deb` URLs + SHA-256 checksums for the cross-sysroot.
- `baseline/etc/mpd.conf` — streaming/radio-only MPD configuration (`device "src_rmpd"`).
- `baseline/etc/systemd/system/mpd.service` — `Type=notify` unit running `/usr/local/bin/mpd`.
- `baseline/install.sh` — install runtime packages (`libavformat61`, `libfaad2`, `mpc`;
  **never** `mpd`), create `mpd` user & `/var/lib/mpd/playlists`, copy `mpd.conf`
  and `mpd.service`, and enable/start `mpd.service` only `if [ -x /usr/local/bin/mpd ]`.
- `baseline/verify.sh` — gate `mpd.service` active and port `6600` listening
  checks on `if [ -x /usr/local/bin/mpd ]` (matching `install.sh`).
- `baseline/README.md` — document `baseline/etc/mpd.conf` and `mpd.service`.

---

## 8. Verification Checklist

- [x] `bash -n scripts/build_mpd_pi.sh baseline/install.sh baseline/verify.sh` passes.
- [x] `scripts/build_mpd_pi.sh` (`preflight + sysroot + build + verify`) compiles
      `MPD v0.25` for `aarch64` and passes `GLIBC <= 2.41`, `GLIBCXX <= 3.4.33`,
      `libcurl.so.4` dynamic TLS, and `DT_NEEDED` checks.
- [x] `scripts/build_mpd_pi.sh --phase apply --i-have-approval`:
  - `dpkg -s mpd` confirms distro `mpd` is **not** installed (`unknown ok not-installed`).
  - `/usr/local/bin/mpd --version` reports `Music Player Daemon 0.25 (v0.25+)` with
    `alsa`, `curl`, `ffmpeg`, `faad`, `mpg123`, `flac`, `opus`, `vorbis`,
    `soxr`, and `extm3u`/`m3u`/`pls`/`xspf`/`asx`/`rss` plugins present (and no `dbus`
    or `zeroconf`).
  - `mpc version` succeeds (`mpd version: 0.25.0`).
  - `rmpd` user service, `/usr/local/bin/rmpd`, `/home/dietpi/.config/rmpd`,
    `/home/dietpi/.local/state/rmpd`, and `/var/lib/systemd/linger/dietpi` are removed.
  - `systemctl is-active mpd.service` is `active` and `ss -tlpn` shows
    `0.0.0.0:6600`.
  - TCP `192.168.2.120:6600` responds with `OK MPD 0.25.0` from LAN.
  - Playing HTTP and HTTPS radio streams (`bollywood.m3u`: `Hits Of Bollywood`,
    `Bollywood 2010's`, `Radio Udaan`, `Goldy Mukesh`) outputs through
    `src_rmpd` → `hw:Loopback,0,1` (`FLOAT_LE`, `44100 Hz`, `period_size: 882`,
    `buffer_size: 4410` with `buffer_time "100000"`) → `camilladsp.service` →
    `audio-manager` (`rate_watch` locks `hw:Loopback,1,1` + `amp_controller`
    unmutes).
