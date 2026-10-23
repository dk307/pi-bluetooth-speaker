#!/usr/bin/env python3
import time
import smbus2

BH1750_ADDR = 0x23
CMD_POWER_ON = 0x01
CMD_CONTINUOUS_H_RES = 0x10  # 1 lx resolution, ~120ms integration time

bus = smbus2.SMBus(1)

# Wake sensor and enter continuous high-resolution mode
bus.write_byte(BH1750_ADDR, CMD_POWER_ON)
bus.write_byte(BH1750_ADDR, CMD_CONTINUOUS_H_RES)
time.sleep(0.18)  # Wait for first integration cycle

print("Reading light levels (Press Ctrl+C to stop):\n")

try:
    while True:
        data = bus.read_i2c_block_data(BH1750_ADDR, CMD_CONTINUOUS_H_RES, 2)
        raw_val = (data[0] << 8) | data[1]
        lux = raw_val / 1.2  # Datasheet conversion factor
        print(f"Illuminance: {lux:7.2f} lx", end="\r", flush=True)
        time.sleep(0.3)
except KeyboardInterrupt:
    print("\nExiting.")
finally:
    bus.close()
