#!/usr/bin/env python3
import glob
import time
from datetime import datetime

def read_temp_raw(device_file):
    try:
        with open(device_file, "r") as f:
            return f.readlines()
    except IOError:
        return []

def parse_temp(device_file):
    lines = read_temp_raw(device_file)
    # Check for valid CRC match
    if not lines or lines[0].strip()[-3:] != "YES":
        return None

    # Parse temperature in millidegrees (t=XXXXX)
    equals_pos = lines[1].find("t=")
    if equals_pos != -1:
        temp_str = lines[1][equals_pos + 2:].strip()
        temp_c = float(temp_str) / 1000.0
        temp_f = (temp_c * 9.0 / 5.0) + 32.0
        return temp_c, temp_f
    return None

def main():
    devices = glob.glob("/sys/bus/w1/devices/28-*")
    if not devices:
        print("Error: No DS18B20 sensors detected under /sys/bus/w1/devices/")
        return

    print("==================================================")
    print("      DS18B20 1-WIRE TEMPERATURE MONITOR         ")
    print("==================================================")
    print(f"Discovered {len(devices)} sensor(s):")
    for dev in devices:
        print(f"  • {dev.split('/')[-1]}")
    print("Press Ctrl+C to exit.\n")

    try:
        while True:
            timestamp = datetime.now().strftime("%H:%M:%S")
            for dev in devices:
                sensor_id = dev.split("/")[-1]
                result = parse_temp(f"{dev}/w1_slave")
                
                if result is not None:
                    temp_c, temp_f = result
                    print(f"[{timestamp}] [{sensor_id}] {temp_c:6.2f}°C  |  {temp_f:6.2f}°F")
                else:
                    print(f"[{timestamp}] [{sensor_id}] Read error / CRC check failed")
            
            time.sleep(1.0)

    except KeyboardInterrupt:
        print("\nMonitoring stopped.")

if __name__ == "__main__":
    main()
