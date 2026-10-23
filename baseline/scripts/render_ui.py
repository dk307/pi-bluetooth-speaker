#!/usr/bin/env python3
import time
import socket
import subprocess
import spidev
import RPi.GPIO as GPIO
from PIL import Image, ImageDraw, ImageFont

DC_PIN  = 25
RST_PIN = 27
CS_PIN  = 8
BL_PIN  = 12

GPIO.setwarnings(False)
GPIO.setmode(GPIO.BCM)
GPIO.setup(DC_PIN, GPIO.OUT)
GPIO.setup(RST_PIN, GPIO.OUT)
GPIO.setup(CS_PIN, GPIO.OUT, initial=GPIO.HIGH)
GPIO.setup(BL_PIN, GPIO.OUT, initial=GPIO.HIGH)

# Open Hardware SPI
spi = spidev.SpiDev()
spi.open(0, 0)
spi.max_speed_hz = 16000000  # 16 MHz for high frame throughput
spi.mode = 0
spi.no_cs = True

def write_cmd(cmd):
    GPIO.output(CS_PIN, GPIO.LOW)
    GPIO.output(DC_PIN, GPIO.LOW)
    spi.writebytes([cmd])
    GPIO.output(CS_PIN, GPIO.HIGH)

def write_data(val):
    GPIO.output(CS_PIN, GPIO.LOW)
    GPIO.output(DC_PIN, GPIO.HIGH)
    if isinstance(val, (list, tuple)):
        spi.writebytes(list(val))
    else:
        spi.writebytes([val])
    GPIO.output(CS_PIN, GPIO.HIGH)

def init_panel():
    GPIO.output(RST_PIN, GPIO.HIGH); time.sleep(0.02)
    GPIO.output(RST_PIN, GPIO.LOW);  time.sleep(0.05)
    GPIO.output(RST_PIN, GPIO.HIGH); time.sleep(0.1)

    write_cmd(0x11); time.sleep(0.12)  # Sleep Out
    write_cmd(0x36); write_data(0xE8)  # Landscape BGR
    write_cmd(0x3A); write_data(0x55)  # 16-bit RGB565
    write_cmd(0x21)                    # IPS Inversion ON
    write_cmd(0x29); time.sleep(0.05)  # Display ON

    # Window (0, 0) to (479, 319)
    write_cmd(0x2A); write_data([0x00, 0x00, 0x01, 0xDF])
    write_cmd(0x2B); write_data([0x00, 0x00, 0x01, 0x3F])

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
        raw = subprocess.check_output(["cat", "/sys/class/thermal/thermal_zone0/temp"]).decode()
        return f"{float(raw) / 1000.0:.1f}°C"
    except Exception:
        return "N/A"

init_panel()

# Create 480x320 24-bit RGB Canvas
img = Image.new("RGB", (480, 320), color=(15, 23, 42)) # Dark slate background
draw = ImageDraw.Draw(img)

# Load fonts (fallback to default if DejaVu not present)
try:
    font_large = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 32)
    font_med   = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 20)
    font_small = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 16)
except Exception:
    font_large = font_med = font_small = ImageFont.load_default()

# Header Bar
draw.rectangle([(0, 0), (480, 50)], fill=(30, 41, 59))
draw.text((20, 10), "DIETPI APPLIANCE STATUS", fill=(56, 189, 248), font=font_large)

# Cards / Containers
draw.rounded_rectangle([(20, 70), (225, 200)], radius=8, fill=(30, 41, 59), outline=(71, 85, 105))
draw.text((35, 80), "SYSTEM TIME", fill=(148, 163, 184), font=font_small)
draw.text((35, 120), time.strftime("%H:%M:%S"), fill=(255, 255, 255), font=font_large)

draw.rounded_rectangle([(255, 70), (460, 200)], radius=8, fill=(30, 41, 59), outline=(71, 85, 105))
draw.text((270, 80), "CPU TEMPERATURE", fill=(148, 163, 184), font=font_small)
draw.text((270, 120), get_cpu_temp(), fill=(74, 222, 128), font=font_large)

# Footer Status Row
draw.rounded_rectangle([(20, 220), (460, 295)], radius=8, fill=(30, 41, 59), outline=(71, 85, 105))
draw.text((35, 235), f"IP Address: {get_ip()}", fill=(241, 245, 249), font=font_med)
draw.text((35, 265), "Interface: Hardware SPI (spidev0.0 @ 16 MHz)", fill=(148, 163, 184), font=font_small)

print("Converting Pillow canvas to RGB565 byte stream...")
raw_rgb = img.tobytes()
# Fast conversion from RGB888 to RGB565
pixels_565 = bytearray(480 * 320 * 2)
j = 0
for i in range(0, len(raw_rgb), 3):
    r = raw_rgb[i]
    g = raw_rgb[i+1]
    b = raw_rgb[i+2]
    # 5 bits R, 6 bits G, 5 bits B
    val = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
    pixels_565[j]   = (val >> 8) & 0xFF
    pixels_565[j+1] = val & 0xFF
    j += 2

print("Streaming frame to panel...")
GPIO.output(CS_PIN, GPIO.LOW)
GPIO.output(DC_PIN, GPIO.LOW)
spi.writebytes([0x2C])
GPIO.output(DC_PIN, GPIO.HIGH)

# 4096-byte hardware transfers
chunk_size = 4096
for offset in range(0, len(pixels_565), chunk_size):
    spi.writebytes(pixels_565[offset:offset+chunk_size])

GPIO.output(CS_PIN, GPIO.HIGH)
print("UI rendered successfully!")
spi.close()
