# Raspberry Pi Audio Bluetooth Server & DSP Sink — System & Services Report

**Generated:** October 20, 2026  
**Host IP:** `192.168.2.120`  
**Hostname:** `DietPi`  
**Purpose:** Dedicated Headless Bluetooth Audio Sink with Active DSP Crossover Engine, Hardware I2S Amplifier, Freenove 4.3" DSI Touchscreen, and Custom GPIO Peripherals (Encoder, Fan, Light & Temperature Sensors)

---

## 1. Executive Summary & System Overview

The target machine is a **Raspberry Pi 3 Model B** running **DietPi (Debian 13 Trixie, aarch64)**. It is purpose-built as an audiophile-grade Bluetooth audio receiver, active loudspeaker crossover system, and interactive audio console.

The system features:
- **Audio Output:** High-resolution digital I2S connected directly to an **InnoMaker Merus MA12070P Class-D Amplifier HAT**, actively managed and crossover-filtered via **CamillaDSP**.
- **Custom Device Tree Overlay:** Uses a custom-compiled `merus-amp-custom.dtbo` with stripped `pinctrl` properties to prevent pin-mux conflicts with other GPIO peripherals.
- **Bluetooth Stack:** High-quality external **TP-Link UB500 (Bluetooth 5.0)** USB adapter with onboard Wi-Fi and Bluetooth disabled in hardware/device-tree. Custom-compiled **BlueALSA v5.0.0** optimized for the Raspberry Pi 3's **Cortex-A53** CPU with hardware **ARM NEON SIMD** vectorization (`-O2 -mcpu=cortex-a53+crc+simd -ftree-vectorize`), featuring high-resolution codec support (aptX-HD, aptX, AAC, LC3plus, MP3, SBC-XQ+) and `libsamplerate` adaptive resampling. (Proprietary/transmit-only Sony LDAC removed; experimental non-standard A2DP Opus excluded — see §7.1).
- **Display & Touch:** **Freenove 4.3" MIPI DSI Display (800x480 IPS)** with native capacitive touch powered via the dedicated Raspberry Pi DSI ribbon cable (`raspberrypi-ts` / `/dev/fb0`).
- **Physical Controls & Sensors:** 4-pin PWM cooling fan with tachometer feedback, 1-Wire DS18B20 digital temperature sensor, BH1750 ambient light sensor over I2C, and a TOP-UP EC12 rotary encoder with an integrated RGB LED and push button.
- **Hardware Telemetry Suites:** Python utilities in `/root/amp/` for real-time monitoring of amplifier I2C registers, CamillaDSP DSP load and buffer stats, and Bluetooth connectivity.

### Host Details

| Parameter | Value |
| :--- | :--- |
| **Model** | Raspberry Pi 3 Model B Rev 1.2 |
| **OS** | Debian GNU/Linux 13 (trixie) (DietPi v10.6.2) |
| **Kernel** | `Linux 6.18.39+rpt-rpi-v8 #1 SMP PREEMPT Debian 1:6.18.39-1+rpt1 aarch64` |
| **RAM** | 917 MB total (~90 MB used, ~826 MB available) |
| **Storage** | 29 GB SD card (~2.8 GB used, 25 GB free, 10% utilization) |
| **Primary Network** | `eth0` @ `192.168.2.120` (Fast Ethernet, MAC: `b8:27:eb:4c:60:d1`) |

---

## 2. Hardware & Attached Peripherals

### 2.1. Audio Amplifier HAT: InnoMaker Merus MA12070P
- **Device:** InnoMaker AMP Pro / Infineon MERUS MA12070P High-Efficiency Multi-Level Class-D Amp
- **Audio Interface:** Native I2S (`snd_soc_bcm2835_i2s` + `snd_soc_ma120x0p`)
- **I2C Control Bus:** Dedicated I2C-1 communication (`i2c-dev`, regmap control on address **`0x20`**)
- **Mixer Card:** `sndrpimerusamp` (`card 1`)
- **Hardware Profile:** Left-Justified (LJ) 256x fs timing, Dynamic power mode (PMP1 idle power optimization), integrated hardware limiter enabled (Fast attack, Slow release).
- **Custom Device Tree Overlay (`merus-amp-custom.dtbo`):**
  - **Location:** `/boot/firmware/overlays/merus-amp-custom.dtbo`
  - **Loaded in `config.txt` via:** `dtoverlay=merus-amp-custom`
  - **Why Custom?** The upstream `/boot/firmware/overlays/merus-amp.dtbo` contains default `pinctrl` properties that reserve/lock GPIO pins in the kernel pin-mux subsystem, causing pin conflicts with other hardware peripherals (e.g., fan, encoder, 1-Wire). Stripping `pinctrl` enables clean co-existence of all peripherals.

### 2.2. Bluetooth Adapter
- **Device:** TP-Link UB500 Bluetooth 5.0 Adapter (`USB ID 2357:0604`)
- **Internal Radios:** Onboard Wi-Fi and BT disabled via `/boot/firmware/config.txt` (`dtoverlay=disable-wifi`, `dtoverlay=disable-bt`).
- **Controller MAC:** `20:E1:5D:B2:16:BF`
- **Bluetooth Name / Class:** `DietPi Speaker` / `0x240414` (Audio/Video Loudspeaker).
- **Firmware Override (mandatory):** The upstream Linux firmware `rtl_bt/rtl8761bu_fw.bin` (0xdfc6d922, May 2023) contains a stability regression causing **silent ACL link stalls** — the controller drops the link without emitting a disconnect event, so BlueZ/bluealsad wedge on stale connection state (no audio despite "connected"; `bluetoothctl disconnect` + `connect` temporarily recovers). Fix: install the newer Realtek firmware **0xddd5c761** (BT 5.4, from Windows WHQL driver 1.9.1038.3023) as a kernel-prioritized override:
  ```bash
  # Source: https://github.com/yurinogueira/rtl8761bu-firmware (md5 6c3729e7376259dbc1edc53e35f5e265, 43340 bytes)
  install -D -m 644 rtl8761bu_fw.bin     /lib/firmware/updates/rtl_bt/rtl8761bu_fw.bin
  install -D -m 644 rtl8761bu_config.bin /lib/firmware/updates/rtl_bt/rtl8761bu_config.bin
  modprobe -r btusb && modprobe btusb && systemctl restart bluetooth bluealsa bluealsa-aplay
  # Verify: dmesg shows "fw version 0xddd5c761"; btmgmt info reports version 13 (BT 5.4)
  ```
  Rollback: `rm /lib/firmware/updates/rtl_bt/*.bin && modprobe -r btusb && modprobe btusb`.

### 2.3. Display & Capacitive Touchscreen (Freenove 4.3" DSI)
- **Device:** **Freenove 4.3" MIPI DSI 800x480 IPS Capacitive Touch Display**
- **Display Bus:** Dedicated **MIPI DSI ribbon cable** (does **not** consume SPI or GPIO header pins).
- **Framebuffer:** `/dev/fb0` (800x480, 32-bit RGBA native framebuffer).
- **Touch Interface:** Native Raspberry Pi VideoCore firmware touch driver:
  - Kernel Device: `raspberrypi-ts` (`/dev/input/event0`, `/dev/input/mouse0`).
  - Sysfs Node: `/devices/platform/soc/soc:firmware/soc:firmware:touchscreen/input/input0`.
- **Backlight Control:** Linux sysfs interface at `/sys/class/backlight/rpi_backlight` (255/255).
- **Console Cursor:** Suppressed via `vt.global_cursor_default=0` in `/boot/firmware/cmdline.txt`.
- *Note on Legacy Service:* A legacy `/etc/systemd/system/touchscreen-init.service` and commented-out overlays in `/boot/firmware/config.txt` for an *Elecrow FT6336U* were left over from earlier experiments. As confirmed by `dmesg` (`edt_ft5x06 1-0038: touchscreen probe failed`) and `i2cdetect -y 1`, no Elecrow touch controller is present on the I2C bus; the active touchscreen is solely the Freenove DSI display via `raspberrypi-ts`.

### 2.4. Temperature Sensor (1-Wire)
- **Device:** Dallas / Maxim DS18B20 Digital Thermometer
- **Interface:** 1-Wire bus on **BCM GPIO 26** configured via `dtoverlay=w1-gpio,gpiopin=26,pullup=0`
- **Status:** Verified functional (`t=26437` -> ~26.44 °C / 79.59 °F).

### 2.5. Ambient Light Sensor (I2C)
- **Device:** ROHM BH1750 Ambient Light Sensor (16-bit Illuminance sensor)
- **Interface:** I2C-1 at 7-bit address **`0x23`** (SDA: GPIO 2, SCL: GPIO 3)
- **Operating Mode:** Continuous High-Resolution Mode (`0x10`), 1 lx resolution.

### 2.6. Cooling Fan (4-Pin PWM with Tachometer)
- **Device:** Noctua / Standard 4-Pin PC Cooling Fan
- **PWM Speed Control:** **BCM GPIO 13** (PWM output)
- **Tachometer RPM Sense:** **BCM GPIO 6** (Hardware interrupt on falling edge, external 10 kΩ pull-up to 3.3V, 2 pulses per revolution).

### 2.7. Rotary Encoder with Push Switch & RGB LED
- **Device:** TOP-UP EC12 Rotary Encoder with integrated push button and common-anode RGB LED
- **Quadrature Phase A:** **BCM GPIO 25** (Physical Pin 22, Active-LOW, internal pull-up)
- **Quadrature Phase B:** **BCM GPIO 24** (Physical Pin 18, Active-LOW, internal pull-up)
- **Push Switch:** **BCM GPIO 22** (Physical Pin 15, Active-HIGH to 3.3V, internal pull-down)
- **RGB LED Red:** **BCM GPIO 20** (Physical Pin 38, Active-LOW sink via 330 Ω)
- **RGB LED Green:** **BCM GPIO 16** (Physical Pin 36, Active-LOW sink via 180 Ω)
- **RGB LED Blue:** **BCM GPIO 12** (Physical Pin 32, Active-LOW sink via 180 Ω)

---

## 3. Master 40-Pin GPIO Header Map

Because the custom Merus overlay strips `pinctrl` and the Freenove display uses the separate DSI ribbon connector, all 40 pins are cleanly partitioned without conflicts:

| Physical Pin | Header Label | BCM GPIO | Bus / Protocol | Connected Peripheral / Signal | Electrical Config |
| :---: | :---: | :---: | :---: | :--- | :--- |
| **3** | GPIO 2 | BCM 2 | I2C-1 (SDA) | **BH1750 Light Sensor** & **MA12070P Amp** | 3.3V I2C Data (Hardware pull-up) |
| **5** | GPIO 3 | BCM 3 | I2C-1 (SCL) | **BH1750 Light Sensor** & **MA12070P Amp** | 3.3V I2C Clock (Hardware pull-up) |
| **12** | GPIO 18 | BCM 18 | I2S (BCLK) | **InnoMaker Merus Amp** Bit Clock | Digital Audio I2S |
| **15** | GPIO 22 | BCM 22 | GPIO In | **EC12 Encoder Push Button** | Active-HIGH (to 3.3V), Internal Pull-Down |
| **18** | GPIO 24 | BCM 24 | GPIO In | **EC12 Encoder Phase B** | Active-LOW, Internal Pull-Up |
| **22** | GPIO 25 | BCM 25 | GPIO In | **EC12 Encoder Phase A** | Active-LOW, Internal Pull-Up |
| **31** | GPIO 6 | BCM 6 | GPIO In (IRQ) | **Fan Tachometer Sense** | Falling Edge IRQ, External 10 kΩ pull-up |
| **32** | GPIO 12 | BCM 12 | GPIO Out | **EC12 Encoder LED Blue** | Active-LOW sinking via 180 Ω |
| **33** | GPIO 13 | BCM 13 | GPIO Out (PWM) | **Fan PWM Speed Control** | 100 Hz Software PWM |
| **35** | GPIO 19 | BCM 19 | I2S (LRCLK) | **InnoMaker Merus Amp** Frame Sync | Digital Audio I2S |
| **36** | GPIO 16 | BCM 16 | GPIO Out | **EC12 Encoder LED Green** | Active-LOW sinking via 180 Ω |
| **37** | GPIO 26 | BCM 26 | 1-Wire (w1) | **DS18B20 Temperature Sensor** | 1-Wire Bus via kernel driver |
| **38** | GPIO 20 | BCM 20 | GPIO Out | **EC12 Encoder LED Red** | Active-LOW sinking via 330 Ω |
| **40** | GPIO 21 | BCM 21 | I2S (DOUT) | **InnoMaker Merus Amp** Audio Data | Digital Audio I2S |

---

## 4. End-to-End Audio Signal Flow

```
[Smartphone / Client (e.g. Pixel 10 Pro XL)]
                     │
                     ▼ (Bluetooth A2DP: aptX-HD / LC3plus / AAC / SBC-XQ+)
       [TP-Link UB500 BT Controller]
                     │
                     ▼
         [bluealsa.service (v5.0.0: bluealsad)]
                     │
                     ▼
       [bluealsa-aplay.service]
                     │
                     ▼ (ALSA PCM default -> dsp_dmix -> speexrate_medium)
            [ALSA Loopback (Card 0: hw:Loopback,0,0)]
                     │
                     ▼ (48 kHz / 32-bit LE Capture)
       [camilladsp.service (PID 434, Port 1234)]
          ├── Mono Sum & Split (-6dB gain each)
          ├── Channel 0 (Subwoofer):
          │     ├── Infrasonic HPF (45 Hz, Q=0.707)
          │     ├── Crossover LPF (180 Hz, Q=0.707)
          │     └── Soft Limiter (Clip limit -3.5 dB)
          └── Channel 1 (Coaxial Satellite):
                ├── Crossover HPF (180 Hz, Q=0.707)
                ├── Tweeter High-Shelf (3000 Hz, -2.0 dB)
                └── Soft Limiter (Clip limit -3.0 dB)
                     │
                     ▼ (48 kHz / 32-bit LE Playback)
     [Infineon MERUS MA12070P Amp (Card 1: hw:sndrpimerusamp,0)]
                     │
                     ▼
       [Physical Speakers: Subwoofer + Coaxial Satellite]
```

---

## 5. Services Inventory

### Core Audio & Bluetooth Services

| Service Name | State | Type | Executable & Key Parameters | Description |
| :--- | :--- | :--- | :--- | :--- |
| **`bluetooth.service`** | `active (running)` | System | `/usr/libexec/bluetooth/bluetoothd` | Official BlueZ 5.88 daemon managing HCI controller and A2DP profile. |
| **`bt-manager.service`** | `active (running)` | Appliance | `/usr/local/bin/bt-manager` | Pure-Rust service providing native BlueZ `org.bluez.Agent1` headless auto-pairing & auto-trust agent, BlueZ/BlueALSA status monitoring, and BIP cover-art publisher (replaces legacy `bt-agent.service`). |
| **`bluealsa.service`** | `active (running)` | Custom/System | `/usr/bin/bluealsad -S -p a2dp-sink --all-codecs -c-opus --sbc-quality=xq+` | BlueALSA v5.0.0 daemon acting as an A2DP sink supporting all high-resolution codecs except Opus, with native AVRCP volume. |
| **`bluealsa-aplay.service`**| `active (running)` | Custom/System | `/usr/bin/bluealsa-aplay -S -D default -M hw:sndrpimerusamp --mixer-control="A.Mstr Vol"` | Pipes incoming decoded audio stream from BlueALSA to ALSA `default` (`dsp_dmix` -> `Loopback,0,0` -> CamillaDSP) with hardware-only volume auto-sync to the Merus MA12070P chip (`hw:sndrpimerusamp`). |
| **`camilladsp.service`** | `active (running)` | Custom | `/usr/local/bin/camilladsp -p 1234 -o /var/log/camilladsp/camilladsp.log /etc/camilladsp/crossover.yml` | CamillaDSP v4.1.3 crossover and filter engine with WebSocket control API on port 1234. |

### Infrastructure & System Services

| Service Name | State | Purpose |
| :--- | :--- | :--- |
| **`ssh.service`** | `active (running)` | OpenSSH server listening on port 22 (IPv4 & IPv6). |
| **`cron.service`** | `active (running)` | Standard system cron scheduler. |
| **`dbus.service`** | `active (running)` | System message bus (required for BlueZ, BT agent, and BlueALSA). |
| **`systemd-journald`** | `active (running)` | System logging manager. |
| **`systemd-udevd`** | `active (running)` | Dynamic device node manager. |
| **`systemd-timesyncd`** | `enabled` | NTP network time synchronization. |
| **`fake-hwclock-save.timer`** | `active` | Saves clock state periodically in lieu of a battery RTC. |
| **`dietpi-ramlog.service`** | `enabled` | Minimizes SD card wear by managing RAM-based volatile logging. |
| **`touchscreen-init.service`** | `active (exited)` | *Legacy/Stale:* Attempted to probe an Elecrow FT6336U on I2C address 0x38 (failed; not connected). |

---

## 6. Hardware Sensors, Actuators & Monitoring Scripts

### 6.1. Amplifier & DSP Telemetry Scripts (`/root/amp/`)
1. **`ma12070p_status.py`**:
   - Communicates directly over I2C to address `0x20` to query the Infineon MA12070P hardware registers.
   - Decodes:
     - Power Mode Profile (PMP1 idle power optimization).
     - I2S format and clock sync (Left-Justified 256x fs).
     - Active charge pumps status (Ch0 Active, Ch1 Standby).
     - Live error flags and latched historical events (Flying capacitor overvoltage, overcurrent, PLL clock loss, undervoltage PVDD, high temperature warning).
   - Clear history command: `python3 /root/amp/ma12070p_status.py --clear`.
2. **`camilladsp-status.py`**:
   - Connects to CamillaDSP WebSocket API on `127.0.0.1:1234` via `CamillaClient`.
   - Reports: Processing state (`RUNNING`), CPU processing load (~1.27%), rate adjust ratio, buffer fill percentage (104%), capture peak/RMS dBFS, playback peak/RMS dBFS (Subwoofer and Coaxial satellite), and clipped samples counter (0).
3. **`bt_status.py`**:
   - Queries systemd service states, Bluetooth adapter settings (`bluetoothctl show`), and active paired/connected client details.

### 6.2. DS18B20 1-Wire Temperature Monitor (`/root/read_temp.py`)
- **Protocol:** 1-Wire bus connected to **BCM GPIO 26** (`dtoverlay=w1-gpio,gpiopin=26,pullup=0`).
- **Sysfs Endpoint:** `/sys/bus/w1/devices/28-*/w1_slave`
- **Operation:**
  1. Scans `/sys/bus/w1/devices/28-*` for attached sensors.
  2. Reads raw data lines, verifies CRC checksum (`... YES`).
  3. Extracts the `t=XXXXX` field (temperature in millidegrees Celsius).
  4. Converts to Celsius (`t / 1000.0`) and Fahrenheit (`(temp_c * 9/5) + 32`).
- **Live Test Output:**
  ```text
  Discovered 1 sensor(s): 28-0000000xxxxx
  Temperature: 26.44 °C | 79.59 °F (CRC Valid)
  ```

### 6.3. BH1750 Ambient Light Sensor (`/root/read_light_level.py`)
- **Protocol:** I2C-1 on address **`0x23`** (`smbus2.SMBus(1)`).
- **Operation:**
  1. Sends Power On command `0x01` (`CMD_POWER_ON`).
  2. Selects Continuous High-Resolution Mode `0x10` (`CMD_CONTINUOUS_H_RES`, 1 lx resolution, ~120 ms measurement time).
  3. Reads 2-byte raw block data `[MSB, LSB]`.
  4. Formula: `lux = ((data[0] << 8) | data[1]) / 1.2`.
- **Use Case:** Ambient room lighting detection for automatic display brightness adjustment.

### 6.4. Noctua 4-Pin PWM Fan & Tachometer (`/root/test_fan.py`)
- **Pins:**
  - **PWM Output:** **BCM GPIO 13**
  - **Tachometer Input:** **BCM GPIO 6** (Falling edge interrupt via `GPIO.add_event_detect`)
- **Circuit Characteristics:**
  - Relies on an external **10 kΩ pull-up resistor to 3.3V** on the tachometer line.
  - Software PWM at 100 Hz (`GPIO.PWM(13, 100)`).
- **Measurement Logic:**
  - Steps through duty cycles: `30%`, `50%`, `70%`, and `100%`.
  - Allows 3 seconds for fan inertia stabilization.
  - Counts falling edge pulses over a 5-second interval.
  - Formula: `RPM = (pulses / 2) * (60 / elapsed_seconds)` (2 pulses per revolution).

### 6.5. TOP-UP EC12 Rotary Encoder & RGB LED (`/root/test_all_encoder.py`)
- **Pins:**
  - **Phase A:** **BCM GPIO 25** (`pull_up_down=GPIO.PUD_UP`)
  - **Phase B:** **BCM GPIO 24** (`pull_up_down=GPIO.PUD_UP`)
  - **Push Switch:** **BCM GPIO 22** (`pull_up_down=GPIO.PUD_DOWN`, wired active-HIGH to 3.3V)
  - **Red LED:** **BCM GPIO 20** (via 330 Ω, active-LOW sink)
  - **Green LED:** **BCM GPIO 16** (via 180 Ω, active-LOW sink)
  - **Blue LED:** **BCM GPIO 12** (via 180 Ω, active-LOW sink)
- **Features & Phased Test Flow:**
  - **Phase 1 (RGB Verification):** Automatically sequences through Red, Green, Blue, Yellow, Cyan, Magenta, White, and OFF.
  - **Phase 2 (Interactive Input Tracking):**
    - **Clockwise (CW):** Increments position counter and flashes Green LED.
    - **Counter-Clockwise (CCW):** Decrements position counter and flashes Blue LED.
    - **Push Switch Click:** Detects button down, prints `[CLICK]`, and illuminates solid White while depressed.

### 6.6. Freenove 4.3" DSI Display Diagnostic Tools (`/root/freenove/`)
- **Scripts:**
  - `/root/freenove/display_info.py`: Diagnostic and calibration tool rendering system IP, CPU temperature, and backlight status to `/dev/fb0`.
  - `/root/freenove/display_perf_test.py`: Benchmark measuring FPS and throughput for 800x480 RGB565 / RGBA framebuffer renders.
- **Display Specifications:** 800x480 IPS resolution via DSI bus, native touch supported under `/dev/input/event0` (`raspberrypi-ts`), native backlight control under `/sys/class/backlight/rpi_backlight`.

---

## 7. Audio Routing & CamillaDSP Configuration

### 7.1. BlueALSA Daemon (`bluealsa.service`)
- **File:** `/etc/systemd/system/bluealsa.service`
- **Version:** BlueALSA v5.0.0 (`/usr/bin/bluealsad`)
- **Command Line:**
  ```bash
  /usr/bin/bluealsad -S -p a2dp-sink --all-codecs -c-opus --sbc-quality=xq+
  ```
- **Opus Exclusion (mandatory):** The non-standard A2DP Opus codec (Google vendor ID `0x00E0`, codec ID `0x0001`) MUST be excluded with `-c-opus`. Pixel/Android phones prefer Opus when it is advertised, but their A2DP stack wedges during Opus negotiation (endless SetConfiguration → START → abort probe loop alternating with aptX-HD, zero media packets transmitted, no audio output). With Opus absent from the endpoint list, clients fall through to aptX-HD and stream stably. Do not re-enable Opus.

### 7.2. BlueALSA Playback Forwarder (`bluealsa-aplay.service`)
- **File:** `/etc/systemd/system/bluealsa-aplay.service`
- **Target Device:** `-D default` (routes through ALSA `pcm.!default` -> `dsp_dmix` -> `Loopback,0,0`)
- **Hardware-Only Volume Sync:** `-M hw:sndrpimerusamp --mixer-control="A.Mstr Vol"` explicitly binds `bluealsa-aplay` to the Merus MA12070P hardware amplifier mixer (`Card 1: sndrpimerusamp`) rather than relying on `ctl.!default`. This sets `SoftVolume: false` on the BlueALSA PCM transport so BlueALSA performs **zero software attenuation**, feeding a bit-perfect 100% full-scale digital signal into CamillaDSP while mapping AVRCP volume (`0..127`) exclusively to the MA12070P hardware `'A.Mstr Vol'` register (`0..144`, `-144 dB..0.00 dB`).
- **Command Line:**
  ```bash
  /usr/bin/bluealsa-aplay -S -D default -M hw:sndrpimerusamp --mixer-control="A.Mstr Vol"
  ```
- **Boot Race Mitigation (Critical):** `bluealsa-aplay` MUST NOT attempt to enumerate the PCM list before `bluealsad` has fully registered `org.bluealsa` on the system D-Bus bus. A boot race here (previously observed when starting the service without proper ordering) manifests as `Couldn't get BlueALSA PCM list: The name org.bluealsa was not provided by any .service files`, after which the AVRCP → Merus hardware volume sync is permanently dead for the session (the mixer handle is never opened, so volume changes on the phone are silently ignored while playback continues at whatever hardware volume was last set).
- **Dependency & Resilience Configuration:** To prevent the boot race, the `[Unit]` section MUST pin `bluealsa-aplay.service` to `bluealsa.service` with both ordering and lifecycle dependencies, and the `[Service]` section MUST enable aggressive automatic restarts:
  ```ini
  [Unit]
  After=bluealsa.service sound.target camilladsp.service
  Requires=bluealsa.service
  Wants=bluealsa.service camilladsp.service

  [Service]
  Restart=always
  RestartSec=2
  ```
  - `Requires=bluealsa.service` + `After=bluealsa.service` ensures the daemon is fully stopped before the forwarder (re)starts.

- **Why `After=` alone is insufficient — `Type=dbus` is mandatory:** With `bluealsa.service` configured as `Type=simple`, systemd considers it "started" the moment the process forks — long before `bluealsad` registers `org.bluealsa` on the system bus. The boot race therefore recurs on every cold start or combined stack restart (observed in the journal as `bluealsad: Couldn't get HCI version: Network is down` alongside the aplay PCM-list error). Critically, `bluealsa-aplay` does **not** exit on the failed PCM list: it logs the error and continues running in a degraded state with no PCM watching, so `Restart=always` never fires and volume sync stays dead until a manual restart.
  - **Fix:** `bluealsa.service` MUST use:
    ```ini
    [Service]
    Type=dbus
    BusName=org.bluealsa
    ```
    With `Type=dbus`, systemd defines bluealsa as started only when `org.bluealsa` actually appears on the bus, making the `After=`/`Requires=` ordering in `bluealsa-aplay.service` genuinely effective. This matches the pattern already used by `obex.service` (`Type=dbus`, `BusName=org.bluez.obex`). Other services are safe as-is: `bt-manager` depends on `bluetooth.service`, which is `Type=notify` upstream (readiness-tracked); `camilladsp` has no D-Bus dependency (ALSA-only, with `Restart=always` as retry).

### 7.3. ALSA Routing Configuration (`/etc/asound.conf`)
- **Module:** `snd-aloop` loaded via `/etc/modules-load.d/snd-aloop.conf`
- **Resampler:** `speexrate_medium`
- **Mixer Definition:**
  ```ini
  pcm.dsp_dmix {
      type dmix
      ipc_key 1024
      ipc_perm 0666
      slave {
          pcm "hw:Loopback,0,0"
          format S32_LE
          rate 48000
          channels 2
          period_size 1024
          buffer_size 4096
      }
  }

  pcm.!default {
      type plug
      slave.pcm "dsp_dmix"
  }

  ctl.!default {
      type hw
      card sndrpimerusamp
  }
  ```

### 7.4. CamillaDSP Active Crossover Engine (`camilladsp.service`)
- **File:** `/etc/systemd/system/camilladsp.service`
- **Version:** CamillaDSP 4.1.3 (`/usr/local/bin/camilladsp`)
- **Control Interface:** WebSocket listening on `127.0.0.1:1234`
- **Config Path:** `/etc/camilladsp/crossover.yml`
- **Audio I/O Configuration:**
  - Sample Rate: `48000 Hz`
  - Chunk Size: `1024` samples
  - Rate Adjust: `enable_rate_adjust: true`
  - Capture Device: `hw:Loopback,1,0` (Format `S32_LE`, 2 channels)
  - Playback Device: `hw:sndrpimerusamp,0` (Format `S32_LE`, 2 channels)
- **DSP Filters & Signal Matrix:**
  1. **Mixer `sub_sat_mono_split`**: Sums stereo input to mono with -6.0 dB attenuation on both L & R to preserve headroom, feeding into channels 0 and 1.
  2. **Channel 0 (Subwoofer Out):**
     - Highpass filter (Infrasonic protection): 45 Hz, Q=0.707 (Biquad Butterworth)
     - Lowpass filter (Crossover): 180 Hz, Q=0.707 (Biquad Butterworth)
     - Soft limiter: Clip limit -3.5 dB
  3. **Channel 1 (Coaxial Satellite Out):**
     - Highpass filter (Crossover): 180 Hz, Q=0.707 (Biquad Butterworth)
     - High-shelf filter (Tweeter Pad): 3000 Hz, Gain -2.0 dB, Q=0.707
     - Soft limiter: Clip limit -3.0 dB

### 7.5. Hardware Amplifier Mixer Controls (`amixer -c sndrpimerusamp`)
- **Master Volume (`A.Mstr Vol`):** 100% (`0.00dB`, limit 144)
- **Left Volume (`B.L Vol`):** 100% (`0.00dB`, limit 50)
- **Right Volume (`C.R Vol`):** 100% (`0.00dB`, limit 50)
- **Hardware Limiter (`F.Limiter Enable`):** Enabled
- **Limiter Attack (`G.Limiter Attck`):** Fast
- **Limiter Release (`H.Limiter Rls`):** Slow
- **Power Management (`R.Power Mode` / `Q.PM Prof`):** Dynamic power mode / PMF0

### 7.6. AVRCP Absolute Volume Synchronization & Calibration

#### 7.6.1. Volume Sync Architecture (BlueZ ↔ bluealsa-aplay ↔ Merus)
Volume control is a **three-stage chain**, and a failure at any stage freezes the physical amplifier:

1. **BlueZ (bluetoothd) — AVRCP TG controller:** Exclusively owns the AVCTP/AVRCP connection (PSM 0x0017) and the SDP records. It receives `SetAbsoluteVolume` (PDU 0x50) commands from the phone (CT, Category 2) and exposes the current volume as `org.bluez.MediaTransport1.Volume` (0..127) on each active transport (`/org/bluez/hci0/dev_<mac>/fdN`). bluez-alsa is only a D-Bus client here — it never touches AVCTP itself.
2. **bluealsad / bluez-alsa 5.0.0:** Maps the D-Bus `Volume` property into its internal PCM volume model and signals changes to `bluealsa-aplay`.
3. **bluealsa-aplay mixer binding:** With `-M hw:sndrpimerusamp --mixer-control="A.Mstr Vol"` (`SoftVolume: false`), every transport volume event is translated directly into the Merus MA12070P hardware register `A.Mstr Vol` (0..144 raw steps, -144 dB..0 dB), leaving the digital path to CamillaDSP bit-perfect.

**Critical BlueZ gate (Rank 0 root cause — BlueZ 5.88 `[AVRCP] VolumeCategory`, fixed 2026-11-09):** BlueZ 5.88 (deployed 2026-11-07) introduced commit `8baabd2f5` ("audio/avrcp: Only allow absolute volume call/event on category-2 peers") with a **new `main.conf` default `[AVRCP] VolumeCategory = true`**. With the default, `bluetoothd`'s AVRCP target init refuses to register the `EVENT_VOLUME_CHANGED` notification (AVRCP event 0x0D) unless the phone's own AVRCP Target SDP record advertises the **category-2** feature bit. The Pixel's TG record (AVRCP **1.4**, no category-2) fails the check, so the phone never learns the sink supports absolute volume, never sends `SetAbsoluteVolume` (PDU 0x50), and `MediaTransport1.Volume` stays frozen while BlueALSA/Merus downstream sit idle. Symptom fingerprint (observed live 2026-11-09): BlueZ transport Volume, BlueALSA PCM `Volume`, and the Merus `A.Mstr Vol` mixer all frozen at stale values during volume-key presses; **no** `SetAbsoluteVolume not supported` error in the journal (the phone silently never sends after the notification registration is skipped), and `btmon` shows only encrypted ACL traffic. Fix: set `[AVRCP] VolumeCategory = false` in `/etc/bluetooth/main.conf` and restart Bluetooth. Verified live after the fix: the phone's PDU 0x50 is accepted (`bluetoothd -d` shows `handle_vendordep_pdu() AVRCP PDU 0x50`), the transport `Volume` follows every key press, and `bluealsa-aplay` maps it onto the Merus mixer through the amp's dB curve. Note the gate requires the *remote's* TG SDP record version ≥ 1.4 **in addition to** category-2 when `VolumeCategory = true` (`avrcp_volume_supported()` in `profiles/audio/avrcp.c`).

**Critical BlueZ gate (Rank 1 root cause of "volume stuck at 0"):** `bluetoothd`'s AVRCP target init only registers the Volume-Changed notification (AVRCP event 0x0D) with the phone if `btd_device_get_volume()` returns a valid value. If the device info file `/var/lib/bluetooth/<adapter_mac>/<device_mac>/info` has **no `Volume=` entry** under `[General]`, this returns -1 and the phone is never told the sink supports absolute volume — the phone then never sends volume updates, and the transport volume stays frozen (observed: stuck at 10/127 ≈ 8% while the Merus mixer stayed at a stale 107/144). Seeding `Volume=64` once in the info file bootstrap-resolves this; afterwards BlueZ persists the phone's last volume automatically on every change.

**Critical BlueZ gate (Rank 2 root cause — ControllerMode collision):** On a `ControllerMode = dual` adapter, the BLE Media Control (MCP) profile collides with Classic AVRCP on modern Android (Pixel) devices, and the AVRCP TG feature init is skipped. The transport then exposes **no `SetVolume` method** and a read-only `Volume` property. Fix: set `ControllerMode = bredr` in `/etc/bluetooth/main.conf` and re-pair. The device `Class` must also be a loudspeaker CoD (`0x240414`), not a generic audio class. Note that starting with BlueZ 5.88, BlueZ also introduces automatic 1Hz `GetPlayStatus` polling (PR #2570) when remote targets lack `AVRCP_EVENT_PLAYBACK_POS_CHANGED`, along with inbound ACL connection race fixes (PR #2320/#2308) and deterministic A2DP codec prioritization (PR #2189).

#### 7.6.2. Applied Configuration Fixes (2026-11-03 / 2026-11-07 / 2026-11-09)
Persistent changes deployed to the Pi (previous values recoverable from backups):

1. `/etc/bluetooth/main.conf` (backup: `/tmp/main.conf.bak`) — full deployed content:
   ```ini
   [General]
   FastConnectable = true      ; added 2026-11-09 (faster phone-initiated
                               ; connects; power tradeoff irrelevant, mains)
   Experimental = true
   Name = DietPi Speaker
   Class = 0x240414            ; loudspeaker CoD (was 0x240418)
   DiscoverableTimeout = 0
   PairableTimeout = 0
   ControllerMode = bredr      ; added 2026-11-03 (was unset -> dual)

   [Policy]
   AutoEnable = true

   [AVRCP]
   VolumeCategory = false      ; added 2026-11-09 (BlueZ 5.88 default true;
                               ; rejects Pixel TG without category-2 -> dead
                               ; absolute volume, see 7.6.1 Rank 0 gate)
   ```
   The same content is committed as `baseline/etc/bluetooth/main.conf` so reinstalls include the fix. Verified live via `btmgmt info` (`fast-connectable` in current settings); note it is a kernel MGMT page-scan setting, not an `Adapter1` D-Bus property.
2. `/var/lib/bluetooth/20:E1:5D:B2:16:BF/30:E0:44:67:38:A2/info` (BlueZ per-device store; backup: `/tmp/info.bak`) — the `Volume=64` seed under `[General]` is the applied change (see 7.6.1). Relevant sections of the deployed file:
   ```ini
   [LinkKey]
   Key=C5F3E0CC822AD31CD20B08BA98FC5171
   Type=4
   PINLength=0

   [General]
   Name=Pixel 10 Pro XL
   Class=0x5a420c
   SupportedTechnologies=BR/EDR;
   Trusted=true
   Volume=64                    ; one-time seed (BlueZ rewrites this automatically afterwards)
   Blocked=false
   Services=00001105-...;0000110a-...;0000110c-...;0000110d-...;...  ; A2DP + AVRCP UUIDs (110c/110d/110e)
   ```
   Note: the `Key=` link key is per-pairing (changes on re-pair) — the documented constant is the `Volume=64` line's presence under `[General]`.

After changing BlueZ configuration, a **full re-pair** is required for the changes to take effect (both stacks cache SDP records and volume capability flags) — except for `[AVRCP] VolumeCategory`, which takes effect on a plain `systemctl restart bluetooth` (the gate is evaluated per-connection in `target_init()`; verified live 2026-11-09 without re-pairing):
```bash
bluetoothctl remove 30:E0:44:67:38:A2   # on the Pi (then "Forget" the device on the Pixel, re-pair)
```

#### 7.6.3. Bluetooth Controller Configuration (`/etc/bluetooth/main.conf`)
- **File:** `/etc/bluetooth/main.conf` (BlueZ 5.88 daemon config)
- **Purpose:** Defines the Classic/BLE mode split and device class, both critical for AVRCP absolute volume (see 7.6.1) and correct device-type advertisement to phones.
- **Full deployed content:**
  ```ini
  [General]
  Experimental = true        # required for bluez-alsa battery/codec extras and player features
  Name = DietPi Speaker
  Class = 0x240414           # Bluetooth loudspeaker CoD (Major: Audio/Video, Minor: Loudspeaker)
  DiscoverableTimeout = 0    # permanently discoverable
  PairableTimeout = 0        # permanently pairable
  ControllerMode = bredr     # Classic BT only — excludes BLE MCP/CCP profile collisions on Pixel phones

  [Policy]
  AutoEnable = true          # power adapter on automatically at boot
  ```
- **Class code breakdown (`0x240414`):** Major Device Class = Audio/Video, Minor Device Class = Loudspeaker, Service Classes = Rendering. Phones use this to present the sink as a speaker and to select appropriate volume UX. The previous value `0x240418` mapped to Minor = Headphones.
- **ControllerMode implications:** The default is `dual` (Classic + BLE). On dual mode, BLE Media Control Profile (MCP) server registration can collide with Classic AVRCP target init on Android 15+ phones, causing `MediaTransport1.SetVolume` to be missing and the transport `Volume` property to be read-only (transport stuck; volume sync dead). `bredr` removes the collision by disabling all BLE GATT server profiles on this controller.

#### 7.6.4. Verified Volume Mapping (Transport → Merus Hardware Mixer)
Measured calibration after the fixes (transport `MediaTransport1.Volume` set via `busctl set-property ... org.bluez.MediaTransport1 Volume q <v>`; mixer read via `amixer -c sndrpimerusamp sget 'A.Mstr Vol'`):

| Transport (AVRCP) | Mixer raw (`A.Mstr Vol`) | Attenuation | Note |
|---|---|---|---|
| 0 | 0 | −144 dB | hard mute |
| 32 (25%) | 124 | −20 dB | bottom of working range |
| 64 (50%) | 134 | −10 dB | ≈ −10 dB per 32 steps |
| 96 (75%) | 140 | −4 dB | ≈ −5 dB per 32 steps |
| 127 (100%) | 144 | 0 dB | full output |

The mapping is **perceptually curved, not linear**: bluez-alsa dB-maps AVRCP volume into the mixer's declared range so phone volume steps sound evenly loud. Endpoints are exact (0 → mute, 127 → 0 dB); the −144 dB range is only reachable at V=0, and above that the curve compresses into roughly the top −20 dB. Do **not** assume the linear `V × 144/127` scaling when predicting mixer positions from transport volume.

#### 7.6.5. Volume Sync Verification Commands
```bash
# Transport volume + State (10..127; healthy phone-initiated sync tracks this)
ssh root@192.168.2.120 "busctl get-property org.bluez /org/bluez/hci0/dev_30_E0_44_67_38_A2/fd0 org.bluez.MediaTransport1 State Volume"

# SetVolume method must exist on the transport (proves AVRCP TG init succeeded)
ssh root@192.168.2.120 "busctl introspect org.bluez /org/bluez/hci0/dev_30_E0_44_67_38_A2/fd0 org.bluez.MediaTransport1 | grep -E 'SetVolume|Volume'"

# BlueALSA-side volume (must equal transport volume; SoftVolume must be false)
ssh root@192.168.2.120 "bluealsa-cli info /org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsnk/source | grep -E 'Volume|Soft'"

# Merus hardware mixer position (follows transport volume per the table in 7.6.3)
ssh root@192.168.2.120 "amixer -c sndrpimerusamp sget 'A.Mstr Vol' | grep Mono"
```

Healthy-state checklist after (re)connection: transport `State = active` and `Volume` writable; `SetVolume` present on `MediaTransport1`; `bluealsa-cli` `Volume` equals transport `Volume`; `SoftVolume: false`; mixer follows transport per 7.6.3. If the mixer is stuck after boot, check for the stale `asound.state` restore (`A.Mstr Vol Volume value 0`) and verify the boot-race fix in 7.2 is still deployed. If the transport `Volume` is frozen during phone key presses, verify `[AVRCP] VolumeCategory = false` survived in `/etc/bluetooth/main.conf` (Rank 0 gate, 7.6.1) — note that `bluealsa-aplay` only drives the Merus mixer while the PCM is open (music streaming); volume updates received while idle are applied at PCM open.

---

## 8. Listening Ports & Network Services

```
Proto Recv-Q Send-Q Local Address:Port  Process
tcp   0      128    127.0.0.1:1234      camilladsp (PID 434)
tcp   0      128    0.0.0.0:22          sshd (PID 634)
tcp   0      128    [::]:22             sshd (PID 634)
udp   0      0      0.0.0.0:68          dhclient (PID 510)
```

---

## 9. Useful Diagnostic & Management Commands (Reference)

### Amplifier & Audio Telemetry
```bash
# Live MA12070P I2C register diagnostics & fault detection
ssh root@192.168.2.120 "python3 /root/amp/ma12070p_status.py"

# CamillaDSP live pipeline load, buffer levels, dBFS peak/RMS meters
ssh root@192.168.2.120 "python3 /root/amp/camilladsp-status.py"

# Overall Bluetooth stack & adapter status
ssh root@192.168.2.120 "python3 /root/amp/bt_status.py"
```

### Test Scripts Execution
```bash
# Display Diagnostic on Freenove 4.3" DSI Screen
ssh root@192.168.2.120 "python3 /root/freenove/display_info.py"

# Read temperature from DS18B20 1-Wire sensor
ssh root@192.168.2.120 "python3 /root/read_temp.py"

# Read ambient light level from BH1750
ssh root@192.168.2.120 "python3 /root/read_light_level.py"

# Test PWM Fan speed control & tachometer RPM feedback
ssh root@192.168.2.120 "python3 /root/test_fan.py"

# Test EC12 rotary encoder rotation, push button, and RGB LED sequence
ssh root@192.168.2.120 "python3 /root/test_all_encoder.py"
```

### Recompile Custom Merus Overlay (If Kernel / Overlays are Updated)
```bash
ssh root@192.168.2.120 "
  dtc -I dtb -O dts /boot/firmware/overlays/merus-amp.dtbo -o /tmp/merus-amp.dts && \
  grep -v 'pinctrl' /tmp/merus-amp.dts > /tmp/merus-amp-custom.dts && \
  dtc -I dts -O dtb -o /boot/firmware/overlays/merus-amp-custom.dtbo /tmp/merus-amp-custom.dts
"
```

### Quick Non-Interactive Hardware Checks
```bash
# Check Active Touchscreen Input Event Device
ssh root@192.168.2.120 "cat /proc/bus/input/devices | grep -A 5 'raspberrypi-ts'"

# Check Framebuffer Resolution & Geometry
ssh root@192.168.2.120 "fbset -fb /dev/fb0"

# Check 1-Wire sensor reading directly
ssh root@192.168.2.120 "cat /sys/bus/w1/devices/28-*/w1_slave"

# Scan I2C Bus 1 (shows 0x20 amplifier and 0x23 BH1750)
ssh root@192.168.2.120 "i2cdetect -y 1"
```

### Audio Services Status
```bash
ssh root@192.168.2.120 "systemctl status bluetooth bt-manager bluealsa bluealsa-aplay camilladsp --no-pager"
```

### BlueALSA Active Streams & Connected Devices
```bash
ssh root@192.168.2.120 "bluealsactl status"
ssh root@192.168.2.120 "bluealsactl list-pcms"
ssh root@192.168.2.120 "bluetoothctl devices"
```

### ALSA Amplifier Volumes & Controls
```bash
ssh root@192.168.2.120 "amixer -c sndrpimerusamp scontents"
```

---

## 10. Verified Build, Compile & Provisioning Runbook

This section reconciles and documents the step-by-step setup procedure taken to configure the Raspberry Pi from a fresh DietPi installation.

### 10.1. Hardware Device Tree Customization (Merus Amp)
To prevent the upstream `merus-amp` overlay from locking GPIO pins needed by the fan, encoder, and sensors:
```bash
# 1. Decompile stock overlay to DTS
dtc -I dtb -O dts /boot/firmware/overlays/merus-amp.dtbo -o /tmp/merus-amp.dts

# 2. Strip all pinctrl pin-mux blocks
grep -v "pinctrl" /tmp/merus-amp.dts > /tmp/merus-amp-custom.dts

# 3. Compile custom overlay binary
sudo dtc -I dts -O dtb -o /boot/firmware/overlays/merus-amp-custom.dtbo /tmp/merus-amp-custom.dts

# 4. Enable in /boot/firmware/config.txt:
# dtoverlay=merus-amp-custom
```

### 10.2. Hardware Mixer & Limiter Configuration
Sets the MA12070P hardware amplifier to unity gain, enables the hardware limiter for speaker protection, and persists to ALSA state:
```bash
# 1. Master & Channel Volumes to 0.00 dB Unity Gain
amixer -c sndrpimerusamp sset 'A.Mstr Vol' 144 unmute
amixer -c sndrpimerusamp sset 'B.L Vol' 100% unmute
amixer -c sndrpimerusamp sset 'C.R Vol' 100% unmute

# 2. Hardware Limiter: Enabled, Threshold at 0 dBFS (Value 60)
amixer -c sndrpimerusamp sset 'F.Limiter Enable' on
amixer -c sndrpimerusamp sset 'D.Lim thresh' 60

# 3. Limiter Dynamics: Fast Attack (protection), Slow Release (prevents pumping)
amixer -c sndrpimerusamp sset 'G.Limiter Attck' 'Fast'
amixer -c sndrpimerusamp sset 'H.Limiter Rls' 'Slow'

# 4. Power Management: Dynamic Power Mode, PMF0 Profile
amixer -c sndrpimerusamp sset 'Q.PM Prof' 'PMF0'
amixer -c sndrpimerusamp sset 'R.Power Mode' 'Dynamic power mode'

# 5. Commit immediately to ALSA state file (/var/lib/alsa/asound.state)
alsactl store
```

> **⚠️ DietPi Warning (`dietpi-config`):** Because this appliance uses a custom DTBO (`dtoverlay=merus-amp-custom`), `/boot/dietpi.txt` has `CONFIG_SOUNDCARD=none`. Running `dietpi-config` (or `/boot/dietpi/func/dietpi-set_hardware soundcard none`) triggers `Soundcard_Reset_All()`, which **deletes `/etc/asound.conf` and `/var/lib/alsa/asound.state` and purges `alsa-utils`**. If `dietpi-config` is ever invoked, immediately restore `alsa-utils`, `/etc/asound.conf`, and `/var/lib/alsa/asound.state` from `baseline/` and run `alsactl restore sndrpimerusamp && systemctl restart camilladsp bluealsa-aplay`.

### 10.3. ALSA Virtual Loopback Configuration
Allows BlueALSA audio to be piped through ALSA dmix and captured by CamillaDSP:
```bash
# 1. Configure persistent module load
echo "snd-aloop" > /etc/modules-load.d/snd-aloop.conf
modprobe snd-aloop

# 2. Install ALSA plugins (required for speexrate_medium resampler)
apt-get update
apt-get install -y libasound2-plugins
```

### 10.4. CamillaDSP Installation & Systemd Service
```bash
# 1. Create directories and download release binary
mkdir -p /etc/camilladsp /var/log/camilladsp
wget https://github.com/HEnquist/camilladsp/releases/download/v4.1.3/camilladsp-linux-aarch64.tar.gz -O /tmp/camilladsp.tar.gz
tar -xvf /tmp/camilladsp.tar.gz -C /usr/local/bin/
chmod +x /usr/local/bin/camilladsp
rm -f /tmp/camilladsp.tar.gz

# 2. Install CamillaDSP systemd service
cat <<'EOF' > /etc/systemd/system/camilladsp.service
[Unit]
Description=CamillaDSP ALSA Crossover Engine
After=sound.target
Requires=sound.target

[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/camilladsp -p 1234 -o /var/log/camilladsp/camilladsp.log /etc/camilladsp/crossover.yml
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

# 3. Enable and start service
systemctl daemon-reload
systemctl enable --now camilladsp
```

### 10.5. Building & Installing BlueZ 5.88 from Source (with C23 GCC 14 Patch)
BlueZ 5.88 brings key audio improvements to the appliance:
- **AVRCP 1Hz Position Polling (PR #2570):** Automatically queries `GetPlayStatus` every 1s when remote players do not fire `AVRCP_EVENT_PLAYBACK_POS_CHANGED`, keeping track progress and UI progress bars accurate.
- **Inbound ACL Connection Fallback (PR #2320 / #2308):** Introduces a 2-second connection fallback delay preventing premature AVDTP/AVCTP profile drops when phones initiate ACL connection before encryption keys finish exchanging.
- **Deterministic A2DP Codec Prioritization (PR #2189):** Automatic ordering (`LDAC > aptX-HD > aptX > AAC > SBC`).

```bash
# 1. Install BlueZ compilation dependencies
apt update && apt install -y \
  libudev-dev libical-dev libreadline-dev python3-docutils \
  libasound2-dev libglib2.0-dev libdbus-1-dev

# 2. Download and extract BlueZ 5.88
mkdir -p /root/build && cd /root/build
wget https://www.kernel.org/pub/linux/bluetooth/bluez-5.88.tar.xz
tar xf bluez-5.88.tar.xz
cd bluez-5.88

# 3. Apply GCC 14 (C23 default) fix in src/shared/ad.c:
# Line 1248 has 'return false;' in 'struct bt_ad_data *bt_ad_has_data(...)',
# which causes GCC 14 to fail compilation with incompatible pointer type error.
sed -i 's/return false;/return NULL;/' src/shared/ad.c

# 4. Configure BlueZ with systemd integration and appliance paths
./configure \
  --prefix=/usr \
  --mandir=/usr/share/man \
  --sysconfdir=/etc \
  --localstatedir=/var \
  --libexecdir=/usr/libexec \
  --enable-library \
  --enable-deprecated \
  --enable-pie \
  --enable-threads \
  --enable-systemd \
  --with-systemdsystemunitdir=/usr/lib/systemd/system \
  --with-systemduserunitdir=/usr/lib/systemd/user \
  --with-udevdir=/usr/lib/udev \
  --disable-cups \
  --disable-mesh \
  --disable-manpages

# 5. Compile and install
make -j$(nproc)
make install

# 6. Update multiarch shared libraries and run ldconfig
cp -a /usr/lib/libbluetooth.so* /usr/lib/aarch64-linux-gnu/
cp -a /usr/lib/pkgconfig/bluez.pc /usr/lib/aarch64-linux-gnu/pkgconfig/
ldconfig

# 7. Restore appliance /etc/bluetooth/main.conf
cat <<'EOF' > /etc/bluetooth/main.conf
[General]
Experimental = true
Name = DietPi Speaker
Class = 0x240414
DiscoverableTimeout = 0
PairableTimeout = 0
ControllerMode = bredr

[Policy]
AutoEnable = true
EOF

# 8. Reload systemd and restart Bluetooth daemon
systemctl daemon-reload
systemctl restart bluetooth
```

### 10.6. Building Fraunhofer LC3plus & BlueALSA v5.0.0 from Source (with NEON & Cortex-A53 Tuning)
Custom build targeting the Raspberry Pi 3's ARM Cortex-A53 CPU with hardware ARM NEON SIMD auto-vectorization (`-O2 -mcpu=cortex-a53+crc+simd -ftree-vectorize`) to ensure maximum DSP throughput with rock-solid stability and zero L1 instruction cache bloat:

```bash
# 1. Install compilation toolchain & codec header libraries
apt update && apt install -y \
  cmake git build-essential autoconf automake libtool pkg-config \
  libasound2-dev libbluetooth-dev libglib2.0-dev libsbc-dev libdbus-1-dev \
  libfreeaptx-dev libfdk-aac-dev libmp3lame-dev libmpg123-dev \
  libspandsp-dev liblc3-dev libsamplerate0-dev systemd-dev

# 2. Build and install Fraunhofer / ETSI LC3plus library (Hi-Res Audio Wireless with NEON)
cd /usr/src
rm -rf LC3plus
git clone --depth=1 https://github.com/arkq/LC3plus.git /usr/src/LC3plus
cd /usr/src/LC3plus/src/floating_point
rm -rf build libLC3plus.so && mkdir -p build
cmake -B build \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
  -DOPTIM=2 \
  -DCMAKE_C_FLAGS="-O2 -mcpu=cortex-a53+crc+simd -ftree-vectorize"
cmake --build build --target LC3plus_shared -j$(nproc)

install -m 755 libLC3plus.so /usr/lib/aarch64-linux-gnu/
install -m 644 lc3plus.h /usr/include/
install -m 644 defines.h /usr/include/
ldconfig

# 3. Build and install BlueALSA v5.0.0 with Cortex-A53 NEON vectorization
cd /usr/src
rm -rf bluez-alsa
git clone https://github.com/arkq/bluez-alsa.git
cd bluez-alsa
git checkout v5.0.0
autoreconf --install --force

mkdir -p build && cd build
make clean || true

CFLAGS="-O2 -mcpu=cortex-a53+crc+simd -ftree-vectorize" ../configure \
  --prefix=/usr \
  --sysconfdir=/etc \
  --with-alsaconfdir=/etc/alsa/conf.d \
  --enable-systemd \
  --enable-aplay \
  --with-libsamplerate \
  --disable-asha \
  --disable-ldac \
  --enable-lc3plus \
  --enable-faststream \
  --disable-opus \
  --enable-mp3lame \
  --enable-mpg123 \
  --enable-aptx \
  --enable-aptx-hd \
  --with-libfreeaptx \
  --enable-aac \
  --enable-lc3-swb \
  --enable-msbc \
  --enable-midi

make -j$(nproc)
make install
ldconfig

# Compatibility symlinks for legacy tool invocations
ln -sf /usr/bin/bluealsad /usr/bin/bluealsa
ln -sf /usr/bin/bluealsactl /usr/bin/bluealsa-cli
```

### 10.7. BlueALSA Systemd Services Installation
Configure the background daemon and audio pipe services:

```bash
# 1. Create BlueALSA daemon service (/etc/systemd/system/bluealsa.service)
cat <<'EOF' > /etc/systemd/system/bluealsa.service
[Unit]
Description=BlueALSA Bluetooth Audio Daemon (v5.0.0)
After=bluetooth.service
Requires=bluetooth.service

[Service]
Type=simple
User=root
StateDirectory=bluealsa
ExecStart=/usr/bin/bluealsad -S -p a2dp-sink --all-codecs -c-opus --sbc-quality=xq+
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

# 2. Create BlueALSA aplay forwarder service (/etc/systemd/system/bluealsa-aplay.service)
cat <<'EOF' > /etc/systemd/system/bluealsa-aplay.service
[Unit]
Description=BlueALSA A2DP Playback Forwarder
After=bluealsa.service sound.target camilladsp.service
Requires=bluealsa.service
Wants=camilladsp.service

[Service]
Type=simple
User=root
ExecStart=/usr/bin/bluealsa-aplay -S -D default -M hw:sndrpimerusamp --mixer-control="A.Mstr Vol"
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

# 3. Reload and enable services
systemctl daemon-reload
systemctl enable --now bluealsa bluealsa-aplay
```

### 10.8. Bluetooth Headless Auto-Pairing & Trust Agent (`bt-manager.service`)
Headless Bluetooth sinks require a D-Bus agent to automatically authorize incoming connections and set devices as "Trusted" without PIN dialog prompts.

In this appliance, auto-pairing and trust management is handled natively by **`bt-manager.service`** (`/usr/local/bin/bt-manager`), a pure-Rust service that hosts `org.bluez.Agent1` directly via `zbus`. This consolidates pairing supervision with the status monitor loop and BIP cover-art provider, eliminating the legacy standalone Python `bt-agent.service` and `/usr/local/bin/bt-auto-trust-agent.py`.

#### Key Architecture & Capabilities
1. **Zero Python Dependencies:** Operates natively in Rust without `python3-dbus` or `python3-gi`.
2. **Re-Pairing Bug Remediation:** Advertises `DisplayYesNo` capability by default, auto-confirming numeric comparison requests and permanently resolving the Raspberry Pi BlueZ 5.55+ silent bond-drop re-pairing bug.
3. **Non-Blocking Trust Assignment:** Uses asynchronous D-Bus property calls (`Device1.Trusted = true`) to prevent D-Bus re-entrancy deadlocks with `bluetoothd`.
4. **Lifecycle Supervision:** Automatically re-claims default agent status if `bluetoothd` stops or restarts.

#### Service Definition (`/etc/systemd/system/bt-manager.service`)
```ini
[Unit]
Description=DietPi Bluetooth Manager Service (Auto-Trust & Cover Art)
After=bluetooth.service
Requires=bluetooth.service

[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/bt-manager
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
# Enable and start the native bt-manager service
systemctl daemon-reload
systemctl enable --now bt-manager

# Decommission legacy bt-agent if previously installed
systemctl disable --now bt-agent.service 2>/dev/null || true
systemctl mask bt-agent.service 2>/dev/null || true
rm -f /etc/systemd/system/bt-agent.service /usr/local/bin/bt-auto-trust-agent.py
```

### 10.9. BlueALSA Runtime Directories & Package Note
```bash
# Install bluez tools & CLI status utility
apt update && apt install -y bluez bluez-tools bluez-alsa-utils

# BlueALSA runtime state directory and localstatedir compatibility symlink
mkdir -p /var/lib/bluealsa
mkdir -p /usr/var/lib
ln -sfn /var/lib/bluealsa /usr/var/lib/bluealsa
chmod 700 /var/lib/bluealsa
```
*(Note: Installing `bluez-alsa-utils` installs `bluealsa-cli` without overwriting the custom-compiled `/usr/bin/bluealsa` daemon).*

### 10.10. Python Libraries & Peripherals Dependencies
*(Note: Python D-Bus & GObject packages previously required for `bt-auto-trust-agent.py` have been deprecated; `bt-manager` runs as a compiled pure-Rust binary).*

```bash
# Hardware libraries (GPIO, SPI, SMBus for light sensor, PIL for displays)
apt install -y python3-pil python3-spidev python3-rpi.gpio python3-smbus2

# CamillaDSP Python Client (pycamilladsp)
pip install --break-system-packages git+https://github.com/HEnquist/pycamilladsp.git
```

### 10.11. Console Display Formatting
Suppresses the blinking text cursor on the framebuffer / DSI display console:
```bash
sudo sed -i 's/$/ vt.global_cursor_default=0/' /boot/firmware/cmdline.txt
```
