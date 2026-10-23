# Raspberry Pi Audio Bluetooth Server & DSP Sink — Baseline State Package

This repository folder (`baseline/`) contains a complete, verified snapshot of all configuration files, services, custom device tree overlays, scripts, and provisioning logic required to reproduce the audio Bluetooth receiver and DSP crossover server from scratch on a clean install of **Debian 13 (Trixie) / DietPi (aarch64)**.

---

## 1. Directory Structure

```text
baseline/
├── README.md                      # Comprehensive baseline documentation & guide
├── install.sh                     # Automated end-to-end clean installation & provisioning script
├── verify.sh                      # Automated 25-point audit & validation test suite
├── boot/
│   ├── config.txt                 # Boot firmware configuration (/boot/firmware/config.txt)
│   ├── cmdline.txt                # Kernel command line parameters (with vt.global_cursor_default=0)
│   └── overlays/
│       ├── merus-amp-custom.dtbo  # Precompiled custom device tree overlay (pinctrl stripped)
│       └── make_custom_overlay.sh # Script to rebuild custom DTBO from stock merus-amp.dtbo
├── etc/
│   ├── asound.conf                # ALSA Loopback, dmix resampler, and default routing
│   ├── alsa/
│   │   └── 20-bluealsa.conf       # BlueALSA ALSA plugin configuration
│   ├── bluetooth/
│   │   └── main.conf              # BlueZ audio loudspeaker class & auto-enable settings
│   ├── camilladsp/
│   │   └── crossover.yml          # Active 2-way DSP crossover & limiter filter graph
│   ├── modules-load.d/
│   │   ├── snd-aloop.conf         # Autoload ALSA virtual loopback driver
│   │   └── i2c-dev.conf           # Autoload I2C device nodes
│   └── systemd/system/
│       ├── camilladsp.service     # CamillaDSP WebSocket & filtering engine daemon
│       ├── bluealsa.service       # BlueALSA high-res A2DP audio sink daemon
│       ├── bluealsa-aplay.service # Forwarder feeding BlueALSA decoded audio into ALSA dmix
│       └── bt-agent.service       # Headless auto-trust & auto-pairing D-Bus agent
├── alsa/
│   └── asound.state               # Persisted MA12070P hardware mixer gains & limiter states
├── bin/
│   └── bt-auto-trust-agent.py     # Python D-Bus auto-trust pairing handler
└── scripts/
    ├── read_temp.py               # DS18B20 1-Wire thermal monitor
    ├── read_light_level.py        # BH1750 ambient light lux reader (I2C)
    ├── test_fan.py                # 4-pin PWM fan speed control & tachometer RPM counter
    ├── test_all_encoder.py        # EC12 rotary quadrature encoder, switch & RGB LED tester
    ├── render_ui.py               # Framebuffer UI graphics renderer
    ├── amp/
    │   ├── camilladsp-status.py   # Live DSP load, buffer fill & dBFS level meters
    │   ├── ma12070p_status.py     # Live MA12070P I2C register diagnostics & fault monitor
    │   └── bt_status.py           # Bluetooth connection status & adapter query
    └── freenove/
        ├── display_info.py        # Freenove 4.3" DSI screen telemetry calibration tool
        └── display_perf_test.py   # DSI framebuffer FPS and throughput benchmark
```

---

## 2. Quick Start: Recreating from Scratch on a Fresh Install

On a fresh Debian 13 (Trixie) or DietPi aarch64 installation on a Raspberry Pi 3:

```bash
# 1. Clone or copy the baseline folder to the target machine
scp -r baseline/ root@<rpi-ip>:/root/

# 2. SSH into the machine
ssh root@<rpi-ip>

# 3. Run the automated installer
cd /root/baseline
chmod +x install.sh verify.sh boot/overlays/make_custom_overlay.sh
sudo ./install.sh

# 4. Reboot to apply device tree overlays and boot parameters
sudo reboot
```

---

## 3. Post-Install Audit & Health Verification

Once rebooted, run the included audit script:
```bash
cd /root/baseline
./verify.sh
```
This performs an automated 25-point check across:
- Kernel modules (`snd-aloop`, `i2c-dev`, `snd_soc_ma120x0p`)
- Systemd services (`bluetooth`, `bt-agent`, `bluealsa`, `bluealsa-aplay`, `camilladsp`)
- Listening ports (`1234` for DSP WebSocket, `22` for SSH)
- ALSA sound cards (Card 0 Loopback, Card 1 MA12070P Amp)
- Hardware sensors (I2C address 0x20 amp, 0x23 BH1750 light sensor, 1-Wire DS18B20)
- Display & Touch (`/dev/fb0`, `raspberrypi-ts` on `/dev/input/event0`)
- Diagnostic scripts in `/root/`

---

## 4. Hardware Pinout Reference

| Physical Pin | BCM GPIO | Protocol / Function | Connected Peripheral | Electrical Details |
| :---: | :---: | :--- | :--- | :--- |
| **3** | BCM 2 | I2C-1 SDA | BH1750 Light Sensor & Amp | 3.3V Data (Hardware pull-up) |
| **5** | BCM 3 | I2C-1 SCL | BH1750 Light Sensor & Amp | 3.3V Clock (Hardware pull-up) |
| **12** | BCM 18 | I2S BCLK | InnoMaker Merus Amp | Bit Clock |
| **15** | BCM 22 | GPIO In | EC12 Push Button | Active-HIGH (to 3.3V), Internal Pull-Down |
| **18** | BCM 24 | GPIO In | EC12 Encoder Phase B | Active-LOW, Internal Pull-Up |
| **22** | BCM 25 | GPIO In | EC12 Encoder Phase A | Active-LOW, Internal Pull-Up |
| **31** | BCM 6 | GPIO In (IRQ) | Fan Tachometer | Falling Edge IRQ, External 10 kΩ pull-up |
| **32** | BCM 12 | GPIO Out | EC12 LED Blue | Active-LOW sink via 180 Ω |
| **33** | BCM 13 | GPIO Out (PWM) | Fan PWM Speed Control | 100 Hz Software PWM |
| **35** | BCM 19 | I2S LRCLK | InnoMaker Merus Amp | Frame Sync |
| **36** | BCM 16 | GPIO Out | EC12 LED Green | Active-LOW sink via 180 Ω |
| **37** | BCM 26 | 1-Wire w1 | DS18B20 Temp Sensor | 1-Wire Bus (`dtoverlay=w1-gpio`) |
| **38** | BCM 20 | GPIO Out | EC12 LED Red | Active-LOW sink via 330 Ω |
| **40** | BCM 21 | I2S DOUT | InnoMaker Merus Amp | Audio Data |
| **DSI Ribbon** | — | MIPI DSI | Freenove 4.3" Screen | 800x480 IPS & Touch (`raspberrypi-ts`) |

---

## 5. Audio Pipeline Architecture

```text
[Phone / Bluetooth Client]
         │ (A2DP: aptX-HD / AAC / Opus / SBC-XQ)
         ▼
  [TP-Link UB500 USB BT Adapter]
         │
         ▼
  [bluealsa.service (v4.3.1)]
         │
         ▼
  [bluealsa-aplay.service -D default]
         │
         ▼
  [ALSA Loopback Card 0: pcm.dsp_dmix @ 48kHz S32_LE]
         │
         ▼
  [camilladsp.service (PID 434, Port 1234)]
     ├── Mono Sum & Split (-6 dB)
     ├── Ch0 (Subwoofer): 45Hz HPF + 180Hz LPF + Limiter (-3.5 dB)
     └── Ch1 (Coax): 180Hz HPF + 3000Hz Shelf (-2 dB) + Limiter (-3.0 dB)
         │
         ▼
  [InnoMaker Merus MA12070P Amp: Card 1 (hw:sndrpimerusamp,0)]
         │
         ▼
  [Physical Speakers: Subwoofer + Coaxial Satellite]
```
