# Bluetooth Disconnect Investigation — RTL8761BU Firmware Wedge (DEEPAK-LAPTOP ↔ Pi 3)

> Captured 2026-11-22, boot window ~19:18–20:50 UTC. Pi restarted afterwards for a clean start;
> all volatile captures (`/tmp` on the Pi) were lost with that reboot. This file is the durable record.
> (Filename keeps the requested spelling `bluetooh_disconnect_issues`.)

## 1. Symptom

DEEPAK-LAPTOP (MAC `C4:03:A8:BD:EF:74`) streams A2DP audio to the Pi and periodically drops.
Every drop correlates 1:1 with a USB command timeout on the Realtek dongle — the connection loss is a
*symptom* of a dongle firmware wedge, not an RF/link problem.

## 2. Environment

| Item | Value |
|---|---|
| Host | Raspberry Pi 3 Model B, DietPi/Debian, kernel `6.18.39+rpt-rpi-v8` |
| SSH | `root@192.168.2.120` (non-destructive rule applies to all probing) |
| Dongle | TP-Link UB500, `2357:0604`, USB dev `004` behind LAN9514 hub (port `1-1.3`, full-speed, dwc_otg) |
| Firmware | `/lib/firmware/updates/rtl_bt/rtl8761bu_fw.bin`, **md5 `6c3729e7…`, 43,340 B, v0xddd5c761 — byte-identical to latest official TP-Link/Realtek release** |
| btusb | `enable_autosuspend=N`; kernel param `usbcore.autosuspend=2` |
| Bluetooth SW | custom bluetoothd build **5.88** at `/usr/libexec/bluetooth/bluetoothd` (dpkg says bluez 5.82); BlueALSA audio path |
| Network | wlan0 carries traffic; eth0 DOWN |
| Boot | ~19:18:39 UTC; journal volatile (this boot only) |

## 3. Failure signature (refined across crashes #5 and #6 — identical both times)

1. Firmware's **HCI event pipe stalls**: interrupt EP completes with `EPIPE (-32)` or falls silent.
2. Host sends a normal HCI command (e.g. Read Local Version `01 10 00`); USB **setup transfer succeeds**,
   but the firmware **never returns Command Complete**.
3. **Bulk ACL/audio keeps flowing normally** for the full 2 seconds after the firmware goes mute —
   the wedge is internal to the chip, not a bus failure.
4. Kernel hits exactly **2.0 s `command tx timeout`** → `Resetting usb device` → `ESHUTDOWN` burst on
   all URBs → USB reset → firmware reload (`hci0` ↔ `hci1` alternating) → A2DP drops.
5. devcoredump trailing bytes `03 00 00 00` = **CMD_TIMEOUT**.

## 4. Crash log (this boot; 6 total, identical signature)

| # | Wall time (UTC) | dmesg uptime | Context | Time-to-crash after reconnect |
|---|---|---|---|---|
| (3 earlier) | 18:00–18:44 | pre-reboot | idle — **evidence lost** (journal volatile + reboot cleared `/tmp`) | unknown |
| #4 | 20:04:41 | 2751.92 s | idle streaming, udev autosuspend rule ACTIVE | ~21.5 min |
| #5 | 20:36:40 | 4670.29 s | idle streaming | ~31 min |
| #6 | 20:47:29 | 5319.99 s | **during scp load test (1.5 GB)** | **5 min 10 s** |
| #7 | 21:20:34.77 | 1517.71 s (new boot ~20:54:57) | **idle — 11.5 min AFTER 2nd scp copy finished** (copy ran 21:01:17→21:09:08, 1.5 GB, 3.4 MB/s, exit 0; only a 1 s stream restart at 21:06:58 during it) | 24.5 min from connect (20:56:06), 23.2 min from streaming (20:57:20) |
| #8 | 21:47:07.71 | 3110.64 s | **Pixel 10 Pro XL streaming** (connected 21:30:57, streaming since 21:31:18, uninterrupted) | 16.2 min from streaming; 51.8 min from first connect of boot |

## 5. Evidence & analyses

### Crash #5 (idle) — usbmon + btmon packet-level proof
- Read Local Version setup completed in 444 µs (ts 572.20 s) → bulk audio streamed flawlessly for
  2.0 s (n=197, avg 22.2 ms, max 36 ms, **zero errors**) → interrupt EP last event 573.25 s,
  never delivered Command Complete → timeout at 574.20 s (exactly 2.0 s) → ESHUTDOWN burst → re-enumeration.
- Ethernet device `003`: **zero usbmon events the entire capture** (eth0 DOWN) → bus contention ruled out.

### Crash #6 (load test: scp 1.5 GB over wlan0 during streaming)
- scp started 20:43:25 (streaming since 20:42:17). Timeline of final seconds:
  - `20:47:27.18` audio stream hiccup (source released → re-acquired) — first sign of trouble
  - `20:47:27.83` usbmon: event pipe completes **EPIPE (-32)** — firmware's event EP stalls
  - `20:47:27.86` hciconfig Read Local Version sent, USB setup OK, **Command Complete never arrives**
  - `20:47:28.99` last ACL bulk IN completes normally (audio still flowing 1.1 s after firmware went mute)
  - `20:47:29.0` `command tx timeout` → ESHUTDOWN → reset → fw reload as `hci1`
  - `20:47:31+` monitor `connected=0`, `timeouts=3`
- Bulk latency under load: n=1147, avg **41.6 ms**, max **117.9 ms** (2× idle, but 17× away from the
  2 s timeout). Zero pre-wedge errors. Ethernet dev `003` still absent (copy went over wlan0).
- usbmon marker: ESHUTDOWN burst at ts `1223.901` (line 25229); ts + 4096 = dmesg uptime (offset
  verified exactly on #5 and #6).
- At the time: hypothesis that heavy host load **accelerates** the wedge (5 min vs 21.5/31 min idle), N=1.

### Round 2 (post-reboot copy, 21:01–21:20 UTC) — load hypothesis WEAKENED
- Pi restarted for a clean start (boot ≈20:54:57). Laptop connected 20:56:06, streaming 20:57:20.
- 1.5 GB scp `laptop → Pi` ran 21:01:17→21:09:08 (471 s, avg 3.4 MB/s, **exit 0, completed**).
  The entire copy overlapped continuous streaming: only a 1 s stream restart at 21:06:58, **no crash**.
- **Crash #7 hit at 21:20:34.77 — 11.5 min after the copy had already finished (idle traffic only).**
  Same signature: `command tx timeout` (dmesg 1517.71) → USB reset → fw reload as `hci1`.
- Cadence: 24.5 min from connect / 23.2 min from streaming start — matches the idle cadence
  (21.5 / 31 min), NOT the 5 min seen in crash #6.
- **Verdict: load correlation is mixed (N=2).** One crash during load (#6), one full load window
  survived followed by an on-schedule idle crash (#7). The wedge now looks **time-based
  (~25 min cadence), not load-triggered**. A capture-armed control round is needed to confirm.

## 6. Hypotheses — verdicts

| Hypothesis | Verdict |
|---|---|
| Old/buggy firmware → downgrade | **REJECTED by user** ("We are not downgrading, that is dead end.") — Pi already runs byte-identical latest official TP-Link/Realtek 0xddd5c761 |
| USB bus contention starving the dongle | **REJECTED with packet-level evidence** (crashes #5 and #6): bulk URBs completed to the last 12 ms before reset, zero starvation, eth device absent from bus, latency never near timeout |
| USB autosuspend causing wedge | **Mitigation proven ineffective**: surgical udev rule (Option B, not global `usbcore.autosuspend=-1`) active for crash #4 → still crashed (N=1). Rule committed to worktree branch `fix/bt-usb-autosuspend` (2 commits, unpushed): f484e4a (udev rule + install.sh step 8 + README), df16167 (docs §2.2) |
| Firmware-internal wedge | **CONFIRMED root cause**: event pipe stalls → no Command Complete → bulk keeps flowing → 2.0 s timeout → USB reset |

## 7. Instruments used (methodology for next round)

- **usbmon text capture**: `cat /sys/kernel/debug/usb/usbmon/1t > /tmp/usbmon_live.txt`
  (works, includes data hex; `1u` yields 0 bytes). Format: `<urb_addr> <ts_us> <S|C|E> <ep> <status> <len> <data_hex>`.
  Statuses: `-115` = EINPROGRESS (normal submit), `-32` = EPIPE, `-108` = ESHUTDOWN.
  `ts + 4096 s` = dmesg uptime. Losslessness checked via `/sys/kernel/debug/usb/usbmon/0s` (`text_lost` stayed 0).
  **Stop captures by PID kill — `pkill -f usbmon_live.txt` self-matches the SSH command line** (use
  `pkill -f "usbmon_live[.]txt"` or kill PIDs directly).
- **btmon**: `btmon -w /tmp/btmon_live.snoop` + log for HCI command/event timeline.
- **monitor**: 2 s sampling loop → `/tmp/loadtest_monitor.log` (`timeouts=… connected=…`).
- Timeout wrap: `timeout 7200` (2 h) so captures self-expire.

## 8. Cover-art / SDP angle (crash #7, 21:20:34) — unrelated to the wedge

bt-manager's cover-art provider (enabled: `cover_art: true`) polls the connected device's SDP
every **15 s** and pulls BIP cover art on track change. Against DEEPAK-LAPTOP:

| Phase | bt-manager behavior |
|---|---|
| 20:56:07 → 21:20:33 | Every tick fails: `no BIP Image Pull service record` and, on the fallback path, `AVRCP 0x110C SDP query failed → SDP L2CAP connect failed: Connection refused (os error 111)` (166 warnings total, every 16 s) |
| 21:20:34.77 | Crash #7: `command tx timeout` → USB reset → fw reload (`hci0`→`hci1`) |
| 21:31:02 | New source (Pixel 10 Pro XL, `30:E0:44:67:38:A2`): BIP session established in 55 ms, cover art fetched (50 KB JPEG) — Pi-side cover-art pipeline is fine |

**Interpretation:** the laptop does not serve cover art at all — no BIP Image Pull / AVRCP SDP
records, and the L2CAP connection is refused. The refusals are **expected behavior from the
first second of the session**, present identically before, during and after the crash window.
They are **not a wedge indicator** and not linked to the crash: cover-art SDP polling simply
adds ~2 refused SDP transactions / 16 s on the dongle — steady, harmless overhead.
**Verdict: the missing cover art is a device capability gap, unrelated to the disconnects.**
The laptop's SDP server was never reachable by design (not wedging, not failing).

### Laptop-only failure pattern vs Pixel 10 Pro XL — REFUTED by crash #8

The user initially observed disconnects only with the laptop source. **Crash #8 disproved
source-specificity**: the Pixel streamed cleanly for 16.2 min (21:31:18 → 21:47:07) and then
wedged identically (`command tx timeout` → reset → `hci0`, no captures armed). Both sources fail;
the laptop's apparent "only" pattern was small-sample luck (4/4 laptop sessions vs 1 phone session
at the time).

| Dimension | DEEPAK-LAPTOP (C4:03:A8:BD:EF:74) | Pixel 10 Pro XL (30:E0:44:67:38:A2) |
|---|---|---|
| Streaming stability | 4 restarts in first 60 s, then steady 23 min → crash #7 | 0 restarts, steady 16 min → crash #8 |
| BIP cover art | Never served (device lacks BIP Image Pull) | Works (BIP session 55 ms, 50 KB JPEGs/track) |
| SDP probes | ~2 refused L2CAP connects / 16 s | none (BIP established once) |
| Extra SDP setup | — | SDP L2CAP PSM 1 + RFCOMM PSM 25 |
| Service profile breadth | PANU, HFP/HFG, vendor UUID, AVRCP CT/TG | +LE/GATT (0x1801/0x1855/0x185f), MAP, PBAP, SAP, NAP, Google |
| Kernel media input | Creates `input: DEEPAK-LAPTOP (AVRCP)` | — |
| Outcome | Wedged 4/4 sessions (5–31 min) | Wedged 1st session (16.2 min) — **both sources fail** |

**Conclusion: the wedge is source-agnostic** — same signature regardless of device, codec path,
cover-art capability, or SDP behavior. The only consistent variable left is **device uptime
before the wedge** (~16–31 min after (re)connect) and the **wedge-prone RTL8761BU firmware
itself**. Discriminating hypotheses now: firmware timer/counter overflow vs. accumulating
AVDTP/ACL state vs. 2.4 GHz coexistence (laptop+Pi both on WiFi — but the Pixel may have been
on cellular, weakening the coexistence angle further).

## 9. Custom bluetoothd 5.88 (Nov 20 03:15) + BlueALSA v5.0.0 (Nov 21 23:29) — both are custom local builds

Two separate custom installs, both on the wedge timeline:

| Component | Detail |
|---|---|
| **bluetoothd** | `/usr/libexec/bluetooth/bluetoothd` **v5.88**, mtime **Nov 20 03:15**, 8,000,128 B, with **`bluetoothd.bak` (7,998,856 B, same timestamp)** beside it — a locally compiled replacement; the distro package (`bluez 5.82-1.1+rpt2`) remains registered in dpkg, and `/usr/bin/btmon`/`hciconfig` are still the stock 5.82 tools |
| **BlueALSA** | `/usr/local/bin/bluealsad` v5.0.0, installed **Nov 21 23:29** (distro `/usr/bin` copy from Oct 22 intact); services run the local build since every boot (`--all-codecs -c-opus --sbc-quality=xq+`, `-D default --volume=none --single-audio`) |

Timeline correlation:
- Crash bursts: **yesterday evening 18:00–18:44 (3×) and today 20:04–21:47 (5×)**
- bluetoothd 5.88 in place: **Nov 20 03:15** · BlueALSA v5.0.0: **Nov 21 23:29**
- **No crash-free sessions with the custom builds have been observed** — every session this
  boot wedged after 16–31 min (21.5 / 31 / 24.5 / 5-under-load / 16.2 min; mean ≈20 min)
- Crash mechanism is firmware-side (`command tx timeout` → USB reset → `hci0`↔`hci1` swap),
  but *something* must drive the wedge — a regression in the custom bluetoothd 5.88 (mgmt/ACL
  handling) or in BlueALSA (SBC xq+ / AAC / LDAC codec loop, AVDTP state mishandling) is a
  credible driver. Earlier boots (pre-Nov-21) cannot be compared — journal was volatile.
- btmon data from crashes #5/#6 (old build, still wedged) showed clean ACL streaming to the
  last 12 ms before timeout — so the wedge driver is not raw bus starvation; whether the
  *new* stack differs at the wedge instant (AVDTP churn vs clean ACL) is still unmeasured.

**Next step: arm usbmon + btmon for one crash and diff the wedge signature against #5/#6;
if it matches, bisect by temporarily reverting to `bluetoothd.bak` (5.82) and/or the distro
BlueALSA (Oct 22) and repeat the 20–30 min cadence test.**

### Provenance: yes — bluetoothd is BlueZ

`bluetoothd` **is** the BlueZ daemon (canonical repo: <https://gitlab.com/bluez/bluez>, mirrors
on kernel.org / GitHub). BlueZ ships: `bluetoothd` (daemon), `btmon`/`hciconfig`/`btmgmt`
(userspace tools), `obexd` (OBEX), and the `libbluetooth` C library (dpkg `libbluetooth3`). So:

- The custom **v5.88** binary (Nov 20 03:15, 8,000,128 B, MD5 `c892236d22fd8b3b8d4affbc39283084`)
  was built from the BlueZ source tree; the **stock distro version is 5.82** (`bluez 5.82-1.1+rpt2`).
- The custom **BlueALSA v5.0.0** (Nov 21 23:29) is a **separate project** — <https://github.com/arkq/bluez-alsa> —
  it pairs with BlueZ via D-Bus (A2DP endpoints) and talks to the dongle through the same HCI socket.
- The `.bak` (7,998,856 B, MD5 `ca80a9088c2257a95aafab0c8cad8901`) is **also v5.88** — a previous
  local build, not the distro 5.82; a true 5.82 binary was never kept.

### Bisect revertability — old binaries still on the Pi (checked 22:07 UTC)

Distro binaries are NOT uniformly available: the apt `bluez-alsa-utils` 4.3.1-3 and `bluez` 5.82
packages **lack apt-X / aptX-HD / AAC** (and 5.82 lacks the RefreshTrack patch), so the bisect
must swap **one component at a time**, keeping the rest of the private stack live.

**BlueZ side:**
- `bluetoothd.bak` (previous 5.88 build, different MD5) — but **distro 5.82 bluetoothd was
  overwritten in place (Nov 7); no 5.82 copy on disk** → restore via `apt-get install --reinstall bluez`
  (repo has 5.82-1.1+rpt2, candidate identical). This also restores the 5.82 tools:
  `btmon`/`hciconfig`/`bluetoothctl`/`btattach`/`rfcomm`/`sdptool` (currently 5.88, mtime Nov 7).
- `libbluetooth`: custom 5.88 `.so.3.19.17` (Nov 7, in `/usr/lib/aarch64-linux-gnu/`, linked by
  bluealsad) — distro 5.82 `.so.3.19.15` (Aug 10) **still on disk**; `/usr/lib/libbluetooth.a/.so*` (5.88) also present
- `obexd`: **untouched distro**

**BlueALSA side:**
- `/usr/bin/bluealsad` (distro v5.0.0, Oct 22, 345,624 B) + `/usr/bin/bluealsactl` — **pristine**
- `bluealsa-aplay` / `bluealsa-rfcomm`: **no distro copies** — distro package only shipped
  `bluealsad`+`bluealsactl`; restore via `apt-get install --reinstall bluez-alsa-utils=4.3.1-3`
  (`bluez-alsa-utils` currently in `rc` state: only `/etc/alsa/conf.d/20-bluealsa.conf` +
  `/etc/default/bluez-alsa` remain)
- Distro ALSA plugins (`/usr/lib/aarch64-linux-gnu/alsa-lib/libasound_module_{pcm,ctl}_bluealsa.so`,
  Oct 22) — intact; private ones shadow them via symlink (Nov 21) → remove symlink to restore
- Private codecs `libfreeaptx.so.0` (0.2.2) + `libfdk-aac.so.2` (2.0.3) in `/usr/local/lib` — no
  distro equivalent; libsbc 2.1-1 distro intact
- Unit backups under `/usr/local/share/private-bluealsa/backups/<stamp>/` back up the **custom**
  units (post-`/usr/local` state); the original dpkg units come from `apt-get install bluez-alsa-utils`

**Bisect matrix (one component per round; both sides fully reversible):**
1. **BlueALSA revert** — run distro `bluealsad` (4.3.1-3, no aptX/AAC) with private bluetoothd 5.88
   (RefreshTrack remains but only fires on BIP epoch changes — laptop has no BIP → untriggered
   for laptop sessions). Revert step: `apt-get install --reinstall bluez-alsa-utils=4.3.1-3` +
   unit ExecStart back to `/usr/bin` + remove ALSA symlink shadows.
2. **BlueZ revert** — `apt-get install --reinstall bluez` (5.82, no RefreshTrack) with private
   BlueALSA 5.0.0 (aptX/AAC intact).
3. **Restore private stack** — re-run `scripts/build_bluez_pi.sh` (patched 5.88) and/or
   `scripts/build_bluealsa_pi.sh --phase apply --i-have-approval` (private units, symlinks, apt remove).
4. Each round: stream ≥30 min (covers the ~20 min mean cadence), arm usbmon+btmon, compare
   signature vs #5/#6. If BOTH single-component reverts still wedge, the driver is the firmware
   itself or the *combination* — then test stock-for-stock (both reverted) once.

So the wedge-time custom stack is **BlueZ 5.88 (self-built) + BlueALSA v5.0.0 (separate project)** —
two independent upstream projects, both replaced locally on Nov 20/21, both live when the crashes began.

### Build-script audit (scripts/build_bluez_pi.sh, scripts/build_bluealsa_pi.sh — worktree `/root/pi-bt-autosuspend`)

**BlueZ 5.88** (`scripts/build_bluez_pi.sh`):
- Stock kernel.org tarball `bluez-5.88.tar.xz` (SHA-256 `6952c39e…` verified) built on the Pi with
  `--prefix=/usr --libexecdir=/usr/libexec` (so `/usr/libexec/bluetooth/bluetoothd` + `make install`
  overwrote dpkg's 5.82 files in place; `bluetoothd.bak` = previous binary kept per the script)
- **Two source modifications beyond stock 5.88:**
  1. GCC 14 fix: `bt_ad_has_data()` in `src/shared/ad.c` returns `NULL` instead of `false` (C23
     compliance — no-op on the wire)
  2. **RefreshTrack patch** `baseline/patches/bluez-5.88-mediaplayer-refresh-track.patch` —
     adds experimental D-Bus method `org.bluez.MediaPlayer1.RefreshTrack`, which calls
     **`avrcp_get_element_attributes()`** over the AVRCP link (i.e., a *new host→laptop device
     command*, gated behind `Experimental = true` in `/etc/bluetooth/main.conf`, which is set)
- bt-manager calls `RefreshTrack` via `cover_provider.rs` whenever the BIP session epoch changes
  with no image handle (`ensure_bip_session`, ~line 618) — i.e., **at BIP session establishment**,
  which for the Pixel is exactly the 21:31:02 session-establishment moment
- Post-build tree `/root/build/bluez-5.88` (Nov 7 18:43) still on the Pi, patch applied
  (5/2/1 matches in player.c/avrcp.c/player.h), `RefreshTrack` symbol present in the running binary

**BlueALSA v5.0.0** (`scripts/build_bluealsa_pi.sh`, `--phase apply` run):
- Pinned commit `1935d6dcb8975f2d7a51aaafe61538d157224623` (tag v5.0.0), verified on Pi
- Private A53-tuned (`-O3 -mcpu=cortex-a53+crc+simd`) build into `/usr/local` with private
  **libfreeaptx 0.2.2** + **libfdk-aac 2.0.3** (SHA-256-verified pins in `scripts/codecs/versions.lock`)
- Configured `--enable-aptx --enable-aptx-hd --enable-aac --enable-lc3plus --enable-faststream
  --enable-opus --enable-mp3lame --enable-mpg123 --disable-asha --disable-ldac`
- `--phase apply` switched `bluealsa.service`/`bluealsa-aplay.service` ExecStart to
  `/usr/local/bin/*`, installed D-Bus policy + ALSA plugins, `apt-get remove bluez-alsa-utils`
  (rc state = config files remain), unit backups under
  `/usr/local/share/private-bluealsa/backups/<stamp>`

**Implication:** the running stack is *patch-modified* BlueZ (extra AVRCP command at BIP epoch
changes) + *fully rebuilt* BlueALSA with private codecs — both candidates for the ~20 min wedge
driver, alongside the firmware itself. Crash #7/#8 had no AVDTP churn data (no captures), so the
signature diff vs #5/#6 remains the decisive next test.

## 10. BlueZ 5.88 upstream issues (searched Nov 22, 2026)

Checked the BlueZ repo (gitlab.com/bluez/bluez, GitHub mirror API) + Debian BTS + kernel bugzilla
for 5.88-specific and A2DP/timeout issues. **No open/closed issue matches our wedge signature
(clean ACL to the last 12 ms → command tx timeout → USB reset on RTL8761BU)** — 5.88-era
"command tx timeout" reports are either Intel-specific (Debian #1037457: `0x2005`, AX200) or
kernel-driver (bugzilla #215167: `0xfc05`). 5.88 changed **no A2DP/AVDTP defaults** (its fixes:
UUID discovery filter, MAP events, AVRCP ListPlayerAttributes; features: shorter conn interval,
conn subrating, Channel Sounding — none touch the A2DP media path).

Most relevant 5.88 issues found (none reproduce ours, all inform the bisect):

| Issue | Relevance to our wedge |
|---|---|
| **#2786** "5.88 regression: dual-role speaker silent because both A2DP SEPs (remote SNK + SRC) are now configured on connect" (open, Nov 10; Fedora/CachyOS confirmations; comment Nov 16: peer latches aborted SRC SEP, then **rejects next Set Configuration with SEP_IN_USE (0x13) → A2DP dead until power-cycle**) | **Closest A2DP behavioral change in 5.88** — commit `eda4262191ba` (stale AVDTP connecting state) + `066a164` (connect source after sink) now configure BOTH remote SEPs on connect. Only matters for **dual-role** devices (advertise 110a + 110b). Our Pi: laptop advertises 110a only (source), Pixel 110a only → **not applicable as a dual-role regression**, but the same extra-SEP churn is the kind of AVDTP-pattern change that could stress wedge-prone firmware. Repro devices: Actions/Creative/Anker (not dual-role) |
| **#2831** "Degraded A2DP on first connection after power-on: SBC bitpool 12 + QoS token rate 1511" (open, Nov 16) | 5.88 does **two Set Configurations** on first A2DP connection (host closes SEID 1 ~3 s in, re-configures SEID 2, runs at 1/3 bitpool with ~50% RTP loss until reconnect). Explains **early source restarts** pattern; different failure mode than ours (no wedge) |
| **#2742** "Intel AX200: A2DP stream stalls the controller's ACL path, all links drop with Connection Timeout" (open, BlueZ 5.87) | **Closest in spirit**: A2DP streaming stalls the controller ACL path → both links time out → reconnect loop. Intel-specific (AX200 firmware), but same *class* of controller-firmware-under-A2DP-load failure — supports "firmware wedge, userspace triggers" framing |
| #2800 Marshall Motif II (GATT write kills link), #2814 BLE HID report map >512 B broken, #2862 Connect never replies on SDP EHOSTDOWN+LE fallback, #2850 RegisterAdvertisement lost reply, #2694 obexd profile registration order (Debian 5.82) | Unrelated to A2DP streaming path — not applicable |

**Bottom line:** no known 5.88 bug reproduces our signature; the 5.88-era A2DP changes
(dual-SEP connect, double Set Configuration) affect *first connection* behavior, not a
mid-stream wedge at ~20 min. The bisect remains the decisive test.

## 11. Side observations (unresolved)

1. **Amp I2C glitch at crash #6 instant** — `audio-manager` WARN `register 0x36 verification failed:
   wrote 0x84, read back 0x04`, occurred exactly at 20:47:29, once all boot. Coincidence vs. second
   load-sensitive fault — **recurred 2026-11-23 00:40:48** (`wrote 0x8B, read back 0x00` ×2, during
   fault recovery) → **resolved** by the amp fix (`fix/amp-fault-cycles` @ `263f383`, atomic
   `I2C_RDWR` reads + confirm-critical-faults-before-reset), deployed 01:03; no recurrence after.
2. **Unidentified 15 s `hciconfig` poller** (PID-spawned, not in cron/systemd/systemd timers).
3. **Something rebooted the Pi at 19:18:39** — not this assistant; cause unknown, flagged, unanswered.
4. Earlier 3 crashes (18:00–18:44) lost: volatile journal + reboot cleared `/tmp`.
5. **Amp fault cycles coexist with a rock-solid BT session — separate subsystem, NOT the wedge
   (observed 2026-11-23 00:40 UTC).** During the record-length BT session (Pixel 10 Pro XL
   connected 23:34, still `Connected: yes` at 00:40 — **65+ min, far past the 16–31 min wedge
   window, `errors:0`**), the MA12070P amp logged **31 `Critical amp fault` recovery cycles**
   since boot (14 by 00:05, 31 by 00:39), recurring every 1–4 min *while audio kept streaming*
   through every one of them. Per event: `flags=AmpErrorFlags(81)` byte-identical 31/31 times
   (0x51 = FlyingCapOvervoltage + HighTempWarning + PinShortCircuit — simultaneously, every
   time, suspicious), first detected by the **1 Hz safety tick** (all timestamps sub-second
   `.806–.808`), never by the GPIO falling-edge watch at onset → amp self-mute persists up to
   ~1 s before recovery (GPIO mute → `/ENABLE` reset pulse → full register re-init → PLL
   handshake → unmute, ~80 ms software-visible). **This is the mechanism behind the user's
   recurring ~1 s volume dips** — BT/CamillaDSP carry straight through (no ACL drop, underruns
   only at stream transitions), confirming dips ≠ disconnects. Open questions: is the
   always-identical 0x51 a genuine triple fault or a spurious/stale register read, and why the
   GPIO edge fires only *after* recovery. Investigation handed to a separate agent.
   **Outcome: FIXED** — `fix/amp-fault-cycles` @ `263f383` deployed 01:03 (Nov 23), restarted
   01:04:44; **0 critical faults for the remainder of the boot** (31 cycles up to 01:03, 0 after).

## 12. Follow-ups / housekeeping

- [ ] **Custom-stack control round (priority)**: arm usbmon + btmon, stream to first wedge,
      diff the signature against crashes #5/#6 (clean ACL to the last 12 ms = stack not the
      driver; AVDTP churn / repeated renegotiation = prime suspect, see §9).
- [ ] **Bisect plan (see §9 matrix)**: arm usbmon + btmon, one component per round —
      (1) distro BlueALSA 4.3.1-3 (no aptX/AAC) + private 5.88, (2) stock 5.82 + private
      BlueALSA, (3) both stock; restore private stack via `scripts/build_bluez_pi.sh` /
      `build_bluealsa_pi.sh --phase apply --i-have-approval`. Each round ≥30 min stream.
- [ ] Control round: stream ~30 min WITHOUT transfer to firm up (or refute) load correlation —
      now the priority: crash #7 (21:20:34) shows an idle-timed wedge right after a successful
      load window, suggesting a ~25 min time-based cadence.
- [ ] Laptop-vs-phone isolation control: superseded by crash #8 — both sources wedge
      (laptop 5–31 min, Pixel 16.2 min). Remaining question is *timing* (~16–31 min after
      (re)connect), not source identity; next round: arm usbmon/btmon and watch for whether the
      wedge always lands ~20–30 min after (re)connect.
- [ ] Crash #7/#8 evidence = dmesg/journal only (no usbmon/btmon captures were armed by request);
      re-armed captures needed for the next round (see §7).
- [ ] Watch for amp I2C `0x36` error recurrence and crash cadence after the clean restart.
- [ ] **Amp fault cycles (§11.5) — separate agent:** correlate user-noted dip wall-clock times
      against `.807` fault timestamps; verify `flags=81` authenticity by polling live 0x7C /
      latched 0x6D mid-fault; determine why the GPIO falling-edge watch misses fault onset
      (edge→tick detection would shrink the dip from ~1 s to ~80 ms).
- [x] **Visualizer A/B test (§13) — DONE (reclassified):** 4 OFF windows (~4.9 h) = 0 anomalies;
      all 12 observed wedges (#1–#12) occurred with the visualizer active (auto-start was enabled
      during #1–#9). Superseded by the `dwc_otg` FIQ/FSM mitigation in §14.
- [x] **`dwc_otg` 64-bit FIQ/FSM mitigation (§14, Step 0 & Step 1) — DONE:** `coherent_pool=4M` alone
      still wedged (+5 m 20 s); adding `dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0` ran
      ~27.5 min (run 1) and ~58 min (run 2) with 60 fps `radial_fft` ON and **0 anomalies**.
- [ ] **Step 3 — Ethernet (`eth0`) coexistence soak:** run a sustained `eth0` transfer / update
      alongside BT streaming + 60 fps visualizer under `dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0`
      and verify 0 `corrupted ACL packet` / 0 wedges; if clean, persist those two flags to
      `baseline/boot/cmdline.txt` and `baseline/install.sh`.
- [ ] **Crash #9 write-up pending** (intervening session, signature identical to #5/#6) —
      append §4 count/timeline + 3-way comparison when details are re-collected.
- [ ] After every wedge the dongle renumbers `hci0`↔`hci1` — scripts/diagnostics must not
      hardcode `hci0` (resets to `hci0` only at reboot).
- [ ] Leftovers: `/root/loadtest.bin` (1.34 GB partial, Pi) cleared by reboot; local
      `/tmp/opencode/loadtest.bin` (1.5 GB) cleaned up on dev host; `/tmp/loadtest_monitor.*` and
      staged rejected firmware candidates `/tmp/cand_aur.bin`, `/tmp/cand_void.bin` (Pi) cleared by reboot.
- [ ] Worktree `/root/pi-bt-autosuspend` (branch `fix/bt-usb-autosuspend`) still 2 commits ahead, unpushed;
      original tree untouched.
- [ ] Re-arm captures if another diagnostic round is run (see §7).

## 13. Crashes #10–#12 — music visualizer correlation (2026-11-23, boot 23:32:41 Nov 22)

> Numbering: crash #9 occurred in an intervening session (same signature) but was never appended
> to this report — volatile journal lost it. #10–#12 all fall in one boot and are analyzed together
> because a controlled A/B test (visualizer ON vs OFF) ran across it.

### Environment
- Boot **23:32:41** (Nov 22), kernel `6.18.39+rpt-rpi-v8`, TP-Link UB500 (`2357:0604`,
  RTL8761BU), Pixel 10 Pro XL (`30:E0:44:67:38:A2`), aptX-HD 48 kHz.
- `screen-manager` build already contained the merged visualizer (PR #100 / `feat/audio-visualizer`,
  built from `/root/pi-bluetooth-speaker-visualizer/`); config:
  `visualizer_auto_start=false`, `visualizer_force_60fps=true`, `default_visualizer="radial_fft"`.
- Amp fix (`fix/amp-fault-cycles` @ `263f383`) deployed 01:03, restarted 01:04:44 —
  **0 amp faults for the rest of the boot** (see §11.5) — amp is a non-factor here.

### Timeline
| Time (UTC) | Event |
|---|---|
| 23:32:41 | boot |
| 23:34:59 | Pixel connected; aptX-HD streaming — **107 min with ZERO kernel BT anomalies** |
| 01:19:45 | **visualizer manually enabled** — `Subscribed to CamillaDSP Spectrum stream host=127.0.0.1 port=1234`, first frame `bins=128` |
| 01:20:46 | `Bluetooth: hci0: corrupted ACL packet` — **first BT anomaly of the entire boot, +61 s** |
| 01:27:34 | `command tx timeout` → `Resetting usb device` → dwc_otg URB dequeue timeout → USB reset → fw reload as **`hci1`** → A2DP drop (`bluealsad: BT read error: Software caused connection abort`) |
| 01:29:36 | manual `bluetoothctl connect` → connected; playback renegotiated aptX-HD |

- Session length at wedge: **112.6 min** — nearly 4× the historical 16–31 min window (record).
- Anomaly count by window: boot → visualizer = **0** (107 min); visualizer → wedge = **2** (8 min).
- Load average rose **0.38 → 0.98** during the visualizer window (screen-manager ≈9 % CPU,
  60 fps Slint timer + FFT worker at Nice=15).

### Crash #11 (05:11:32 UTC) — visualizer + fresh stream start
| Time (UTC) | Event |
|---|---|
| 04:30:35 | track ended → **40 min idle, connection held, zero anomalies** (visualizer off) |
| 05:10:56 | new track → streaming resumes |
| 05:11:00 | **visualizer enabled (+4 s)** → `Subscribed to CamillaDSP Spectrum` |
| 05:11:32 | `command tx timeout` → `Resetting usb device` → fw reload `hci1` → `hci0` → A2DP drop (**+32 s after t0**) |
| 05:14:31 | manual reconnect → streaming 05:14:49 |

### Crash #12 (06:05:28 UTC) — visualizer on established session
| Time (UTC) | Event |
|---|---|
| 05:14:49 | streaming resumes, **visualizer OFF → 28 min clean** |
| 05:42:53 | **visualizer enabled (t0)** — session already warm (stream up 28 min) |
| 05:48:11 → 05:50:05 | track pause/restart inside the window — connection held, no churn |
| 06:05:28 | `command tx timeout` → `Resetting usb device` → fw reload `hci1` → A2DP drop (**+22 m 35 s after t0**; no corrupted-ACL precursor) |
| 06:06:17 | reconnect (first attempt `br-connection-canceled` during fw reload, second completed) |

### Signature match vs §3
- **Identical:** `command tx timeout` (2 s) → `Resetting usb device` → URB resubmit-failure burst →
  USB reset → firmware reload (`hci0`→`hci1`) → A2DP drop. Same RTL8761BU firmware wedge, unambiguous.
- **NEW element:** `corrupted ACL packet` 6 min 48 s *before* the timeout. Crashes #5/#6 showed bulk
  ACL flowing **clean to the last 12 ms** — a corrupted ACL was never observed before. Data-path
  corruption preceding the command-path stall is a new observable for the signature.

### Controlled A/B results (complete — alternating OFF/ON arms on one boot)

**OFF arms (~4.9 h connection-up, visualizer never rendered):**

| OFF window | Span (UTC) | Duration | Anomalies |
|---|---|---|---|
| Pixel streaming (→ crash #10's session start) | 23:34:59 → 01:19:45 | 104.8 min | **0** |
| laptop streaming | 01:50:11 → 02:24:41 | 34.5 min | **0** |
| new track + idle gap | 03:06:05 → 05:10:56 | 124.8 min | **0** |
| post-#11 reconnect | 05:14:49 → 05:42:53 | 28.1 min | **0** |

**ON arms (visualizer rendering — verified by `Subscribed to CamillaDSP` log + load 1.3–1.6 vs 0.5–0.8 off):**

| ON window | t0 (UTC) | Duration | Anomalies |
|---|---|---|---|
| 1 | 01:19:45 | 7.8 min | corrupted ACL **+61 s** → wedge **+7 m 49 s** (#10) |
| 2 | 02:24:41 | 34.9 min | corrupted ACL **+4 m 55 s** and **+27 m 48 s**, no wedge |
| 3 | 05:11:00 | 0.5 min | wedge **+32 s** (#11) |
| 4 | 05:42:53 | 22.6 min | wedge **+22 m 35 s** (#12), no precursor ACL |

- **4/4 ON windows produced BT anomalies; 0/4 OFF windows did (0 events in ~4.9 h OFF vs 6 events in ~66 min ON).**
- All 3 wedges of this boot (#10, #11, #12) occurred inside an ON window.
- Load signature is unambiguous: **0.47–0.87 OFF → 1.04–1.61 ON** (60 fps Slint timer + FFT worker).

### Visualizer correlation — honest assessment
- Architecture (shares zero code with BT): dedicated WebSocket `SubscribeSpectrum` to CamillaDSP
  `127.0.0.1:1234` (separate from the control connection), single-slot latest-frame drop (no TCP
  backpressure), Nice=15 worker thread, 16 ms (60 fps) Slint UI timer + FFT.
- **For trigger:** 4/4 vs 0/4 windows with clean alternation on a single boot; fastest wedge
  +32 s; CPU/USB load roughly doubles while rendering; `corrupted ACL packet` (a symptom never
  seen in #1–#9) appears only in ON windows and sometimes precedes the wedge.
- **Against root cause (initial assumption — LATER CORRECTED):** the initial assessment assumed
  crashes #4–#9 occurred with no visualizer running. The user later confirmed `visualizer_auto_start`
  was enabled during #1–#9 as well, meaning **12/12 observed wedges occurred with the visualizer
  active**, vs. **0/4 (~4.9 h)** with the visualizer disabled.
- **Revised verdict:** the visualizer is the **sole demonstrated trigger** on this stack — not
  because of shared code, but because its 60 Hz `vc4`/V3D/`glTexImage2D` + FFT workload pushes
  the Pi 3's 64-bit emulated `dwc_otg_sim-fiq` split-transaction path over its latency limit.

## 14. `dwc_otg` 64-bit FIQ/FSM Mitigation & Verification (2026-11-23)

### Root Hardware/Kernel Mechanism
- On Raspberry Pi 3B (`aarch64`, kernel `6.18.39+rpt-rpi-v8`), `dwc_otg` cannot use a true 32-bit
  hardware ARM FIQ and instead emulates it via a maskable IRQ (`dwc_otg_sim-fiq`) + `fiq_fsm`
  (`fiq_fsm_mask=15`) to schedule **125 µs USB 2.0 split transactions** for the Full-Speed (12 Mbps)
  RTL8761BU dongle behind the High-Speed (480 Mbps) LAN9514 hub.
- At 60 fps, `RadialFft` uploads a `228×228` RGBA `SharedPixelBuffer` (**~208 KB/frame**,
  ~12.5 MB/s) via `glTexImage2D` alongside V3D tile rendering, DRM page-flips, and 128-bin FFT
  WebSocket traffic. Even at modest average CPU (`load 1.0–1.9`), IRQ-off / DMA / `schedutil`
  jitter causes `dwc_otg_sim-fiq` to miss 125 µs split-transaction windows, producing
  `corrupted ACL packet` and stalling the RTL8761BU HCI event pipe (`command tx timeout`).
- `dwc_otg.speed=1` (forcing USB 1.1 Full-Speed to bypass split transactions) was **rejected**
  because `eth0` (`smsc95xx`) shares the same LAN9514 hub and would be capped to 12 Mbps (~1 MB/s).

### Controlled Kernel Parameter Tests (60 fps `radial_fft` Visualizer ON, 480 Mbps USB Preserved)

| Step | `/boot/firmware/cmdline.txt` additions | Boot (UTC) | Visualizer window | CPU / IRQ load | Outcome |
|---|---|---|---|---|---|
| **Step 0** | `coherent_pool=4M` *(also added to `baseline/boot/cmdline.txt` & `baseline/install.sh`)* | `06:40:06` | `07:04:27 → 07:09:47` (**5 m 20 s**) | `load 1.31` | **FAILED** — identical `command tx timeout` → USB reset → `hci1` reload at `07:09:47` |
| **Step 1 (Run 1)** | `coherent_pool=4M dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0` | `07:10:53` | `07:13:33 → 07:40:48` (**~27.5 min**) | `75.7% id`, **`0.0% hi`**, **`0.9% si`**, `load 1.57` | **CLEAN** — **0** `corrupted ACL packet`, **0** `command tx timeout`, **0** disconnects |
| **Step 1 (Run 2)** | `coherent_pool=4M dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0` | `23:03:25` | `23:05:55 → 00:03:51` (**~58 min**, `up 1:00`) | `76.2% id`, **`0.0% hi`**, **`0.3% si`**, `load 1.60` | **CLEAN** — **0** `corrupted ACL packet`, **0** `command tx timeout`, **0** disconnects (even across mode/settings changes at `23:58`) |

### Key Takeaways & Next Step
1. **`coherent_pool=4M` alone** did not prevent the wedge (failed in 5 m 20 s).
2. **Disabling `dwc_otg_sim-fiq` + `fiq_fsm` (`dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0`)**
   eliminated both the wedges and `corrupted ACL packet` across **~85+ cumulative visualizer-ON
   minutes** (27.5 min + 58 min) with **<1% IRQ/softirq CPU overhead** (`0.0% hi`, `0.3–0.9% si`)
   while keeping 480 Mbps USB for `eth0`.
3. **Pending (Step 3):** test a sustained `eth0` transfer / system update while BT streaming +
   60 fps visualizer are active. Once verified, persist `dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0`
   into `baseline/boot/cmdline.txt` and `baseline/install.sh`.
