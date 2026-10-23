//! DS18B20 1-Wire temperature monitor utility.
//!
//! Rust replacement for `read_temp.py` using [`speaker_core::hardware::TempSensor`].
//! Displays the Raspberry Pi GPIO header pin mapping, discovers all attached
//! `28-*` DS18B20 sensors on the 1-Wire bus, and continuously prints temperature
//! readings in Celsius and Fahrenheit.

use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use speaker_core::hardware::TempSensor;

/// Polling interval between temperature reads in continuous mode.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn format_timestamp() -> String {
    let total_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let hours = (total_secs / 3600) % 24;
    let minutes = (total_secs / 60) % 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn parse_max_iterations() -> Option<usize> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" | "-1" => return Some(1),
            "--count" | "-n" => {
                if let Some(val) = args.next() {
                    if let Ok(count) = val.parse::<usize>() {
                        return Some(count);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn main() -> ExitCode {
    let sensor = TempSensor::new();
    let gpio = sensor.gpio_info();
    let max_iterations = parse_max_iterations();

    println!("==================================================");
    println!("      DS18B20 1-WIRE TEMPERATURE MONITOR          ");
    println!("==================================================");
    println!("Hardware GPIO Info:");
    println!("  • Signal       : {}", gpio.signal_name);
    println!(
        "  • GPIO Pin     : {} ({})",
        gpio.bcm_pin, gpio.physical_pin
    );
    println!("  • Bus Protocol : {}", gpio.bus_protocol);
    println!("  • DT Overlay   : {}", gpio.electrical_config);
    println!(
        "  • Sysfs Path   : {}/28-*/w1_slave",
        sensor.sysfs_base_path().display()
    );
    println!("--------------------------------------------------");

    let devices = match sensor.discover_sensors() {
        Ok(devs) => devs,
        Err(err) => {
            eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("Discovered {} sensor(s):", devices.len());
    for dev in &devices {
        println!("  • {dev}");
    }
    if max_iterations.is_none() {
        println!("Press Ctrl+C to exit.\n");
    } else {
        println!();
    }

    let mut iteration = 0usize;
    loop {
        let timestamp = format_timestamp();
        for dev in &devices {
            match sensor.read_sensor(dev) {
                Ok(reading) => {
                    println!("[{timestamp}] {reading}");
                }
                Err(err) => {
                    println!("[{timestamp}] [{dev}] Read error / CRC check failed: {err}");
                }
            }
        }

        iteration += 1;
        if let Some(limit) = max_iterations {
            if iteration >= limit {
                break;
            }
        }

        thread::sleep(POLL_INTERVAL);
    }

    ExitCode::SUCCESS
}
