#!/usr/bin/env python3
import time
import RPi.GPIO as GPIO

# Verified BCM Pin Assignments
PIN_A  = 25  # Physical Pin 2 (Quadrature Phase A)
PIN_B  = 24  # Physical Pin 18 (Quadrature Phase B)[cite: 4]
PIN_SW = 22  # Physical Pin 15 (Push Switch - Pin 3 to Pin 5/3.3V)[cite: 4, 6]
LED_R  = 20  # Physical Pin 38 (Red Cathode via 330Ω)[cite: 4]
LED_G  = 16  # Physical Pin 36 (Green Cathode via 180Ω)[cite: 5]
LED_B  = 12  # Physical Pin 32 (Blue Cathode via 180Ω)[cite: 5, 7]

GPIO.setwarnings(False)
GPIO.setmode(GPIO.BCM)

# 1. Rotary inputs (Active-LOW contacts to GND -> Internal Pull-Up)
GPIO.setup(PIN_A, GPIO.IN, pull_up_down=GPIO.PUD_UP)
GPIO.setup(PIN_B, GPIO.IN, pull_up_down=GPIO.PUD_UP)

# 2. Push Switch (Active-HIGH contact to 3.3V -> Internal Pull-Down)
GPIO.setup(PIN_SW, GPIO.IN, pull_up_down=GPIO.PUD_DOWN)

# 3. LED Outputs (Common Anode on 3.3V -> Sinking Active-LOW)
for pin in (LED_R, LED_G, LED_B):
    GPIO.setup(pin, GPIO.OUT, initial=GPIO.HIGH)

def set_leds(r=False, g=False, b=False):
    """Active-LOW driver: True pulls to 0V (ON), False leaves HIGH (OFF)."""
    GPIO.output(LED_R, GPIO.LOW if r else GPIO.HIGH)
    GPIO.output(LED_G, GPIO.LOW if g else GPIO.HIGH)
    GPIO.output(LED_B, GPIO.LOW if b else GPIO.HIGH)

print("==================================================")
print("     TOP-UP EC12 ENCODER FULL SYSTEM TEST         ")
print("==================================================")

# --------------------------------------------------------
# Phase 1: Automated RGB Channel Verification
# --------------------------------------------------------
print("\n[Phase 1/2] Running RGB LED Sequence...")
colors = [
    ("Red",     True,  False, False),
    ("Green",   False, True,  False),
    ("Blue",    False, False, True),
    ("Yellow",  True,  True,  False),
    ("Cyan",    False, True,  True),
    ("Magenta", True,  False, True),
    ("White",   True,  True,  True),
    ("OFF",     False, False, False),
]

for name, r, g, b in colors:
    print(f"  -> Testing Color: {name:<8}")
    set_leds(r, g, b)
    time.sleep(3.0)

# --------------------------------------------------------
# Phase 2: Interactive Real-Time Input Test
# --------------------------------------------------------
print("\n[Phase 2/2] Interactive Input Test Ready")
print("  • Turn CW (Right)     -> Prints [+] and flashes Green")
print("  • Turn CCW (Left)     -> Prints [-] and flashes Blue")
print("  • Push Switch Click   -> Prints [CLICK] and illuminates White")
print("  • Press Ctrl+C to terminate.\n")

counter = 0
last_state = (GPIO.input(PIN_A) << 1) | GPIO.input(PIN_B)
sw_pressed = False

try:
    while True:
        # Active-HIGH Switch Detection
        sw_current = (GPIO.input(PIN_SW) == GPIO.HIGH)
        if sw_current and not sw_pressed:
            sw_pressed = True
            print(">>> [CLICK] PUSH SWITCH PRESSED <<<")
            set_leds(True, True, True)  # Solid White while pressed
        elif not sw_current and sw_pressed:
            sw_pressed = False
            print("--- [RELEASE] PUSH SWITCH RELEASED ---")
            set_leds(False, False, False)

        # Quadrature Rotation Detection (Full-step transitions)
        current_state = (GPIO.input(PIN_A) << 1) | GPIO.input(PIN_B)
        if current_state != last_state:
            if (last_state == 0b00 and current_state == 0b01) or \
               (last_state == 0b11 and current_state == 0b10):
                counter += 1
                print(f"[+] CW  (Turn Right) | Position: {counter:4d}")
                if not sw_pressed:
                    set_leds(False, True, False)  # Green flash
                    time.sleep(0.04)
                    set_leds(False, False, False)
            elif (last_state == 0b00 and current_state == 0b10) or \
                 (last_state == 0b11 and current_state == 0b01):
                counter -= 1
                print(f"[-] CCW (Turn Left)  | Position: {counter:4d}")
                if not sw_pressed:
                    set_leds(False, False, True)  # Blue flash
                    time.sleep(0.04)
                    set_leds(False, False, False)

            last_state = current_state
            time.sleep(0.002)

except KeyboardInterrupt:
    print("\nCleaning up GPIO pins...")
    set_leds(False, False, False)
    GPIO.cleanup()
    print("Test finished successfully.")
