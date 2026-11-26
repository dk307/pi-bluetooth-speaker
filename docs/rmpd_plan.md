# rmpd (Streaming MPD Service) Build & Integration Plan

> Status: **executed & deployed** (2026-11-26) — cross-compiled, verified, and
> installed on the live Pi (`192.168.2.120`); `rmpd.service` is active under
> `dietpi` with MPD `:6600`, HTTP `:6680`, mDNS `_mpd._tcp`, session MPRIS, and
> ALSA → Loopback → CamillaDSP playback verified.
> Scope: cross-compile and deploy [`M0Rf30/rmpd`](https://github.com/M0Rf30/rmpd)
> (streaming-only MPD server with HTTP JSON-RPC API, Jellyfin, Podcasts, Internet
> Radio, MPRIS, and mDNS) for the Raspberry Pi 3, routing audio through the exact
> same ALSA → Loopback → CamillaDSP pipeline used by `bluealsa-aplay`.
> Follows [`AGENTS.md`](../AGENTS.md) §4 (first-pass quality, scope containment,
> zero-trust version pinning) and the `scripts/build_*_pi.sh` conventions.

---

## 1. Problem Statement

The appliance is a Raspberry Pi 3 Model B (Broadcom BCM2837, quad-core Cortex-A53
`aarch64`, DietPi / Debian 13 Trixie, glibc 2.41) running CamillaDSP as an active
2-way loudspeaker crossover with `audio-manager` owning hardware volume and
dynamic sample-rate switching.

We want to add a lightweight, Rust-native **MPD-compatible streaming daemon**
(`rmpd`) controllable from mobile MPD clients (e.g., **M.A.L.P.**) and HTTP/MPRIS
clients, supporting:

- **Streaming-only sources:** Internet Radio (Radio-Browser / SomaFM + live `.m3u`
  stream playlists), Podcasts, and Jellyfin (no local disk music library).
- **Identical audio path to BlueALSA:** native sample-rate pass-through via ALSA
  `default` → `hw:Loopback,0,0` → CamillaDSP → Merus MA12070P amp.
- **Headless session MPRIS (`org.mpris.MediaPlayer2.rmpd`) & LAN Zeroconf
  (`_mpd._tcp`)** without requiring a desktop login or `avahi-daemon`.
- **Host cross-compilation** tuned for Cortex-A53 (`+neon,-aes,-sha2`), matching
  [`scripts/build_camilladsp_pi.sh`](../scripts/build_camilladsp_pi.sh).

---

## 2. Architecture Decisions

| Concern | Choice | Rationale |
| :--- | :--- | :--- |
| Upstream component | `M0Rf30/rmpd` **`0.19.0`** | Latest tagged release (`08ef16eefca450ee49a57432051bb176a383e21a`), Rust 2024 edition. |
| Cargo features | `http-api,jellyfin,podcast,radio` | Enables HTTP JSON-RPC/WS (`:6680`), Jellyfin catalog/streaming, on-demand podcasts, and Radio-Browser/SomaFM. `mpris` (`mpris-server` / `zbus`) and `mdns` (`mdns-sd`) are built-in on Linux. |
| Build strategy | **Host cross-compile** (`aarch64-unknown-linux-gnu`) | Matches [`scripts/build_camilladsp_pi.sh`](../scripts/build_camilladsp_pi.sh) and [`.cargo/config.toml`](../.cargo/config.toml). Host already has multiarch arm64 `alsa.pc` (1.2.15.3) and `dbus-1.pc` (1.16.2) in `/usr/lib/aarch64-linux-gnu/pkgconfig`; no `cross` or separate sysroot needed. |
| CPU tuning | `-C target-cpu=cortex-a53 -C target-feature=+neon,-aes,-sha2` | Optimizes NEON SIMD audio decoding for Pi 3 BCM2837 while explicitly disabling ARMv8 Crypto extensions omitted by BCM2837 (prevents `SIGILL`). Not compatible with ARMv6 Pis (Pi 1 / Zero W). |
| GLIBC compatibility | ABI gate rejecting `GLIBC_2.42+` (+ `.symver ...@GLIBC_2.17` if needed) | Build host is Ubuntu (glibc **2.43**); Pi 3 is Debian 13 Trixie (glibc **2.41**). Mirrors [`build_camilladsp_pi.sh`](../scripts/build_camilladsp_pi.sh) and [`crates/common/src/glibc.rs`](../crates/common/src/glibc.rs). |
| Audio routing | ALSA `device = "default"` → `hw:Loopback,0,0` → CamillaDSP | **Exact same audio output path as `bluealsa-aplay`** (§3). Preserves active 2-way crossover and dynamic sample-rate switching. |
| Volume / mixer | `mixer_type = "none"` (or `"software"` if M.A.L.P. gain is desired; **never** `"hardware"`) | Matches `bluealsa-aplay --volume=none`. `audio-manager` exclusively owns the Merus `A.Mstr Vol` hardware mixer; hardware mixer access from `rmpd` would cause a two-writer fight. |
| Service model | **Headless `systemd --user`** under `dietpi` (`uid=1000`) with `loginctl enable-linger dietpi` | Required for session D-Bus MPRIS (`org.mpris.MediaPlayer2.rmpd`). `dbus-user-session` is already installed on the Pi; `dietpi` must be added to the `audio` group to open ALSA. |
| Zeroconf / mDNS | Built-in pure-Rust `mdns-sd` (`type = "mdns"`, port `6600`) | Works out of the box without enabling `avahi-daemon` (currently `inactive` on the Pi). |

---

## 3. Audio Output Routing (Same as BlueALSA)

The existing Bluetooth playback chain
([`baseline/etc/systemd/system/bluealsa-aplay.service`](../baseline/etc/systemd/system/bluealsa-aplay.service),
[`baseline/etc/asound.conf`](../baseline/etc/asound.conf),
[`baseline/etc/camilladsp/crossover.yml`](../baseline/etc/camilladsp/crossover.yml)) is:

```text
bluealsa-aplay -S -D default --volume=none --single-audio
        │
        ▼
/etc/asound.conf: pcm.!default (type plug → slave.pcm "hw:Loopback,0,0")
        │   (native sample rate preserved; audio-manager rate_watch reconfigures CamillaDSP)
        ▼
camilladsp.service (capture: hw:Loopback,1,0 → active 2-way crossover & limiters)
        │
        ▼
hw:sndrpihifiberry,0 (S32_LE, Merus MA12070P amp; volume owned by audio-manager)
```

`rmpd` will use the **exact same ALSA output**:

```toml
[[output]]
name = "ALSA"
type = "default"
device = "default"
enabled = true
mixer_type = "none"   # Matches bluealsa-aplay --volume=none; use "software" only if M.A.L.P. software attenuation is wanted
```

### Why This Exact Output Matters
1. **Crossover protection:** Writing directly to `hw:sndrpihifiberry,0` (card 1)
   would bypass `camilladsp.service` and send full-range audio to the subwoofer
   and coaxial satellite without HPF/LPF or limiters. Using `device = "default"`
   guarantees every stream goes through `hw:Loopback,0,0` → CamillaDSP.
2. **Native sample-rate pass-through:** Because `pcm.!default` is `plug` over
   `hw:Loopback,0,0` (no `dmix` resampling),
   [`services/audio-manager/src/rate_watch.rs`](../services/audio-manager/src/rate_watch.rs)
   automatically observes `PCM Slave Active` / `PCM Slave Rate` on `hw:Loopback`
   and live-reconfigures CamillaDSP's `samplerate` and `chunksize` for any stream
   rate (44.1, 48, 88.2, 96, 176.4, or 192 kHz).
3. **Single-owner hardware volume:** `audio-manager` (`org.speaker.AudioManager`)
   is the sole writer to the Merus hardware mixer (`A.Mstr Vol`). Setting
   `mixer_type = "none"` matches `bluealsa-aplay --volume=none` (bit-exact 100%
   pass-through; volume controlled by the rotary encoder / `audio-manager`).
   If software volume control from M.A.L.P. is desired later, `mixer_type = "software"`
   is safe; `mixer_type = "hardware"` must **never** be enabled.
4. **Known constraint (deferred):** `hw:Loopback,0,0` is an exclusive single-writer
   device by design. `bluealsa-aplay` releases it when Bluetooth playback stops,
   and `rmpd` releases it when stopped/paused, so they coexist cleanly one at a
   time. Simultaneous playback from Bluetooth and `rmpd` will return `EBUSY`
   until the active source stops; cross-source arbitration is deferred (§9).

---

## 4. Configuration (`rmpd.toml` for `0.19.0`)

In `rmpd 0.19.0`, settings are sectioned (`[general]`, `[database]`, `[network]`,
`[[output]]`, `[[integration]]`, `[[source]]`).

A sanitized template will be committed at
`baseline/etc/rmpd/rmpd.toml.example` (no real credentials or bearer tokens in
git) and seeded on the Pi at `/home/dietpi/.config/rmpd/rmpd.toml` (mode `0600`,
owned by `dietpi:dietpi`) only if absent:

```toml
[general]
# Omit music_directory for streaming-only mode (no local disk browsing).
playlist_directory = "~/.config/rmpd/playlists"
log_level = "info"

[database]
auto_update = false
filesystem_watch = false

[network]
bind_address = "0.0.0.0"
port = 6600
zeroconf_name = "rmpd@%h"

[[output]]
name = "ALSA"
type = "default"
device = "default"
enabled = true
mixer_type = "none"

[[integration]]
name = "desktop"
type = "mpris"

[[integration]]
name = "zeroconf"
type = "mdns"
port = 6600

[[integration]]
name = "web"
type = "http"
bind = "0.0.0.0:6680"
token = "replace-with-strong-random-token"

[[source]]
name = "radio"
type = "radio"

# Uncomment and configure credentials/feeds on the Pi:
# [[source]]
# name = "jelly"
# type = "jellyfin"
# url = "https://jellyfin.example.com"
# username = "alice"
# password = "s3cr3t"
#
# [[source]]
# name = "podcasts"
# type = "podcast"
# feeds = ["https://example.com/podcast.xml"]
```

### Configuration Side Effects
- Omitting `music_directory` and disabling `[database].auto_update` /
  `filesystem_watch` prevents local disk scanning and avoids watching a
  non-existent `~/Music` directory.
- Defining explicit `[[integration]]` blocks replaces `rmpd`'s implicit
  `media_controls` / `zeroconf_enabled` defaults, so `mpris` and `mdns` are both
  declared explicitly alongside `http`.
- `jellyfin` syncs catalog metadata into local SQLite on startup, whereas
  `podcast` and `radio` query upstream on demand.
- Editing `rmpd.toml` requires restarting `rmpd.service` (interrupting active
  playback); live radio station edits via `.m3u` playlists do **not** require a
  restart (§8).

---

## 5. Headless User Service & Session D-Bus (MPRIS)

Verified live on `root@192.168.2.120`:
- Non-root user: `dietpi` (`uid=1000`, home `/home/dietpi`).
- `dbus-user-session` (`1.16.2-2`) is **already installed**.
- `dietpi` is **not yet** in the `audio` group (`groups=1000(dietpi)`).
- `/var/lib/systemd/linger` does **not yet** exist.

### Adapted User Unit (`baseline/etc/systemd/user/rmpd.service`)
Adapted from upstream `contrib/systemd/rmpd.service`:
- `ExecStart=/usr/local/bin/rmpd` (matches our `/usr/local/bin` deployment
  convention rather than upstream `/usr/bin/rmpd`).
- `After=network-online.target sound.target` (drops desktop PipeWire/PulseAudio
  unit references not present on DietPi).
- Retains `Type=notify`, `ConfigurationDirectory=rmpd`, `StateDirectory=rmpd`,
  `ReadWritePaths=%E/rmpd`, and upstream hardening (`NoNewPrivileges=true`,
  `PrivateTmp=true`, `ProtectSystem=strict`, `ProtectHome=read-only`,
  `ProtectKernelTunables=true`, `ProtectKernelModules=true`,
  `ProtectControlGroups=true`, `RestrictSUIDSGID=true`).

### Enablement Steps (Executed Only in `--phase apply --i-have-approval`)
```bash
usermod -aG audio dietpi
# DietPi masks systemd-logind.service by default; unmask it so user@1000 & linger work.
systemctl unmask systemd-logind.service
systemctl enable --now systemd-logind.service
loginctl enable-linger dietpi
systemctl start user@1000.service
install -Dm644 -o dietpi -g dietpi rmpd.service /home/dietpi/.config/systemd/user/rmpd.service
install -d -o dietpi -g dietpi -m 755 /home/dietpi/.config/rmpd/playlists

sudo -u dietpi \
  XDG_RUNTIME_DIR=/run/user/1000 \
  DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  systemctl --user daemon-reload

sudo -u dietpi \
  XDG_RUNTIME_DIR=/run/user/1000 \
  DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  systemctl --user enable --now rmpd.service
```

### Side Effects
- `mpris` binds strictly to `dietpi`'s session D-Bus
  (`org.mpris.MediaPlayer2.rmpd`). Running `rmpd` as a system service would
  break MPRIS visibility.
- Any SSH command or script interacting with `rmpd`'s user unit or MPRIS bus
  must export `XDG_RUNTIME_DIR=/run/user/1000` and
  `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus` (as user `dietpi`).

---

## 6. Version Pin & Dependencies (Zero-Trust)

Resolved via `git ls-remote https://github.com/M0Rf30/rmpd.git`:

| Component | Tag | Commit SHA |
| :--- | :--- | :--- |
| `rmpd` | `0.19.0` | `08ef16eefca450ee49a57432051bb176a383e21a` |

### Host Build Dependencies (Verified Present on Host)
- `cargo` + `rustup` target `aarch64-unknown-linux-gnu`
- `aarch64-linux-gnu-gcc`, `aarch64-linux-gnu-objdump`
- Multiarch arm64 `alsa.pc` (`1.2.15.3`) in `/usr/lib/aarch64-linux-gnu/pkgconfig`
  (needed by `cpal` / `alsa-sys`)

### Pi Runtime Dependencies (Verified Present on Pi)
- `libasound2t64` (`1.2.14-1+rpt1+deb13u1`)
- `dbus-user-session` (`1.16.2-2`)
- `libdbus-1-3` (`1.16.2-2`)
- `rusqlite` is compiled with `features = ["bundled"]` (statically linked SQLite).
- `mpris-server` (`zbus`) and `mdns-sd` are pure Rust (no `avahi-daemon` needed).

---

## 7. Build & Deploy Script (`scripts/build_rmpd_pi.sh`)

Follows the phased safety model of [`scripts/build_bluealsa_pi.sh`](../scripts/build_bluealsa_pi.sh)
and [`scripts/build_camilladsp_pi.sh`](../scripts/build_camilladsp_pi.sh). Default
invocation (`preflight`, `build`, `verify`) is **100% non-destructive** and never
modifies persistent files on the Pi.

### Phases
1. **`preflight`** — Verify `cargo`, `rustup` `aarch64-unknown-linux-gnu` target,
   `aarch64-linux-gnu-gcc`, `aarch64-linux-gnu-objdump`, and arm64 `alsa.pc`.
2. **`build`** — Clone `M0Rf30/rmpd` at `0.19.0` into `${BUILD_DIR:-/tmp/rmpd-build}`,
   verify `git rev-parse HEAD == 08ef16eefca450ee49a57432051bb176a383e21a`, patch
   `rmpd-player/src/cpal_utils.rs` from `cpal::BufferSize::Default` to
   `cpal::BufferSize::Fixed(2048)` (so `hw:Loopback,0,0` opens with a
   2048-period / 4096-frame buffer above CamillaDSP's 1024-frame `chunksize`),
   patch `rmpd-integrations/src/mdns.rs` with `.enable_addr_auto()` (so
   `mdns-sd` advertises `wlan0`'s IPv4 address for `_mpd._tcp.local` and
   `DietPi.local`), apply GLIBC 2.17 `.symver` shim in `rmpd/src/main.rs` if
   required by libm symbols, and cross-compile:
   ```bash
   CARGO_PROFILE_RELEASE_LTO=fat \
   CARGO_PROFILE_RELEASE_STRIP=true \
   CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
   CFLAGS_aarch64_unknown_linux_gnu="-O3 -mcpu=cortex-a53+crc+simd" \
   PKG_CONFIG_ALLOW_CROSS=1 \
   PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu=/ \
   PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
   RUSTFLAGS="-C target-cpu=cortex-a53 -C target-feature=+neon,-aes,-sha2 -C link-arg=-Wl,--as-needed -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,-O1" \
     cargo build --release --target aarch64-unknown-linux-gnu \
       --features http-api,jellyfin,podcast,radio
   ```
3. **`verify`** — ABI gate on `target/aarch64-unknown-linux-gnu/release/rmpd`:
   - Confirm ELF `aarch64`.
   - Run `aarch64-linux-gnu-objdump -T` and fail if any symbol requires
     `GLIBC_2.42+`.
   - Verify all `DT_NEEDED` shared libraries resolve on the Pi (read-only SSH check).
4. **`apply` (requires `--i-have-approval`)** — Stage binary to `/tmp/<mktemp>/rmpd`,
   verify `rmpd --version` on the Pi, atomically install to `/usr/local/bin/rmpd`,
   ensure `dietpi` is in `audio`, enable lingering, install
   `~/.config/systemd/user/rmpd.service`, seed `~/.config/rmpd/rmpd.toml` if
   missing (mode `0600`), create `~/.config/rmpd/playlists/`, and enable/start
   `rmpd.service` on `dietpi`'s user bus.

---

## 8. Network Stability & M.A.L.P. Runtime Radio Workflow

1. **Wi-Fi Power Management:** Live check on `192.168.2.120` confirmed
   `iw dev wlan0 get power_save` already reports **`Power save: off`**. During
   deployment/verification we will confirm it persists across reboots (and add a
   note/udev rule if needed).
2. **Static IP:** `wlan0` currently holds `192.168.2.120/24` via DHCP (`dynamic`).
   Assign a static DHCP reservation on the router (or configure a static IP in
   DietPi) so M.A.L.P. does not lose the host after lease renewal.
3. **M.A.L.P. Client & Live Radio Management:**
   - Point M.A.L.P. at `192.168.2.120`, port `6600`.
   - Keep `random` and `repeat` disabled (note: keeping `repeat` off stops
     playback cleanly if a remote stream drops instead of auto-retrying).
   - Store direct stream URLs in `/home/dietpi/.config/rmpd/playlists/*.m3u`—
     adding, editing, and switching stations takes effect live with zero daemon
     restarts.

---

## 9. Deferred / Out of Scope

- **Multi-source ALSA arbitration** between `bluealsa-aplay`, `rmpd`, and future
  sources (e.g., `shairport-sync`) — tracked separately from this service setup.
- **`screen-manager` UI integration** (reading `rmpd` status/metadata/cover art
  via MPD `:6600`, HTTP `:6680`, or `dietpi`'s session MPRIS bus) — future PR.

---

## 10. Files to Create & Verification Checklist

### Files Created / Updated (in `/root/pi-bluetooth-speaker-rmpd`)
- [`docs/rmpd_plan.md`](rmpd_plan.md) — this plan document.
- [`scripts/build_rmpd_pi.sh`](../scripts/build_rmpd_pi.sh) — pinned cross-compile, ABI verification, and
  approval-gated deployment script.
- [`baseline/etc/rmpd/rmpd.toml.example`](../baseline/etc/rmpd/rmpd.toml.example) — sanitized streaming-only config
  template matching BlueALSA's ALSA output (`type = "default"`, `device = "default"`,
  `mixer_type = "none"`).
- [`baseline/etc/systemd/user/rmpd.service`](../baseline/etc/systemd/user/rmpd.service) — adapted headless user systemd unit.
- [`baseline/README.md`](../baseline/README.md), [`baseline/install.sh`](../baseline/install.sh), [`baseline/verify.sh`](../baseline/verify.sh) — baseline provisioning and audit integration for `rmpd`.

### Verification Checklist
- [x] `bash -n scripts/build_rmpd_pi.sh` passes cleanly.
- [x] `scripts/build_rmpd_pi.sh` (default `preflight + build + verify`) compiles
      `rmpd 0.19.0` for `aarch64-unknown-linux-gnu` and passes the `GLIBC <= 2.41`
      and `DT_NEEDED` ABI gates.
- [x] `scripts/build_rmpd_pi.sh --phase apply --i-have-approval`:
  - `rmpd --version` (`rmpd 0.19.0`) succeeds on Pi.
  - `systemctl --user is-active rmpd.service` is `active` under `dietpi`.
  - TCP `:6600` responds with `OK MPD 0.24.0` and HTTP `:6680` is listening.
  - `org.mpris.MediaPlayer2.rmpd` is registered on `unix:path=/run/user/1000/bus`.
  - Playing a test radio stream (`Radio Paradise 320k AAC`) outputs through
    `default` → `hw:Loopback,0,0` (`S32_LE`, `44100 Hz`, `period_size: 2048`,
    `buffer_size: 4096`) → `camilladsp.service` (`chunksize=1024`) → Merus amp
    with `audio-manager::rate_watch` locking sample rate and `amp_controller`
    unmuting.
