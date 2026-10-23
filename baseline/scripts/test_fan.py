import RPi.GPIO as GPIO
import time

# Hardware pin definitions from "Speaker Build" layout
PWM_PIN = 13
TACH_PIN = 6
PULSES_PER_REV = 2

# Global variables for the interrupt counter
pulse_count = 0
start_time = 0

# Interrupt callback function to count tachometer pulses
def tachometer_callback(channel):
    global pulse_count
    pulse_count += 1

def setup():
    GPIO.setwarnings(False)
    GPIO.setmode(GPIO.BCM)
    
    # Configure PWM output
    GPIO.setup(PWM_PIN, GPIO.OUT)
    global fan_pwm
    # Using 100Hz as standard RPi.GPIO software PWM struggles with 25kHz, 
    # but 100Hz is perfectly sufficient for testing 4-pin fans.
    fan_pwm = GPIO.PWM(PWM_PIN, 100) 
    fan_pwm.start(0)

    # Configure Tachometer input
    # (Relies on your physical 10k pull-up resistor to 3.3V)
    GPIO.setup(TACH_PIN, GPIO.IN)
    
    # Attach hardware interrupt to trigger on falling voltage edges
    GPIO.add_event_detect(TACH_PIN, GPIO.FALLING, callback=tachometer_callback)

def test_fan():
    global pulse_count, start_time
    
    print("Starting Noctua Fan Test...")
    print("Press Ctrl+C to stop.\n")
    
    test_speeds = [30, 50, 70, 100] # Duty cycles in %
    
    try:
        for speed in test_speeds:
            print(f"--- Ramping to {speed}% Speed ---")
            fan_pwm.ChangeDutyCycle(speed)
            
            # Allow 3 seconds for the physical fan motor to stabilize
            time.sleep(3)
            
            # Measure RPM over a 5-second window
            pulse_count = 0
            start_time = time.time()
            time.sleep(5)
            
            # Calculate RPM: (Pulses / 2) * (60 / seconds)
            elapsed_time = time.time() - start_time
            rpm = (pulse_count / PULSES_PER_REV) * (60 / elapsed_time)
            
            print(f"Measured RPM at {speed}%: {int(rpm)} RPM\n")

    except KeyboardInterrupt:
        print("\nTest stopped by user.")
    finally:
        print("Cleaning up GPIO state...")
        fan_pwm.stop()
        GPIO.cleanup()

if __name__ == '__main__':
    setup()
    test_fan()
