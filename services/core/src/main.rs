//! Central speaker daemon and hardware orchestration service for Raspberry Pi 3.

use speaker_core::hardware::TempSensor;

fn main() {
    let temp_sensor = TempSensor::new();

    println!("============================================================");
    println!("  Speaker Core Service is running on Raspberry Pi 3!");
    println!("  Target: ARM Cortex-A53 (aarch64)");
    println!("  Thermal Sensor: {}", temp_sensor.gpio_info());
    println!("============================================================");
}
