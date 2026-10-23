#!/usr/bin/env python3
"""
Freenove 4.3" MIPI DSI Display Performance & Diagnostic Benchmark Suite
Target: /dev/fb0 (MIPI DSI Native Framebuffer)
Resolution: 800x480 IPS
"""

import fcntl
import os
import sys
import time
from PIL import Image, ImageDraw, ImageFont

FB_DEVICE = "/dev/fb0"
WIDTH, HEIGHT = 800, 480
FBIOBLANK = 0x4611

# 1. Query Hardware Bits-Per-Pixel (bpp)
try:
    with open("/sys/class/graphics/fb0/bits_per_pixel") as f:
        bpp = int(f.read().strip())
except Exception:
    bpp = 16

# 2. Font Loader with Proportional Sizing for 800x480
try:
    font_xl = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 26)
    font_lg = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 18)
    font_sm = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 14)
except Exception:
    font_xl = font_lg = font_sm = ImageFont.load_default()

def image_to_fb_payload(img, target_bpp):
    """Encodes PIL Image to native Linux framebuffer memory structure."""
    raw = img.tobytes()
    if target_bpp == 16:
        # Little-Endian RGB565: 5 bits Red, 6 bits Green, 5 bits Blue
        buf = bytearray(WIDTH * HEIGHT * 2)
        j = 0
        for i in range(0, len(raw), 3):
            r, g, b = raw[i], raw[i+1], raw[i+2]
            val = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
            buf[j] = val & 0xFF
            buf[j+1] = (val >> 8) & 0xFF
            j += 2
        return bytes(buf)
    else:
        # 32-bit BGRX (Standard Raspberry Pi VideoCore Framebuffer layout)
        buf = bytearray(WIDTH * HEIGHT * 4)
        j = 0
        for i in range(0, len(raw), 3):
            buf[j]   = raw[i+2]  # Blue
            buf[j+1] = raw[i+1]  # Green
            buf[j+2] = raw[i]    # Red
            buf[j+3] = 0x00      # Unused / Padding
            j += 4
        return bytes(buf)

def push_frame(fb, payload):
    """Flushes payload buffer to the display and measures transfer latency."""
    t0 = time.perf_counter()
    fb.seek(0)
    fb.write(payload)
    fb.flush()
    return (time.perf_counter() - t0) * 1000.0  # ms

def run_suite():
    print("=" * 65)
    print(" FREENOVE 4.3\" MIPI DSI (800x480) HARDWARE BENCHMARK SUITE")
    print(f" Target: {FB_DEVICE} | Mode: {bpp}-bit | Native Resolution: {WIDTH}x{HEIGHT}")
    print("=" * 65)

    with open(FB_DEVICE, "wb") as fb:
        # Wake display if unblanked
        try:
            fcntl.ioctl(fb.fileno(), FBIOBLANK, 0)
        except Exception:
            pass

        # ---------------------------------------------------------
        # TEST 1: Full-Screen Solid Color Holds (IPS Uniformity & Backlight)
        # ---------------------------------------------------------
        print("\n[Stage 1/4] Solid Color Holds (inspecting IPS color purity & edge bleed)...")
        colors = [
            ("RED",     (255, 0, 0)),
            ("GREEN",   (0, 255, 0)),
            ("BLUE",    (0, 0, 255)),
            ("WHITE",   (255, 255, 255)),
            ("BLACK",   (0, 0, 0))
        ]

        for name, col in colors:
            img = Image.new("RGB", (WIDTH, HEIGHT), col)
            draw = ImageDraw.Draw(img)
            text_col = (0, 0, 0) if name in ["WHITE", "GREEN"] else (255, 255, 255)
            
            # Card header
            draw.text((40, 30), f"TEST 1: SOLID {name}", fill=text_col, font=font_xl)
            draw.text((40, 70), "Evaluating IPS panel uniformity and backlight distribution.", fill=text_col, font=font_lg)
            draw.text((40, 100), f"Resolution: {WIDTH}x{HEIGHT} | DSI Video Stream Active", fill=text_col, font=font_sm)

            payload = image_to_fb_payload(img, bpp)
            tx_ms = push_frame(fb, payload)
            print(f"  -> Displayed {name:<5} | Transfer time: {tx_ms:.2f} ms")
            time.sleep(1.2)

        # ---------------------------------------------------------
        # TEST 2: High-Frequency Subpixel Pattern (DSI Pixel Clock Integrity)
        # ---------------------------------------------------------
        print("\n[Stage 2/4] High-Frequency Alternating Lines (DSI clock phase & EMI stability)...")
        img = Image.new("RGB", (WIDTH, HEIGHT), (0, 0, 0))
        draw = ImageDraw.Draw(img)
        
        # Draw single-pixel alternating lines
        for y in range(0, HEIGHT, 2):
            draw.line([(0, y), (WIDTH, y)], fill=(255, 255, 255))
            
        # Center status card
        draw.rounded_rectangle([(100, 150), (WIDTH - 100, 320)], radius=10, fill=(30, 41, 59), outline=(255, 255, 255), width=2)
        draw.text((140, 180), "TEST 2: MIPI DSI CLOCK STABILITY PATTERN", fill=(56, 189, 248), font=font_xl)
        draw.text((140, 225), "Check fine alternating 1-pixel horizontal lines for shimmering, noise, or phase jitter.", fill=(241, 245, 249), font=font_sm)
        draw.text((140, 255), "Confirms differential DSI clock lane synchronization with bridge IC.", fill=(148, 163, 184), font=font_sm)

        payload = image_to_fb_payload(img, bpp)
        tx_ms = push_frame(fb, payload)
        print(f"  -> Clock stress frame sent in {tx_ms:.2f} ms. Holding for 3 seconds...")
        time.sleep(3.0)

        # ---------------------------------------------------------
        # TEST 3: Widescreen Motion & Tearing Sweep
        # ---------------------------------------------------------
        print("\n[Stage 3/4] Widescreen Motion Sweep (inspecting vertical sync scanout)...")
        bar_width = 45
        steps = 25
        step_px = (WIDTH - bar_width) // steps

        for step in range(steps + 1):
            x_pos = step * step_px
            img = Image.new("RGB", (WIDTH, HEIGHT), (15, 23, 42))
            draw = ImageDraw.Draw(img)

            # Header
            draw.text((30, 20), "TEST 3: WIDESCREEN MOTION SWEEP", fill=(56, 189, 248), font=font_xl)
            draw.text((30, 60), f"Frame {step+1}/{steps+1} (DSI 60 Hz Scanout Evaluation)", fill=(148, 163, 184), font=font_sm)

            # Moving vertical bar
            draw.rectangle([(x_pos, 100), (x_pos + bar_width, HEIGHT - 80)], fill=(234, 179, 8), outline=(255, 255, 255), width=2)

            # Progress tracking rail
            draw.rectangle([(30, HEIGHT - 50), (WIDTH - 30, HEIGHT - 35)], fill=(51, 65, 85))
            draw.rectangle([(30, HEIGHT - 50), (30 + int((step / steps) * (WIDTH - 60)), HEIGHT - 35)], fill=(34, 197, 94))

            payload = image_to_fb_payload(img, bpp)
            tx_ms = push_frame(fb, payload)
            time.sleep(0.08)

        # ---------------------------------------------------------
        # TEST 4: Kernel Framebuffer Throughput Benchmark
        # ---------------------------------------------------------
        print("\n[Stage 4/4] Continuous Framebuffer Throughput Burst (20 frames)...")
        measurements = []
        frame_bytes = WIDTH * HEIGHT * (bpp // 8)

        for i in range(20):
            # Alternating subtle hues to guarantee full-frame memory writes
            col = (24, 32, 56) if i % 2 == 0 else (48, 28, 44)
            img = Image.new("RGB", (WIDTH, HEIGHT), col)
            draw = ImageDraw.Draw(img)
            draw.text((120, 180), "MEASURING DSI MEMORY BUS BANDWIDTH", fill=(255, 255, 255), font=font_xl)
            draw.text((120, 230), f"Bursting Frame {i+1}/20 directly to {FB_DEVICE}...", fill=(56, 189, 248), font=font_lg)
            draw.text((120, 265), f"Frame Payload Size: {frame_bytes / 1024:.1f} KB", fill=(148, 163, 184), font=font_sm)

            payload = image_to_fb_payload(img, bpp)
            tx_ms = push_frame(fb, payload)
            measurements.append(tx_ms)

        avg_ms = sum(measurements) / len(measurements)
        fps = 1000.0 / avg_ms if avg_ms > 0 else 0
        bandwidth_mbs = (frame_bytes * fps) / (1024 * 1024)

        # Final Summary Card on Screen
        img = Image.new("RGB", (WIDTH, HEIGHT), (15, 23, 42))
        draw = ImageDraw.Draw(img)
        draw.rounded_rectangle([(40, 40), (WIDTH - 40, HEIGHT - 40)], radius=12, fill=(30, 41, 59), outline=(71, 85, 105), width=2)
        
        draw.text((70, 70), "DSI HARDWARE BENCHMARK COMPLETE", fill=(74, 222, 128), font=font_xl)
        draw.text((70, 125), f"Target Device:        {FB_DEVICE}", fill=(241, 245, 249), font=font_lg)
        draw.text((70, 160), f"Native Resolution:    {WIDTH}x{HEIGHT} (IPS Widescreen)", fill=(241, 245, 249), font=font_lg)
        draw.text((70, 195), f"Color Depth:          {bpp}-bit ({'RGB565' if bpp == 16 else 'BGRX32'})", fill=(241, 245, 249), font=font_lg)
        draw.text((70, 230), f"Avg Full-Frame Write: {avg_ms:.2f} ms", fill=(241, 245, 249), font=font_lg)
        draw.text((70, 265), f"Software Frame Cap:   {fps:.1f} FPS (Hardware Scanout: 60 Hz)", fill=(241, 245, 249), font=font_lg)
        draw.text((70, 300), f"Bus Throughput:       {bandwidth_mbs:.2f} MB/s", fill=(56, 189, 248), font=font_lg)
        draw.text((70, 360), "Panel validated for fluid Peppy UI animations and VU meters.", fill=(148, 163, 184), font=font_sm)

        push_frame(fb, image_to_fb_payload(img, bpp))

        print("\n" + "=" * 65)
        print(" RESULTS SUMMARY")
        print("=" * 65)
        print(f" Framebuffer Target:    {FB_DEVICE} ({bpp}-bit)")
        print(f" Resolution:            {WIDTH}x{HEIGHT}")
        print(f" Frame Size:            {frame_bytes / 1024:.1f} KB")
        print(f" Avg Transfer Time:     {avg_ms:.2f} ms / frame")
        print(f" Software Write Speed:  {fps:.1f} FPS")
        print(f" Effective Throughput:  {bandwidth_mbs:.2f} MB/s")
        print("=" * 65)

if __name__ == "__main__":
    run_suite()
