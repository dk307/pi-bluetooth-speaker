#!/usr/bin/env python3
"""
Freenove 4.3" DSI Display Diagnostic & Calibration Tool
Target: /dev/fb0 (MIPI DSI Native Framebuffer)
Resolution: 800x480 @ 16-bit / 32-bit
"""

import fcntl
import os
import re
import socket
import subprocess
import time
from PIL import Image, ImageDraw, ImageFont

FB_DEVICE = "/dev/fb0"
WIDTH, HEIGHT = 800, 480
FBIOBLANK = 0x4611


def get_ip():
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception:
        return "127.0.0.1"


def get_cpu_temp():
    try:
        with open("/sys/class/thermal/thermal_zone0/temp", "r") as f:
            return f"{float(f.read().strip()) / 1000.0:.1f}°C"
    except Exception:
        return "N/A"


def get_backlight_level():
    """Reads current backlight level from the Raspberry Pi backlight sysfs driver."""
    paths = [
        "/sys/class/backlight/rpi_backlight/actual_brightness",
        "/sys/class/backlight/rpi_backlight/brightness",
    ]
    for p in paths:
        if os.path.exists(p):
            try:
                with open(p, "r") as f:
                    val = int(f.read().strip())
                    pct = int((val / 255.0) * 100)
                    return f"{val}/255 ({pct}%)"
            except Exception:
                pass
    return "Hardware Keypad / Unmanaged"


def get_touch_device():
    """Scans /proc/bus/input/devices for capacitive touch controllers."""
    try:
        with open("/proc/bus/input/devices", "r") as f:
            content = f.read()
        for block in content.split("\n\n"):
            if re.search(r"(Touch|Goodix|FT5|edt_ft|FT6)", block, re.IGNORECASE):
                name_match = re.search(r'Name="([^"]+)"', block)
                handler_match = re.search(r"Handlers=.*?(event\d+)", block)
                name = name_match.group(1) if name_match else "Capacitive Touch"
                handler = handler_match.group(1) if handler_match else "eventX"
                return f"{name} ({handler})"
    except Exception:
        pass
    return "No Touch Input Detected"


def get_system_stats():
    """Fetches memory usage and load average."""
    try:
        with open("/proc/meminfo", "r") as f:
            lines = f.readlines()
        mem_total = int([l for l in lines if "MemTotal" in l][0].split()[1]) // 1024
        mem_avail = (
            int([l for l in lines if "MemAvailable" in l][0].split()[1]) // 1024
        )
        mem_used = mem_total - mem_avail
        ram_str = f"{mem_used} MB / {mem_total} MB"
    except Exception:
        ram_str = "N/A"

    try:
        load = os.getloadavg()[0]
        load_str = f"{load:.2f}"
    except Exception:
        load_str = "N/A"

    return ram_str, load_str


# 1. Detect bits per pixel on /dev/fb0
try:
    with open("/sys/class/graphics/fb0/bits_per_pixel") as f:
        bpp = int(f.read().strip())
except Exception:
    bpp = 16

# 2. Render Diagnostic Pattern (800x480 Canvas)
img = Image.new("RGB", (WIDTH, HEIGHT), (15, 23, 42))  # Slate dark background
draw = ImageDraw.Draw(img)

# Outer 1px perimeter border (tests full 800x480 display clipping)
draw.rectangle([(0, 0), (WIDTH - 1, HEIGHT - 1)], outline=(255, 255, 255))

# Top Color Bar Spectrum (8 Reference Blocks)
colors = [
    ("RED", (255, 0, 0)),
    ("GREEN", (0, 255, 0)),
    ("BLUE", (0, 0, 255)),
    ("YELLOW", (255, 255, 0)),
    ("CYAN", (0, 255, 255)),
    ("MAGENTA", (255, 0, 255)),
    ("WHITE", (255, 255, 255)),
    ("GRAY", (128, 128, 128)),
]
bar_w = WIDTH // len(colors)
for i, (name, col) in enumerate(colors):
    x0 = i * bar_w
    x1 = (i + 1) * bar_w if i < len(colors) - 1 else WIDTH - 1
    draw.rectangle([(x0, 1), (x1, 55)], fill=col)

# Fonts
try:
    font_title = ImageFont.truetype(
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 20
    )
    font_body = ImageFont.truetype(
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 15
    )
    font_sm = ImageFont.truetype(
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 13
    )
except Exception:
    font_title = font_body = font_sm = ImageFont.load_default()

# Corner Coordinate Tags
draw.text((8, 60), "(0,0)", fill=(255, 255, 255), font=font_sm)
draw.text(
    (WIDTH - 75, 60), f"({WIDTH-1},0)", fill=(255, 255, 255), font=font_sm
)
draw.text(
    (8, HEIGHT - 20), f"(0,{HEIGHT-1})", fill=(255, 255, 255), font=font_sm
)
draw.text(
    (WIDTH - 95, HEIGHT - 20),
    f"({WIDTH-1},{HEIGHT-1})",
    fill=(255, 255, 255),
    font=font_sm,
)

# Header Title
draw.text(
    (24, 75),
    "FREENOVE 4.3\" MIPI DSI IPS DISPLAY VERIFICATION",
    fill=(56, 189, 248),
    font=font_title,
)

# Left Diagnostic Card: Hardware & Video Pipeline
card1_x0, card1_y0, card1_x1, card1_y1 = 24, 115, 388, 380
draw.rounded_rectangle(
    [(card1_x0, card1_y0), (card1_x1, card1_y1)],
    radius=8,
    fill=(30, 41, 59),
    outline=(71, 85, 105),
)
draw.text(
    (card1_x0 + 16, card1_y0 + 14),
    "DISPLAY HARDWARE",
    fill=(148, 163, 184),
    font=font_sm,
)

h_labels = [
    ("Interface:", "MIPI DSI 15-pin (Native GPU)"),
    ("Target Device:", f"{FB_DEVICE} ({bpp} bpp detected)"),
    ("Resolution:", f"{WIDTH} x {HEIGHT} (IPS 5:3 Widescreen)"),
    ("Touch Driver:", get_touch_device()),
    ("Backlight:", get_backlight_level()),
]
y_offset = card1_y0 + 42
for label, val in h_labels:
    draw.text((card1_x0 + 16, y_offset), label, fill=(148, 163, 184), font=font_sm)
    draw.text((card1_x0 + 130, y_offset), val, fill=(241, 245, 249), font=font_sm)
    y_offset += 38

# Right Diagnostic Card: Host & System Metrics
ram_usage, sys_load = get_system_stats()
card2_x0, card2_y0, card2_x1, card2_y1 = 412, 115, 776, 380
draw.rounded_rectangle(
    [(card2_x0, card2_y0), (card2_x1, card2_y1)],
    radius=8,
    fill=(30, 41, 59),
    outline=(71, 85, 105),
)
draw.text(
    (card2_x0 + 16, card2_y0 + 14),
    "HOST SYSTEM STATUS",
    fill=(148, 163, 184),
    font=font_sm,
)

s_labels = [
    ("IP Address:", get_ip(), (241, 245, 249)),
    ("CPU Temp:", get_cpu_temp(), (74, 222, 128)),
    ("RAM Usage:", ram_usage, (241, 245, 249)),
    ("1-Min Load:", sys_load, (241, 245, 249)),
    ("Timestamp:", time.strftime("%Y-%m-%d %H:%M:%S"), (148, 163, 184)),
]
y_offset = card2_y0 + 42
for label, val, color in s_labels:
    draw.text((card2_x0 + 16, y_offset), label, fill=(148, 163, 184), font=font_sm)
    draw.text((card2_x0 + 120, y_offset), val, fill=color, font=font_sm)
    y_offset += 38

# Bottom Calibration Elements (Aspect Ratio Check)
# A true circle verifies 1:1 pixel aspect ratio (no horizontal or vertical stretching)
circle_cx, circle_cy, circle_r = 730, 430, 32
draw.ellipse(
    [
        (circle_cx - circle_r, circle_cy - circle_r),
        (circle_cx + circle_r, circle_cy + circle_r),
    ],
    outline=(56, 189, 248),
    width=2,
)
draw.text(
    (circle_cx - 24, circle_cy - 6),
    "1:1 CAL",
    fill=(56, 189, 248),
    font=font_sm,
)

# Gradient calibration bar
grad_x0, grad_y0, grad_w, grad_h = 24, 405, 650, 22
draw.rectangle(
    [(grad_x0, grad_y0), (grad_x0 + grad_w, grad_y0 + grad_h)],
    outline=(71, 85, 105),
)
for step in range(grad_w):
    intensity = int((step / grad_w) * 255)
    draw.line(
        [(grad_x0 + step, grad_y0 + 1), (grad_x0 + step, grad_y0 + grad_h - 1)],
        fill=(intensity, intensity, intensity),
    )

draw.text(
    (grad_x0, grad_y0 + 26),
    "256-Level Grayscale Gradient & Geometry Ring",
    fill=(100, 116, 139),
    font=font_sm,
)

# 3. Pixel Conversion & Transmission to /dev/fb0
raw = img.tobytes()
if bpp == 16:
    buf = bytearray(WIDTH * HEIGHT * 2)
    j = 0
    for i in range(0, len(raw), 3):
        r, g, b = raw[i], raw[i + 1], raw[i + 2]
        val = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
        buf[j] = val & 0xFF
        buf[j + 1] = (val >> 8) & 0xFF
        j += 2
    payload = bytes(buf)
else:
    # 32-bit Framebuffer (Raspberry Pi VideoCore standard format: BGRA / BGRX)
    buf = bytearray(WIDTH * HEIGHT * 4)
    j = 0
    for i in range(0, len(raw), 3):
        buf[j] = raw[i + 2]      # Blue
        buf[j + 1] = raw[i + 1]  # Green
        buf[j + 2] = raw[i]      # Red
        buf[j + 3] = 0x00        # Alpha / Padding
        j += 4
    payload = bytes(buf)

# 4. Push Frame
with open(FB_DEVICE, "wb") as fb:
    fcntl.ioctl(fb.fileno(), FBIOBLANK, 0)
    fb.seek(0)
    fb.write(payload)
    fb.flush()

print(f"Test frame written to {FB_DEVICE} ({len(payload)} bytes, {bpp} bpp).")
