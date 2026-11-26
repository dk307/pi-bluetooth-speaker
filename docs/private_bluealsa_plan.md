# Private BlueALSA Deployment Plan — `/usr/local` Binaries + A53-Tuned Codecs

> Status: **implemented & deployed** (2026-11-21)
> Scope: build **all BlueALSA v5.0.0 binaries and the codec libraries** from
> source with Cortex-A53 tuning, install them entirely under `/usr/local`
> (binaries, libraries, ALSA plugins), remove the apt `bluez-alsa-utils`
> package, and point the existing systemd units at the private binaries.
> Platform libraries (glib/gio/dbus-1/libasound/libbluetooth/libsamplerate/
> libopus) stay on the distro. Private appliance use, **no distribution** →
> licensing is not a blocker.
> Evidence below was measured live on `root@192.168.2.120` (Pi 3, aarch64,
> Debian 13, 1.2 GHz, `throttled=0x0`).
> **Deployment result:** both units `active` running `/usr/local/bin/bluealsad`
> / `bluealsa-aplay`; `bluez-alsa-utils` removed; D-Bus policy and ALSA
> plugin symlinks in place. Remaining step: live per-codec CPU gates (need a
> connected BT device for a stream).

---

## 1. Goals / non-goals

**Goals**

1. **No apt-owned BlueALSA binaries remain.** Remove `bluez-alsa-utils`
   (4.3.1-3) after the private install is in place; `dpkg -l | grep
   bluez-alsa-utils` returns nothing.
2. Every BlueALSA executable runs from `/usr/local/bin`: `bluealsad`,
   `bluealsa-aplay`, `bluealsactl`, `bluealsa-rfcomm`, plus compatibility
   symlinks `bluealsa` → `bluealsad`, `bluealsa-cli` → `bluealsactl`.
   `command -v bluealsad` resolves to `/usr/local/bin/bluealsad` (PATH already
   puts `/usr/local/bin` before `/usr/bin`).
3. All libraries we build — codecs, `libbluealsa*`, ALSA plugins — live under
   `/usr/local/lib`. `ldd` proves `bluealsad` loads our tuned codec `.so`
   files, not the Debian copies.
4. Codec `.so` files keep the **same SONAME** as the distro libraries, so
   A/B testing is a symlink swap + restart (RUNPATH still pins the path
   explicitly).
5. Reproducible builds: pinned upstream versions + SHA-256 / `git ls-remote`
   verification, one script per layer.
6. Measurable CPU reduction per codec, with before/after numbers recorded.

**Non-goals**

- **Platform libraries stay dynamic** (glib/gio/dbus-1/libasound/libbluetooth/
  libsamplerate): gio and ALSA `dlopen` modules at runtime, glib needs NSS,
  and private copies can clash with the system copies already mapped into
  other processes.
- No changes to codec selection in `bluealsa.service` (`-p a2dp-sink
  --all-codecs -c-opus --sbc-quality=xq+` stays).
- Not building `a2dpconf` / `hcitop`: they are conditional `utils/` programs
  (not enabled by the deployed configure line, not used by the appliance).

---

## 2. Codec inventory & decisions

`bluealsad` is built with the deployed configure flags (§5.2) and run with
`-p a2dp-sink --all-codecs -c-opus`
(`bluealsactl status` → `SBC MP3 AAC aptX aptX-HD FastStream LC3plus`).
Instruction/vector counts are from `objdump` on the **installed** libraries.

| Codec | Library (Debian) | Used in a2dp-sink? | SIMD status of distro build | Decision |
|---|---|---|---|---|
| aptX, aptX-HD | libfreeaptx 0.2.2-1 | **yes** (current stream) | none — 1.7 k insns, **0 vector-FP** | **private tuned lib (P0)** |
| AAC | libfdk-aac 2.0.3-1 | yes (iPhone/iPad) | none — 224 k insns, **0 vector-FP**, no NEON sources upstream | **private tuned lib (P0)** |
| SBC, mSBC, FastStream | libsbc 2.1-1 | yes | NEON primitives compiled unconditionally (`sbc_primitives_neon.c`) | optional private lib (P2) — already SIMD |
| MP3 | libmpg123 1.32.10 | yes (rarely negotiated) | hand-written **NEON64 asm** (`*_neon64*.S`, runtime-detected) | leave distro — self-build gains ~nothing |
| LC3plus | libLC3plus (source, `/usr/src/LC3plus`) | yes (rarely negotiated) | already `-O2 -DOPTIM=2 -mcpu=cortex-a53+crc+simd -ftree-vectorize` (confirmed in `CMakeCache.txt`) | keep — already A53-tuned; verify only, no rebuild |
| Opus | libopus 1.5.2 | disabled at runtime (`-c-opus`) but **compiled in** (`--enable-opus`; system lib) | NEON/asm, mature | leave distro — linked as system `libopus.so.0`, not rebuilt |
| MP3 encode | libmp3lame 3.100 | yes (configure `--enable-mp3lame`; sink never encodes) | — | leave distro |
| LC3-SWB / mSBC (HFP) | liblc3 1.1.3 | enabled at configure, HFP/SCO only | — | leave distro |
| CVSD/SCO | libspandsp 3.x | enabled at configure, HFP/SCO only | — | leave distro |
| LDAC | libldacBT | **no** (`--disable-ldac`, TX-only) | — | skip |

**Why aptX + AAC only:** these are the two decoders with *zero* SIMD sources
upstream. Everything else is either already vectorized (mpg123 NEON asm,
libsbc NEON primitives) or not on the a2dp-sink path. Self-building those adds
maintenance surface with no measurable CPU win.

**Licensing note:** libfreeaptx LGPL-2.1, fdk-aac Apache-2.0, libsbc GPL-2+.
Private, non-distributed use means none of these block including libsbc if it
is ever wanted — the decision above is purely performance-driven.

---

## 3. A53/NEON tuning — every binary and `.so`

- aarch64 has **mandatory** NEON/ASIMD; `-mcpu=cortex-a53+crc+simd` implies
  it (`+simd` is a no-op but documents intent). `+crc` is valid on BCM2837.
- **Never add `+aes,+sha2`** — BCM2837 lacks the crypto extensions. On this
  GCC 14.2.0, `-mno-aes` / `-mno-sha2` are *unrecognized options* (verified on
  the Pi); that rule applies to LLVM/Rust only. `-mcpu=cortex-a53+crc+simd`
  alone is the correct C/C++ tuning.
- `-O3 -ftree-vectorize`: vectorization is on at `-O3` anyway; explicit for
  clarity. `-fPIC` is required for shared libs and harmless for executables.
- `-frecord-gcc-switches` embeds the exact compile flags in every object's
  `.GCC.command.line` section, so **each shipped binary and `.so` can be
  verified after the build** (§8). `readelf -A` prints nothing on these
  aarch64 ELFs (no `.ARM.attributes` section) — do not use it as a check.
- Only mpg123/libsbc ship **hand-written** NEON asm; fdk-aac and libfreeaptx
  are plain C/C++, so `-O3` + A53 scheduling is the only lever available.
  Measured for libfreeaptx: **5.1 % → 3.8 % of one core (−26 %)**, output
  bit-identical (FNV hash matched across `-O2`/`-O3`/`-Ofast`).

```sh
# ONE canonical flag set — use for CFLAGS, CXXFLAGS and every codec compile.
TUNE="-O3 -mcpu=cortex-a53+crc+simd -ftree-vectorize -fPIC -frecord-gcc-switches"
```

**Critical:** fdk-aac is written in **C++** (`*.cpp`). Its CMake build must be
tuned with `-DCMAKE_CXX_FLAGS` (and `-DCMAKE_C_FLAGS` for any C sources); a
bare `CFLAGS` is ignored. `CMAKE_BUILD_TYPE=Release` still supplies a default
`-O3`, so an untuned build can look correct while silently lacking
`-mcpu=cortex-a53` — the §8 `.GCC.command.line` check catches it.

---

## 4. Target artifact layout (all private)

```text
/usr/local/bin/                       # private binaries (PATH-preferred)
├── bluealsad                         # tuned codec libs via RUNPATH
├── bluealsa-aplay
├── bluealsactl
├── bluealsa-rfcomm                   # built with --enable-rfcomm
├── bluealsa -> bluealsad             # compatibility symlink
└── bluealsa-cli -> bluealsactl       # compatibility symlink

/usr/local/lib/                       # private libraries (in ld.so.conf)
├── libfreeaptx.so.0.2.2 / .so.0 / .so
├── libfdk-aac.so.2.0.3 / .so.2 / .so
├── libbluealsa.so*                   # from the bluez-alsa build
├── pkgconfig/
└── alsa-lib/
    ├── libasound_module_ctl_bluealsa.so
    └── libasound_module_pcm_bluealsa.so

/etc/dbus-1/system.d/
└── bluealsa.conf                     # our copy (dbus-1 reads /etc first)

/etc/systemd/system/                  # existing CUSTOM units, only ExecStart changes
├── bluealsa.service                  # ExecStart=/usr/local/bin/bluealsad ...
└── bluealsa-aplay.service            # ExecStart=/usr/local/bin/bluealsa-aplay ...

/usr/lib/aarch64-linux-gnu/alsa-lib/  # two SYMLINKS only (no real files)
├── libasound_module_ctl_bluealsa.so -> /usr/local/lib/alsa-lib/...
└── libasound_module_pcm_bluealsa.so -> /usr/local/lib/alsa-lib/...
```

**Why the ALSA symlinks:** libasound's compiled plugin directory is
`/usr/lib/aarch64-linux-gnu/alsa-lib` (confirmed with `strings
libasound.so.2`); it honours `ALSA_PLUGIN_DIR` but setting that for every ALSA
client is impractical. A symlink keeps the *real* plugin files private while
letting all clients resolve `type bluealsa` from `20-bluealsa.conf`.

**D-Bus policy:** dbus-1 does **not** scan
`/usr/local/share/dbus-1/system.d`. Configure writes to
`/usr/local/etc/dbus-1/system.d` (private build phase); the gated `apply`
phase copies the policy into `/etc/dbus-1/system.d` — the final runtime
location (§5, §7). The current checkout at `/usr/src/bluez-alsa` has no
`Makefile`/`config.status` yet; `/usr/bin/bluealsad` was built from this tree
with the flags recorded in `docs/rpi_audio_bluetooth_report.md` §10.6.

Invariant: **`/usr/local/lib` contains only libraries we built** (no distro
glib/gio/dbus/alsa), so RUNPATH cannot accidentally redirect platform libs.

---

## 5. One script drives the whole setup

The earlier compile was done by `baseline/install.sh` §7 (BlueALSA v4.3.1,
untuned, installed into `/usr`); the v5 rebuild was a hand-entered sequence in
`docs/rpi_audio_bluetooth_report.md` §10.6. The repo already has host-side Pi
build drivers — `scripts/build_bluez_pi.sh` and
`scripts/build_camilladsp_pi.sh`. **This time every step is driven by the same
pattern: `scripts/build_bluealsa_pi.sh` in the repo's `scripts/` folder**, no
ad-hoc shell on the Pi. The script is a host-side driver that SSHes to the Pi
and executes each phase there.

**No more two build paths:** `baseline/install.sh` §7 is now **bootstrap-only**
(it just installs the distro `bluez-alsa-utils` so a fresh image has working
audio, exactly as it already does for BlueZ). The authoritative A53-tuned v5
build lives solely in `scripts/build_bluealsa_pi.sh`, run after the baseline
install to replace the bootstrap with the private stack. This removes the
duplication where `install.sh` previously compiled v4.3.1 inline.

### 5.1 `scripts/build_bluealsa_pi.sh`

```sh
scripts/build_bluealsa_pi.sh \
  [--phase codecs|bluealsa|verify|apply] [--all] [--force] [--with=sbc] \
  [--i-have-approval]
```

- **Default** (no flags): `codecs` → `bluealsa` → `verify` — writes only under
  `/usr/local`, `/usr/src/private-bluealsa` and `/tmp` on the Pi; no `/etc`,
  service or apt changes.
- `--phase <name>` runs one phase; phases are idempotent (`--force` rebuilds).
- `--phase apply` performs the destructive rollout steps (§9) and **requires
  `--i-have-approval`**; without it the script exits non-zero before any SSH
  mutation. It backs up units/config, prints an `apt-get -s remove` dry-run,
  then edits units, installs the D-Bus policy/ALSA symlinks, removes the apt
  package, restores `20-bluealsa.conf` and restarts both services.
- All versions/URLs/SHAs are pinned in `scripts/codecs/versions.lock` (`name
  version url sha256`) and verified before use (AGENTS zero-trust rule):
  bluez-alsa `v5.0.0` commit `1935d6dc…` (dereferenced tag, matched to the live
  Pi checkout), freeaptx `0.2.2` and fdk-aac `2.0.3` tarball SHA-256s.
  Verbose build logs are teed to `/tmp/private-bluealsa-logs/` for §8.
- Logs each phase with a `[<phase>]` prefix; no silent skips.
- `bash -n` syntax check passes; no Pi mutation has been performed yet.

### 5.2 Phase `codecs`

- Reads `scripts/codecs/versions.lock` and verifies each tarball's SHA-256
  before extraction (AGENTS zero-trust rule).
- Builds shared libs into `/usr/src/private-bluealsa/codecs/<name>-<version>`
  with the tuning from §3 and installs real file + SONAME/dev symlinks into
  `/usr/local/lib` and headers into `/usr/local/include`.
- Asserts for each lib:
  - `readelf -d <real-file> | grep SONAME` equals the distro SONAME,
  - `ldd /usr/local/bin/bluealsad | grep <lib>` resolves into `/usr/local/lib`,
  - `/usr/local/lib/<lib>` is not the Debian file.
- Default set: `freeaptx`, `fdk-aac`. `sbc` behind `--with=sbc`.
  `mpg123`/`LC3plus` intentionally excluded (§2).

### 5.3 Phase `bluealsa`

Wraps the existing bluez-alsa v5.0.0 build. Effective configure invocation
(exact deployed flags plus the four private-deployment additions):

```sh
PREFIX=/usr/local
# $TUNE from §3 — identical A53 flags for C and C++ compilation.
CFLAGS="$TUNE" CXXFLAGS="$TUNE" \
CPPFLAGS="-I$PREFIX/include" \
LDFLAGS="-L$PREFIX/lib -Wl,-rpath,$PREFIX/lib" \
APTX_CFLAGS="-I$PREFIX/include" APTX_LIBS="-L$PREFIX/lib -lfreeaptx" \
APTX_HD_CFLAGS="-I$PREFIX/include" APTX_HD_LIBS="-L$PREFIX/lib -lfreeaptx" \
AAC_CFLAGS="-I$PREFIX/include" AAC_LIBS="-L$PREFIX/lib -lfdk-aac" \
../configure \
  --prefix=/usr/local \
  --sysconfdir=/etc \
  --with-alsaconfdir=/etc/alsa/conf.d \
  --with-alsaplugindir=/usr/local/lib/alsa-lib \
  --with-dbusconfdir=/usr/local/etc/dbus-1/system.d \
  --enable-systemd \
  --enable-aplay \
  --with-libsamplerate \
  --enable-rfcomm \
  --disable-asha \
  --disable-ldac \
  --enable-lc3plus \
  --enable-faststream \
  --enable-opus \
  --enable-mp3lame \
  --enable-mpg123 \
  --enable-aptx \
  --enable-aptx-hd \
  --with-libfreeaptx \
  --enable-aac \
  --enable-lc3-swb \
  --enable-msbc \
  --enable-midi

make -j"$(nproc)"
make install
ldconfig
```

Notes:

- `--enable-rfcomm` is **required** — the deployed build left it at its
  default `off` (so the old `/usr/bin/bluealsa-rfcomm` was the distro v4.3.1
  binary). This private build enables it and installs `/usr/local/bin/bluealsa-rfcomm`
  (v5.0.0); apt package removal deletes the old distro copy.
- `--with-alsaplugindir` and `--with-dbusconfdir` are new versus the deployed
  build; see §4 for why. The D-Bus policy is installed under `/usr/local` by
  the build phase and copied into `/etc/dbus-1/system.d` only by the gated
  `apply` phase, so default phases never modify `/etc`.
- `CFLAGS`/`CXXFLAGS` are raised from the deployed `-O2` to `$TUNE` (`-O3`
  plus A53 flags + `-frecord-gcc-switches`); the aptX bench proved `-O3` is
  bit-identical and 26 % faster. These variables are consumed by the whole
  automake build, so **all four binaries, `libbluealsa*` and both ALSA plugins**
  get identical A53 tuning — not just the codec-facing daemon.
- Capture the build log: `make V=1 2>&1 | tee /tmp/bluezalsa-build.log`. The
  §8 flag verification greps this plus `.GCC.command.line` in the artifacts.
- Ordinary link — **no archive-order trick, no `-lstdc++`, no `-lm`**, because
  the codec `.so` files declare their own `DT_NEEDED`.
- `--stock-codecs` omits all overrides, `-L` and `-rpath`, building a binary
  that uses the distro libs (for A/B comparison and rollback).

**Binaries produced by this build (v5.0.0, verified from `Makefile.am`):**

| binary | source | codec libs linked? | install path |
|---|---|---|---|
| `bluealsad` | `src/` | **9** (fdk-aac, freeaptx, mpg123, sbc, opus, lc3, LC3plus, lame, spandsp) | `/usr/local/bin` |
| `bluealsa-aplay` | `utils/aplay/` | none (only `libsamplerate`) | `/usr/local/bin` |
| `bluealsactl` | `src/bluealsactl/` | none | `/usr/local/bin` |
| `bluealsa-rfcomm` | `utils/rfcomm/` | none | `/usr/local/bin` |
| `a2dpconf`, `hcitop` | `utils/` | conditional, not enabled | not built |
| `test-codec-*`, `test-utils-*`, `sndalign`, … | `test/` | `check_PROGRAMS`, not installed | not installed |
| `libasound_module_{ctl,pcm}_bluealsa.so` | `src/asound/` | ALSA plugins | `/usr/local/lib/alsa-lib` |

Only `bluealsad` links codec libs; the other three need no `*_LIBS`
overrides. All four still need to be installed privately because apt removal
will delete their `/usr/bin` counterparts.

Compatibility symlinks (created by the script, not by `make install`):

```sh
ln -sfn /usr/local/bin/bluealsad   /usr/local/bin/bluealsa
ln -sfn /usr/local/bin/bluealsactl /usr/local/bin/bluealsa-cli
```

`/root/amp/bt_status.py` calls `bluealsa-cli` via `shutil.which` → resolves
through PATH to `/usr/local/bin/bluealsa-cli`, no edit needed.

---

## 6. Per-codec recipes (exact)

### 6.1 libfreeaptx 0.2.2 — measured win

```sh
tar xf libfreeaptx-0.2.2.tar.gz && cd libfreeaptx-0.2.2
gcc $TUNE -c freeaptx.c
gcc -shared -Wl,-soname,libfreeaptx.so.0 -o libfreeaptx.so.0.2.2 freeaptx.o
ln -sf libfreeaptx.so.0.2.2 libfreeaptx.so.0
ln -sf libfreeaptx.so.0     libfreeaptx.so
install -m 644 freeaptx.h /usr/local/include/
install -m 755 libfreeaptx.so* /usr/local/lib/
```

No `-lm` needed (0 libm symbols). Preserving SONAME `libfreeaptx.so.0` keeps
the binary's `DT_NEEDED` identical to the distro build.

### 6.2 libfdk-aac 2.0.3 — P0

CMake (no autogen). **Tune with `CMAKE_CXX_FLAGS`**, and force the libdir
because Debian's `GNUInstallDirs` may otherwise use `lib/aarch64-linux-gnu`:

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
  -DCMAKE_INSTALL_LIBDIR=lib \
  -DCMAKE_CXX_FLAGS="$TUNE" -DCMAKE_C_FLAGS="$TUNE"
cmake --build build --verbose -j"$(nproc)" 2>&1 | tee /tmp/fdk-build.log
cmake --install build --prefix /usr/local
```

CMake sets `VERSION 2.0.3 / SOVERSION 2` → `libfdk-aac.so.2.0.3` +
`libfdk-aac.so.2` (matches the distro SONAME) and installs public headers to
`/usr/local/include/fdk-aac/`. bluez-alsa includes
`<fdk-aac/aacdecoder_lib.h>` and the Debian `.pc` uses `-I/usr/include`, so
`AAC_CFLAGS="-I/usr/local/include"` is correct.

Autotools fallback: `./autogen.sh && ./configure --prefix=/usr/local
--enable-shared --disable-static CXXFLAGS="..." && make -j && make install`
(`CFLAGS` is ignored for this codebase).

No NEON sources upstream, so `-O3`+A53 is the whole win. Verify
`readelf -d /usr/local/lib/libfdk-aac.so.2 | grep NEEDED` shows `libstdc++`
and `libm` (CMake handles both automatically for the shared library).

### 6.3 libsbc 2.1 — optional (P2)

```sh
./configure --prefix=/usr/local --enable-shared --disable-static CFLAGS="$TUNE"
make -j"$(nproc)" && make install
```

NEON primitives are compiled unconditionally and guarded by `__ARM_NEON`
(active on aarch64). Private use makes its GPL-2+ license a non-issue; the
gain is only A53 scheduling, not SIMD.

### 6.4 Intentionally not built

- **mpg123** — hand-written aarch64 NEON asm, runtime detected.
- **LC3plus** — already built with `-O2 -DOPTIM=2` + A53 flags.
- **lame / opus / spandsp / liblc3** — not on the a2dp-sink path.

---

## 7. Gotchas specific to the private `/usr/local` approach

> **P2 (optional libsbc):** if `--with=sbc` is later wanted, add its
> `name version url sha256` line to `scripts/codecs/versions.lock` — the script
> now refuses `--with=sbc` without a lock entry, so the flag can never be
> silently ignored.

1. **Prefix is the deployment boundary.** `--prefix=/usr/local` (not `/usr`)
   is what keeps `make install` from overwriting apt-owned `/usr/bin` files.
   Keep `--sysconfdir=/etc` explicitly, otherwise it becomes `/usr/local/etc`.
2. **D-Bus policy path** — dbus-1 does not read
   `/usr/local/share/dbus-1/system.d`. Configure writes the policy under
   `/usr/local/etc/dbus-1/system.d`; the gated `apply` phase copies it to
   `/etc/dbus-1/system.d/bluealsa.conf` (backing up any existing file), so the
   service keeps its bus policy when the apt package is removed.
3. **ALSA plugin discovery** — `--with-alsaplugindir=/usr/local/lib/alsa-lib`
   plus the two symlinks in §4; libasound's compiled path is
   `/usr/lib/aarch64-linux-gnu/alsa-lib`.
4. **Systemd units use absolute paths** — both `/etc/systemd/system/*.service`
   must change `ExecStart` to `/usr/local/bin/...`; PATH doesn't apply there.
   This is the only config edit the plan requires (explicit user approval).
5. **CXXFLAGS for fdk-aac** — see §3/§6.2; the single easiest mistake to make.
6. **SONAME must match the distro** — `-Wl,-soname,libfreeaptx.so.0` and
   CMake's `SOVERSION 2`. A different SONAME changes `DT_NEEDED`.
7. **RPATH belongs on the executable** — `-Wl,-rpath,/usr/local/lib`. All
   codec libs are *direct* `DT_NEEDED` entries of `bluealsad`, so its RUNPATH
   is sufficient. `/usr/local/lib` is also in `/etc/ld.so.conf.d/libc.conf`,
   but RUNPATH pins it deterministically (run `ldconfig` after install).
8. **Build-time and runtime paths must agree** — `-L/usr/local/lib` selects
   which `.so` the linker records, RUNPATH selects which one the loader opens.
9. **Header layout** — `freeaptx.h` at `include/` root, FDK headers under
   `include/fdk-aac/`; the two `*_CFLAGS` overrides reflect that.
10. **apt package ownership trap** — `bluez-alsa-utils` owns
    `/etc/alsa/conf.d/20-bluealsa.conf`, which our custom baseline file
    overwrote. `apt remove` **deletes that path**. Restore it from
    `baseline/etc/alsa/20-bluealsa.conf` in the same rollout step (§9).
    The package also owns distro systemd units under
    `/usr/lib/systemd/system/`, `/usr/share/dbus-1/system.d/bluealsa.conf`,
    `a2dpconf`, `hcitop` and man pages — all harmless to lose; our active
    units (in `/etc/systemd/system`) and D-Bus policy (in `/etc/dbus-1`) take
    precedence. The package does **not** own `/usr/bin/bluealsad` (unowned v5
    file), so that path survives until we remove it ourselves.

---

## 8. Verification & acceptance criteria

**Path & linkage checks** (all must pass before removing apt):

```sh
command -v bluealsad bluealsa-aplay bluealsactl bluealsa-rfcomm bluealsa-cli
#   all -> /usr/local/bin/...
ldd /usr/local/bin/bluealsad | grep -E 'freeaptx|fdk-aac'
#   libfreeaptx.so.0 => /usr/local/lib/libfreeaptx.so.0
#   libfdk-aac.so.2  => /usr/local/lib/libfdk-aac.so.2
readelf -d /usr/local/bin/bluealsad | grep -E 'RUNPATH|NEEDED'
readelf -d /usr/local/lib/libfreeaptx.so.0 | grep SONAME
/usr/local/bin/bluealsad -h | grep -i aptx      # codecs still registered
ldconfig -p | grep -E 'freeaptx|fdk-aac'        # private libs in cache
test -L /usr/lib/aarch64-linux-gnu/alsa-lib/libasound_module_pcm_bluealsa.so
```

**Optimization checks — every shipped binary and `.so`:**

```sh
# -frecord-gcc-switches (in $TUNE) records real compile flags in each ELF.
for f in \
  /usr/local/bin/bluealsad /usr/local/bin/bluealsa-aplay \
  /usr/local/bin/bluealsactl /usr/local/bin/bluealsa-rfcomm \
  /usr/local/lib/libfreeaptx.so.0 /usr/local/lib/libfdk-aac.so.2 \
  /usr/local/lib/alsa-lib/libasound_module_pcm_bluealsa.so \
  /usr/local/lib/alsa-lib/libasound_module_ctl_bluealsa.so; do
  readelf -p .GCC.command.line "$f" | grep -q 'cortex-a53' \
    || { echo "NOT A53-TUNED: $f"; exit 1; }
  readelf -p .GCC.command.line "$f" | grep -q -- '-O3' \
    || { echo "NOT -O3: $f"; exit 1; }
done
```

- `readelf -A` is **not** usable here: these aarch64 ELFs have no
  `.ARM.attributes` section (verified on the Pi). `.GCC.command.line` works
  on GCC 14.2.0 (verified with a test object) and is the post-build proof.
- Also grep the captured build logs (`/tmp/bluezalsa-build.log`,
  `/tmp/fdk-build.log`) for `-mcpu=cortex-a53` to catch any source that
  bypassed `$TUNE`.
- LC3plus stays on the distro but is already A53-tuned — confirm from its
  build config: `grep CMAKE_C_FLAGS
  /usr/src/LC3plus/src/floating_point/build/CMakeCache.txt` →
  `-O2 -mcpu=cortex-a53+crc+simd -ftree-vectorize` (verified).

**CPU gates** (per codec, `top -H -p $(pgrep -f /usr/local/bin/bluealsad)`):

| metric | target |
|---|---|
| aptX-HD decode thread | ≤ 3.8 % of one core (baseline 5.1 %) |
| `bluealsad` total, aptX-HD stream | ≤ ~10 % of one core (baseline 11.6–12.3 %) |
| AAC decode thread | record baseline first, then require ≤ −15 % vs distro lib |
| system total (4 cores) | no regression; `screen-manager` (~30 %) is the real target |

**Functional gates:**

- `systemctl status bluealsa bluealsa-aplay` active; `journalctl -u bluealsa`
  free of growing `Missing RTP packets` / `stream out of sync`.
- Stream ≥ 5 min per codec (SBC, AAC, aptX, aptX-HD) with no underruns.
- `bluealsa-cli list-pcms` works (PATH resolves to private CLI);
  `/root/amp/bt_status.py` still reports the connected device.
- `dpkg -l | grep bluez-alsa-utils` empty; no service references `/usr/bin`.

**Regression:** compare against the pre-change numbers in §8; keep the stock
codec `.so` copies in `/usr/local/lib/variants/stock/` for A/B.

---

## 9. Rollout (ordered — apt removal is last)

1. `scripts/build_bluealsa_pi.sh --phase codecs` — verified pins, tuned codec
   `.so` files into `/usr/local/lib`. (A no-flag run does phases 1–3 in order.)
2. `scripts/build_bluealsa_pi.sh --phase bluealsa` — pinned v5.0.0 commit, all
   four binaries + ALSA plugins into `/usr/local`, compatibility symlinks;
   D-Bus policy stays under `/usr/local/etc/dbus-1/system.d` for now.
3. `scripts/build_bluealsa_pi.sh --phase verify` — all §8 path/linkage and
   optimization checks; then run the §8 CPU gates with a live Bluetooth stream. No `/etc`,
   service or apt changes have been made at this point.
4. **Approval gate:** review the printed apt dry-run, then
   `scripts/build_bluealsa_pi.sh --phase apply --i-have-approval`. The script
   backs up units/config to `/usr/local/share/private-bluealsa/backups/<ts>/`,
   prints `apt-get -s remove`, rewrites both `ExecStart` lines to
   `/usr/local/bin`, installs the D-Bus policy + ALSA plugin symlinks, runs
   `ldconfig`, removes `bluez-alsa-utils`, restores
   `/etc/alsa/conf.d/20-bluealsa.conf` from baseline, then `daemon-reload` +
   restarts both services.
5. Re-run `--phase verify` and the §8 functional gates. If any fail, execute
   the rollback below before debugging.
6. **A/B:** keep copies of the distro `.so` files in
   `/usr/local/lib/variants/stock/`. Swapping the real file (or its symlink)
   and restarting `bluealsa` switches codec implementation without relinking.
7. Doc/recipe alignment for a clean install:
   - ✅ `baseline/install.sh` §7 — now bootstrap-only (distro package), private
     v5 built by `scripts/build_bluealsa_pi.sh` (done in this PR).
   - ✅ `docs/rpi_audio_bluetooth_report.md` §10.6 — points at the script as
     authoritative and flags the old `-O2`/`--prefix=/usr`/`--disable-opus`
     divergences as wrong (done in this PR).
   - ✅ Doc `/usr/bin/bluealsa*` path references — all updated to
     `/usr/local/bin` in report §5/§7/§10.6/§10.7/§10.9,
     `volume_sync_plan.md`, `single_active_bt_source.md` and
     `ui_architecture_plan_rust.md` (done in this PR).

**Rollback:** `apt-get install bluez-alsa-utils=4.3.1-3`, restore the two units
and `20-bluealsa.conf` from the timestamped backup directory created by the
`apply` phase, `systemctl daemon-reload && systemctl restart bluealsa
bluealsa-aplay`. Functional but loses aptX-HD (v4.3.1) until the private
install is retried; the private files remain in `/usr/local` for diagnosis.

---

## 10. Maintenance & risk

- **Pinned versions:** fdk-aac 2.0.3, libfreeaptx 0.2.2 (sha256 in
  `scripts/codecs/versions.lock`). Upgrades are a deliberate edit + rebuild.
- **CVE ownership:** apt no longer patches *any* BlueALSA component after
  package removal. Track upstream fixes for bluez-alsa itself plus these two
  libs and rebuild both layers; this is the cost of going private.
- **Runtime file dependency:** deleting/corrupting a private `.so` prevents
  `bluealsad` from starting. `scripts/build_bluealsa_pi.sh --phase verify`
  and the §8 `ldd` check are the guardrails.
- **Single point of truth:** all private artifacts live under `/usr/local`;
  `dpkg` will never touch them. The only files outside that tree are `/etc`
  configs and the two ALSA symlinks (documented in §4).
- **Size cost:** ~67 KB (freeaptx) + ~1.25 MB (fdk-aac) of private `.so` text;
  demand-paged, negligible RAM.
- **Complexity cost:** one driver script
  (`scripts/build_bluealsa_pi.sh`) + one lock file; bluez-alsa stays a single
  `configure` invocation and an ordinary `make`.
- **Platform libs stay dynamic** — if glib/gio/dbus/alsa ever become a
  problem, private copies of those are not the answer (§1).

---

## 11. Decision points

| # | Question | Default |
|---|---|---|
| 1 | Private tuned fdk-aac (AAC)? | **yes** (P0) |
| 2 | Private tuned libsbc (SBC/FastStream)? | no — already NEON (licensing irrelevant for private use) |
| 3 | Private tuned LC3plus? | no — already A53-tuned, rarely negotiated |
| 4 | Remove apt `bluez-alsa-utils`? | **yes** — after private install, per §9 |
| 5 | Build all four binaries privately? | **yes** — including rfcomm (`--enable-rfcomm`) |
| 6 | Edit systemd units to `/usr/local/bin`? | **yes** — required, needs explicit approval |
| 7 | Drop unused codecs at configure time (`--disable-mp3lame`, etc.)? | separate P3 change |

---

## 12. Open items to measure before committing

1. **Tuning-application check (first)** — run the §8 loop over every built
   artifact and confirm `.GCC.command.line` contains both `cortex-a53` and
   `-O3`; grep the captured verbose build logs for `-mcpu=cortex-a53`.
   `readelf -d /usr/local/lib/libfdk-aac.so.2 | grep NEEDED` must show
   `libstdc++`/`libm`; `readelf -d .../libfreeaptx.so.0 | grep SONAME` is
   `libfreeaptx.so.0`. An untuned but correctly-named lib is invisible to
   `ldd` checks.
2. **FDK-AAC baseline** — encode+decode harness (FDK encoder → ADTS →
   `aacDecoder_Fill`/`aacDecoder_DecodeFrame`) measuring distro vs tuned; the
   aptX 26 % gain must not be assumed for AAC.
3. **Per-codec live CPU** — force each codec once (`bluealsactl codec <pcm>
   SBC|AAC|aptX|aptX-HD`) and record `ba-a2dp-*` thread CPU before/after.
4. **apt removal dry run** — the `apply` phase prints
   `apt-get -s remove bluez-alsa-utils` (also saved to
   `/tmp/private-bluealsa-logs/apt-remove-dryrun.log`); confirm the deletion
   list contains only the package-owned paths listed in §7 and that no *other*
   package depends on it (`apt-get -s autoremove` too).
5. **rfcomm smoke test** — after `--enable-rfcomm`, run `/usr/local/bin/bluealsa-rfcomm -h`
   and confirm it starts without the missing-d distro dependencies; the
   appliance does not use HFP, so a `-h` check is sufficient.
6. **ALSA plugin resolution** — with the symlinks in place, run
   `alsactl` / a short `aplay -D bluealsa` probe and confirm
   `strace -e openat` shows `/usr/local/lib/alsa-lib/...` being loaded (not
   a stale distro copy).
7. **Final Pi-optimization sweep** — after all installs, repeat the §8 loop on
   the *installed* files (not the build tree); check all four binaries, both
   codec `.so` files and both ALSA plugins. Any artifact missing
   `cortex-a53`/`-O3` must be rebuilt before apt removal.
