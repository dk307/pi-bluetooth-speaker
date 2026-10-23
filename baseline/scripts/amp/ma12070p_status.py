#!/usr/bin/env python3
import sys
import os
import fcntl
import argparse

I2C_BUS = 1
I2C_ADDR = 0x20
I2C_SLAVE_FORCE = 0x0706

# Registers
REG_AUDIO_IN   = 0x1B  # Audio format & clocking
REG_EH_CLEAR   = 0x2D  # Error handling / clear latch (bit 2)
REG_POWER_MODE = 0x60  # Power mode profile
REG_DCU_MON0   = 0x61  # DCU monitor channel 0
REG_DCU_MON1   = 0x62  # DCU monitor channel 1
REG_ERROR_ACC  = 0x6D  # Latched historical errors
REG_ERROR_LIVE = 0x7C  # Live / real-time errors

ERROR_BITS = [
    (0, "FLYING_CAP_OVERVOLTAGE", "Flying capacitor voltage exceeded safe limit"),
    (1, "OVERCURRENT_PROTECT",   "Speaker overload / short circuit detected"),
    (2, "PLL_CLOCK_LOSS",        "Audio clock missing or PLL lost sync (normal on boot/idle)"),
    (3, "UNDERVOLTAGE_PVDD",     "Main DC power supply dropped below 4V"),
    (4, "HIGH_TEMP_WARNING",     "Die temperature high (>125°C) - cooling needed"),
    (5, "THERMAL_SHUTDOWN",      "Thermal overload (>150°C) - amplifier auto-shutoff"),
    (6, "PIN_SHORT_CIRCUIT",     "Power stage output pin-to-pin short detected"),
    (7, "DC_OFFSET_PROTECT",     "DC voltage detected at speaker terminals"),
]

def open_i2c(bus, addr):
    dev = f"/dev/i2c-{bus}"
    if not os.path.exists(dev):
        print(f"Error: {dev} not found. Enable I2C in DietPi first.", file=sys.stderr)
        sys.exit(1)
    fd = os.open(dev, os.O_RDWR)
    fcntl.ioctl(fd, I2C_SLAVE_FORCE, addr)
    return fd

def read_reg(fd, reg):
    try:
        os.write(fd, bytes([reg]))
        return os.read(fd, 1)[0]
    except OSError as e:
        print(f"I2C Read Error (reg 0x{reg:02X}): {e}", file=sys.stderr)
        sys.exit(1)

def write_reg(fd, reg, val):
    try:
        os.write(fd, bytes([reg, val]))
    except OSError as e:
        print(f"I2C Write Error (reg 0x{reg:02X}): {e}", file=sys.stderr)
        sys.exit(1)

def parse_errors(val):
    flags = []
    for bit, name, desc in ERROR_BITS:
        if val & (1 << bit):
            flags.append((name, desc))
    return flags

def decode_audio_format(val):
    # Bits [2:0]: Audio input format
    formats = {
        0b000: "Standard I2S (32-bit slot)",
        0b001: "Left-Justified (LJ)",
        0b010: "Right-Justified 16-bit (RJ-16)",
        0b011: "Right-Justified 20-bit (RJ-20)",
        0b100: "Right-Justified 24-bit (RJ-24)",
    }
    fmt_bits = val & 0x07
    fmt_str = formats.get(fmt_bits, f"Custom Format (0b{fmt_bits:03b})")

    # Bits [4:3]: Sample rate / clock ratio mode
    ratio_bits = (val >> 3) & 0x03
    ratios = {
        0b00: "Auto-detect clock ratio",
        0b01: "Manual Ratio (64x fs)",
        0b10: "Manual Ratio (128x fs)",
        0b11: "Manual Ratio (256x fs)",
    }
    ratio_str = ratios.get(ratio_bits, "Unknown ratio mode")
    return f"{fmt_str}, {ratio_str}"

def decode_power_profile(val):
    pmp_num = val & 0x03
    pmp_names = {
        0: "PMP0 (High Audio Performance / Class D)",
        1: "PMP1 (Low Idle Power Optimization)",
        2: "PMP2 (Default Balanced / Dynamic Power Mode)",
        3: "PMP3 (Custom Profile)",
    }
    return pmp_names.get(pmp_num, f"PMP{pmp_num}")

def decode_dcu_monitor(dcu0, dcu1):
    # Check charge pump / DCU operational bit (bit 4)
    ch0_active = "Active / Regulated" if (dcu0 & 0x10) else "Standby / Idle"
    ch1_active = "Active / Regulated" if (dcu1 & 0x10) else "Standby / Idle"
    return f"Ch0: {ch0_active} | Ch1: {ch1_active}"

def main():
    parser = argparse.ArgumentParser(description="MA12070P Amplifier Hardware Monitor")
    parser.add_argument("--clear", action="store_true", help="Clear historical latched errors")
    args = parser.parse_args()

    fd = open_i2c(I2C_BUS, I2C_ADDR)

    if args.clear:
        cur = read_reg(fd, REG_EH_CLEAR)
        write_reg(fd, REG_EH_CLEAR, cur | 0x04)
        write_reg(fd, REG_EH_CLEAR, cur & ~0x04)
        print("Success: Cleared historical latched error logs.")
        return

    live_err = read_reg(fd, REG_ERROR_LIVE)
    acc_err  = read_reg(fd, REG_ERROR_ACC)
    pwr_mode = read_reg(fd, REG_POWER_MODE)
    dcu0     = read_reg(fd, REG_DCU_MON0)
    dcu1     = read_reg(fd, REG_DCU_MON1)
    audio_in = read_reg(fd, REG_AUDIO_IN)

    print("=========================================================")
    print("            MA12070P AMPLIFIER STATUS                    ")
    print("=========================================================")
    print(f"Power Mode Profile    : {decode_power_profile(pwr_mode)}")
    print(f"I2S Format & Timing   : {decode_audio_format(audio_in)}")
    print(f"Internal Charge Pumps : {decode_dcu_monitor(dcu0, dcu1)}")
    print("---------------------------------------------------------")

    # Real-time state
    print("Real-Time Amplifier Status:")
    live_flags = parse_errors(live_err)
    if not live_flags:
        print("  [OK] Normal operation (No faults detected)")
    else:
        for name, desc in live_flags:
            print(f"  [CRITICAL FAULT] {name}: {desc}")

    print("---------------------------------------------------------")

    # Historical state
    print("Historical Event History (Latched Since Boot):")
    acc_flags = parse_errors(acc_err)
    if not acc_flags:
        print("  [OK] No errors logged since boot")
    else:
        for name, desc in acc_flags:
            print(f"  [HISTORICAL EVENT] {name}")
            print(f"                     ↳ Detail: {desc}")
        print("\nTip: Clear this historical log with: python3 ma12070p_status.py --clear")
    print("=========================================================")

if __name__ == "__main__":
    main()
