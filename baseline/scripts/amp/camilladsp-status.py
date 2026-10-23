#!/usr/bin/env python3
import sys
import time
from camilladsp import CamillaClient

def main():
    client = CamillaClient('127.0.0.1', 1234)
    client.connect()

    fmt = lambda vals: [round(x, 1) for x in vals] if isinstance(vals, list) else vals

    try:
        buf_samples = client.status.buffer_level()
        cpu_load = client.status.processing_load()

        print("=== CamillaDSP Pipeline Status ===")
        print("Processing State         :", client.general.state())
        print("Processing Load          :", f"{cpu_load:.2f}%")
        print("Rate Adjust Ratio        :", round(client.status.rate_adjust(), 6))
        print("Buffer Fill              :", f"{buf_samples} / 1024 samples ({buf_samples / 1024 * 100:.1f}%)")
        print("Capture Peak (L/R)       :", fmt(client.levels.capture_peak()), "dBFS")
        print("Capture RMS (L/R)        :", fmt(client.levels.capture_rms()), "dBFS")
        print("Playback Peak (Sub/Coax) :", fmt(client.levels.playback_peak()), "dBFS")
        print("Playback RMS (Sub/Coax)  :", fmt(client.levels.playback_rms()), "dBFS")
        print("Clipped Samples Counter  :", client.status.clipped_samples())
    finally:
        client.disconnect()

if __name__ == '__main__':
    main()
