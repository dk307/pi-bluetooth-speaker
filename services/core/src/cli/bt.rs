//! Bluetooth and `BlueALSA` hardware monitor and diagnostic utility.
//!
//! Provides CLI diagnostics for the Broadcom BCM43438 Bluetooth controller and the
//! `BlueALSA` audio streaming pipeline on the Raspberry Pi 3 appliance. Displays
//! systemd service states, local controller properties, connected/paired remote devices,
//! and real-time `BlueALSA` PCM audio stream telemetry.

use std::process::ExitCode;
use std::time::Duration;

use crate::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts, run_poll_loop,
    should_print_summary,
};
use crate::hardware::{
    BlueAlsaPcm, BluetoothDevice, BluetoothHardwareInfo, BluetoothStatus, BtAddress, BtDevice,
    BtError,
};

/// Default polling interval between status checks (1.0 second).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Command-line configuration parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    /// Shared polling and formatting options (`-1`, `-n`, `-i`, `-v`, `-j`).
    pub common: CommonCliOptions,
    /// Target Bluetooth adapter interface (e.g. `hci0`).
    pub adapter: Option<Box<str>>,
    /// Target specific Bluetooth device address for filtering.
    pub target_device: Option<BtAddress>,
    /// Whether to include paired but disconnected devices in human output.
    pub include_paired: bool,
    /// Optional radio power command (`true` for on, `false` for off).
    pub power: Option<bool>,
    /// Optional discoverable mode command.
    pub discoverable: Option<bool>,
    /// Optional pairable mode command.
    pub pairable: Option<bool>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            adapter: None,
            target_device: None,
            include_paired: false,
            power: None,
            discoverable: None,
            pairable: None,
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
    println!("Bluetooth & BlueALSA Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once                 Read once and exit (default)");
    println!("  -R, --repeat               Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>        Read COUNT times and exit");
    println!("  -i, --interval <SECS>      Polling interval in seconds (default: 1.0)");
    println!("  -a, --adapter <ID>         Bluetooth adapter ID (default: hci0)");
    println!("  -d, --device <MAC>         Filter for specific Bluetooth MAC address");
    println!("  -p, --paired               List all paired devices (including disconnected)");
    println!("      --power <on|off>       Set adapter radio power state");
    println!("      --discoverable <on|off> Set adapter discoverable state");
    println!("      --pairable <on|off>    Set adapter pairable state");
    println!(
        "  -v, --verbose              Display extended hardware diagnostics (human format only)"
    );
    println!("  -j, --json                 Stream readings in newline-delimited JSON");
    println!("  -V, --version              Print semantic version and exit");
    println!("  -h, --help                 Display this help text");
}

/// Parses a boolean from on/off/true/false string flags.
///
/// # Errors
///
/// Returns an error string if value is neither on nor off.
pub fn parse_on_off(val: &str, flag_name: &str) -> Result<bool, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!(
            "invalid value '{other}' for {flag_name}: expected 'on' or 'off'"
        )),
    }
}

/// Parses command-line arguments into structured [`CliOptions`].
///
/// # Errors
/// Returns a descriptive error message string if argument parsing or validation fails.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    parse_args_with_common(
        CliOptions::default(),
        args,
        |o| &mut o.common,
        |opts, arg, args| match arg {
            "-a" | "--adapter" => {
                opts.adapter = Some(next_arg_value(args, "--adapter")?.into());
                Ok(true)
            }
            "-d" | "--device" => {
                let mac_str = next_arg_value(args, "--device")?;
                opts.target_device = Some(BtAddress::parse_str(&mac_str)?);
                Ok(true)
            }
            "-p" | "--paired" => {
                opts.include_paired = true;
                Ok(true)
            }
            "--power" => {
                let val = next_arg_value(args, "--power")?;
                opts.power = Some(parse_on_off(&val, "--power")?);
                Ok(true)
            }
            "--discoverable" => {
                let val = next_arg_value(args, "--discoverable")?;
                opts.discoverable = Some(parse_on_off(&val, "--discoverable")?);
                Ok(true)
            }
            "--pairable" => {
                let val = next_arg_value(args, "--pairable")?;
                opts.pairable = Some(parse_on_off(&val, "--pairable")?);
                Ok(true)
            }
            _ => Ok(false),
        },
    )
}

/// Parses CLI arguments from environment `std::env::args()`.
///
/// # Errors
///
/// Returns an error string if an argument or flag value is invalid.
pub fn parse_cli_args() -> Result<CliParse<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

/// Diagnostic session statistics for Bluetooth monitoring iterations.
#[derive(Debug, Default)]
pub struct BtStats {
    /// Cumulative read and error counters.
    pub samples: NumericStats,
    /// Number of reads where services were operational and radio was powered.
    pub operational_reads: usize,
    /// Number of reads where audio was actively streaming via `BlueALSA`.
    pub streaming_reads: usize,
    /// Cumulative sum of connected devices across successful reads.
    pub total_connected_devices: usize,
}

impl BtStats {
    /// Records a successful Bluetooth status reading.
    pub fn record_success(&mut self, status: &BluetoothStatus) {
        self.samples.record_success_no_sample();
        if status.is_operational() {
            self.operational_reads += 1;
        }
        if status.is_streaming() {
            self.streaming_reads += 1;
        }
        self.total_connected_devices += status.connected_count();
    }

    /// Records a failed reading iteration.
    pub fn record_error(&mut self) {
        self.samples.record_error();
    }
}

/// Prints the hardware metadata banner to stdout.
pub fn print_header() {
    println!("=========================================================");
    println!("               BLUETOOTH & BLUE-ALSA STATUS              ");
    println!("=========================================================");
}

/// Prints session summary statistics to stdout.
pub fn print_session_summary(stats: &BtStats) {
    let s = &stats.samples;
    println!("\n--- Bluetooth Diagnostic Session Summary ---");
    println!("Total reads : {}", s.total_reads());
    println!("Successful  : {}", s.successful_reads());
    if s.total_reads() > 0 {
        let errors = s.errors();
        println!(
            "Read errors : {errors} ({:.2}% error rate)",
            s.error_rate_percent()
        );
    }
    println!("Radio operational reads : {}", stats.operational_reads);
    println!("Active streaming reads  : {}", stats.streaming_reads);
    if s.successful_reads() > 0 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "telemetry averages do not require exact integer precision"
        )]
        let avg_connected = stats.total_connected_devices as f64 / s.successful_reads() as f64;
        println!("Avg connected devices   : {avg_connected:.2}");
    }
}

/// Prints human-readable service status.
pub fn print_human_services(status: &BluetoothStatus) {
    let svcs = &status.services;
    println!("Bluetooth Service (bluetoothd) : {}", svcs.bluetoothd);
    println!("BlueALSA Service (bluealsa)    : {}", svcs.bluealsa);
    println!("BlueALSA Aplay (audio forward) : {}", svcs.bluealsa_aplay);
    println!("BT Auto-Trust Agent (bt-agent) : {}", svcs.bt_agent);
    println!("---------------------------------------------------------");
}

/// Prints human-readable adapter status.
pub fn print_human_adapter(status: &BluetoothStatus) {
    if let Some(ref adapter) = status.adapter {
        let pwr = if adapter.powered { "ON" } else { "OFF" };
        let disc = if adapter.discoverable { "YES" } else { "NO" };
        let pair = if adapter.pairable { "YES" } else { "NO" };
        let scan = if adapter.discovering {
            "SCANNING"
        } else {
            "IDLE"
        };

        println!(
            "Controller Name       : {} ({})",
            adapter.name, adapter.address
        );
        println!("Radio Power State     : {pwr}");
        println!("Discoverable / Pair   : {disc} / {pair}");
        println!("Scanning / Discovery  : {scan}");
    } else {
        println!("[CRITICAL] No Bluetooth controller detected.");
    }
    println!("---------------------------------------------------------");
}

/// Prints human-readable device listings.
pub fn print_human_devices(status: &BluetoothStatus, opts: &CliOptions) {
    let mut matching_devices: Vec<&BtDevice> = status.devices.iter().collect();
    if let Some(target) = opts.target_device {
        matching_devices.retain(|d| d.address == target);
    }

    let connected: Vec<&BtDevice> = matching_devices
        .iter()
        .copied()
        .filter(|d| d.connected)
        .collect();
    println!("Connected Devices ({}):", connected.len());
    if connected.is_empty() {
        println!("  [IDLE] No active Bluetooth connections");
    } else {
        for dev in connected {
            let mut meta = Vec::new();
            if let Some(rssi) = dev.rssi {
                meta.push(format!("RSSI: {rssi} dBm"));
            }
            if let Some(bat) = dev.battery_percentage {
                meta.push(format!("Battery: {bat}%"));
            }
            let meta_str = if meta.is_empty() {
                String::new()
            } else {
                format!(" | {}", meta.join(" | "))
            };
            println!(
                "  [CONNECTED] {} ({}){meta_str}",
                dev.display_name(),
                dev.address
            );
        }
    }

    if opts.include_paired {
        let paired: Vec<&BtDevice> = matching_devices
            .iter()
            .copied()
            .filter(|d| d.paired && !d.connected)
            .collect();
        if !paired.is_empty() {
            println!("Paired Standby Devices ({}):", paired.len());
            for dev in paired {
                println!("  [PAIRED] {} ({})", dev.display_name(), dev.address);
            }
        }
    }
    println!("---------------------------------------------------------");
}

/// Prints human-readable `BlueALSA` audio pipeline status.
pub fn print_human_audio_pipeline(status: &BluetoothStatus, opts: &CliOptions) {
    println!("BlueALSA Audio Pipeline:");
    let mut pcms: Vec<&BlueAlsaPcm> = status.pcms.iter().collect();
    if let Some(target) = opts.target_device {
        pcms.retain(|p| p.device_address == target);
    }

    if pcms.is_empty() {
        println!("  [STANDBY] No active audio transport streams registered.");
    } else {
        for pcm in pcms {
            let codecs_avail = if pcm.available_codecs.is_empty() {
                "N/A".to_string()
            } else {
                pcm.available_codecs.join(", ")
            };
            let delay_str = pcm.delay.as_deref().unwrap_or("N/A");
            let vol_str = pcm.volume.as_deref().unwrap_or("N/A");
            let muted_str = if pcm.muted { "YES" } else { "NO" };

            println!("  Stream State      : {}", pcm.stream_state_description());
            println!(
                "  ↳ Audio Source    : {} ({})",
                pcm.resolved_name, pcm.device_address
            );
            println!("  ↳ Transport Mode  : {}", pcm.transport);
            println!("  ↳ Active Codec    : {}", pcm.selected_codec);
            println!(
                "  ↳ Sample Rate     : {} Hz ({} Channels, {})",
                pcm.sampling_rate, pcm.channels, pcm.format
            );
            println!("  ↳ Hardware Volume : {vol_str} (Muted: {muted_str})");
            println!("  ↳ Audio Latency   : {delay_str}");
            println!("  ↳ Codecs Allowed  : {codecs_avail}");
            println!();
        }
    }
}

/// Prints human-readable hardware info.
pub fn print_human_hardware_info(hw: &BluetoothHardwareInfo) {
    println!("---------------------------------------------------------");
    println!("Hardware & Driver Specifications:");
    println!("  Chip Model        : {}", hw.chip);
    println!("  Transport Bus     : {}", hw.bus);
    println!("  UART Baud Rate    : {} baud", hw.baud_rate);
    println!("  Firmware File     : {}", hw.firmware);
    println!("  Linux Driver      : {}", hw.driver);
    println!("  Default HCI Dev   : {}", hw.default_controller);
    let c = &hw.controller;
    if c.is_known() {
        println!(
            "  Controller Live Probe (hciconfig {}):",
            c.adapter.as_deref().unwrap_or(&hw.default_controller)
        );
        print_opt("  BD Address        : ", c.bd_address.as_deref());
        print_opt("  HCI Version       : ", c.hci_version.as_deref());
        print_opt("  HCI Revision      : ", c.hci_revision.as_deref());
        print_opt("  LMP Version       : ", c.lmp_version.as_deref());
        print_opt("  LMP Subversion    : ", c.lmp_subversion.as_deref());
        print_opt("  Manufacturer      : ", c.manufacturer.as_deref());
        print_opt("  Bus Type          : ", c.bus_type.as_deref());
        print_opt("  Firmware Version  : ", c.firmware_version.as_deref());
    } else {
        println!("  Controller Live Probe: N/A");
    }
}

/// Prints an optional label and value.
pub fn print_opt(label: &str, value: Option<&str>) {
    println!("{label}{}", value.unwrap_or("N/A"));
}

/// Prints a single complete status snapshot.
pub fn print_human_single_status(status: &BluetoothStatus, opts: &CliOptions) {
    print_human_services(status);
    print_human_adapter(status);
    print_human_devices(status, opts);
    print_human_audio_pipeline(status, opts);
    if opts.common.verbose {
        print_human_hardware_info(&status.hardware);
    }
    println!("=========================================================");
}

/// Formats and outputs Bluetooth status as JSON.
pub fn output_json_status(timestamp: &str, status: &BluetoothStatus, opts: &CliOptions) {
    let mut devices: Vec<&BtDevice> = status.devices.iter().collect();
    let mut pcms: Vec<&BlueAlsaPcm> = status.pcms.iter().collect();

    if let Some(target) = opts.target_device {
        devices.retain(|d| d.address == target);
        pcms.retain(|p| p.device_address == target);
    }

    let obj = serde_json::json!({
        "timestamp": timestamp,
        "is_operational": status.is_operational(),
        "services": {
            "bluetoothd": status.services.bluetoothd.description(),
            "bluealsa": status.services.bluealsa.description(),
            "bluealsa_aplay": status.services.bluealsa_aplay.description(),
            "bt_agent": status.services.bt_agent.description(),
        },
        "adapter": status.adapter.as_ref().map(|a| serde_json::json!({
            "id": a.id,
            "address": a.address.to_string(),
            "name": a.name,
            "alias": a.alias,
            "powered": a.powered,
            "discoverable": a.discoverable,
            "pairable": a.pairable,
            "discovering": a.discovering,
            "class": a.class,
            "modalias": a.modalias,
        })),
        "devices": devices.into_iter().map(|d| serde_json::json!({
            "address": d.address.to_string(),
            "name": d.name,
            "alias": d.alias,
            "connected": d.connected,
            "paired": d.paired,
            "trusted": d.trusted,
            "blocked": d.blocked,
            "rssi": d.rssi,
            "battery_percentage": d.battery_percentage,
            "icon": d.icon,
            "uuids": d.uuids,
        })).collect::<Vec<_>>(),
        "pcms": pcms.into_iter().map(|p| serde_json::json!({
            "pcm_path": p.pcm_path,
            "device_address": p.device_address.to_string(),
            "resolved_name": p.resolved_name,
            "transport": p.transport,
            "running": p.running,
            "stream_state": p.stream_state_description(),
            "selected_codec": p.selected_codec,
            "available_codecs": p.available_codecs,
            "sampling_rate": p.sampling_rate,
            "channels": p.channels,
            "format": p.format,
            "volume": p.volume,
            "muted": p.muted,
            "delay": p.delay,
        })).collect::<Vec<_>>(),
        "playback": status.playback.as_ref().map(|p| serde_json::json!({
            "status": p.status,
            "title": p.title,
            "artist": p.artist,
            "album": p.album,
            "position_ms": p.position_ms,
            "duration_ms": p.duration_ms,
            "is_playing": p.is_playing(),
            "device_address": p.device_address.map(|a| a.to_string()),
            "device_name": p.device_name,
        })),
        "hardware": {
            "chip": status.hardware.chip,
            "bus": status.hardware.bus,
            "baud_rate": status.hardware.baud_rate,
            "firmware": status.hardware.firmware,
            "driver": status.hardware.driver,
            "default_controller": status.hardware.default_controller,
            "controller": {
                "adapter": status.hardware.controller.adapter,
                "bd_address": status.hardware.controller.bd_address,
                "hci_version": status.hardware.controller.hci_version,
                "hci_revision": status.hardware.controller.hci_revision,
                "lmp_version": status.hardware.controller.lmp_version,
                "lmp_subversion": status.hardware.controller.lmp_subversion,
                "manufacturer": status.hardware.controller.manufacturer,
                "bus_type": status.hardware.controller.bus_type,
                "firmware_version": status.hardware.controller.firmware_version,
            }
        }
    });
    println!("{obj}");
}

/// Formats and outputs an error as JSON.
pub fn output_json_error(timestamp: &str, err: &BtError) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "error": err.summary(),
        })
    );
}

/// Handles a single Bluetooth monitoring iteration.
pub fn handle_bt_iteration(
    device: &BluetoothDevice,
    opts: &CliOptions,
    timestamp: &str,
    session_stats: &mut BtStats,
) {
    match device.status_for_adapter(opts.adapter.as_deref()) {
        Ok(status) => {
            session_stats.record_success(&status);
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_status(timestamp, &status, opts);
                }
                OutputFormat::Human => {
                    if opts.common.max_count == Some(1) {
                        print_human_single_status(&status, opts);
                    } else {
                        let radio_str = match &status.adapter {
                            Some(a) if a.powered => "ON ",
                            Some(_) => "OFF",
                            None => "N/A",
                        };
                        let audio_str = match status.active_pcm() {
                            Some(p) => format!("STREAMING ({})", p.selected_codec),
                            None => "IDLE".to_string(),
                        };
                        println!(
                            "[{timestamp}] Status: {:5} | Radio: {:3} | Devices: {:2} connected | Audio: {audio_str}",
                            if status.is_operational() {
                                "OK"
                            } else {
                                "WARN"
                            },
                            radio_str,
                            status.connected_count(),
                        );
                        if opts.common.verbose {
                            print_human_single_status(&status, opts);
                        }
                    }
                }
            }
        }
        Err(err) => {
            session_stats.record_error();
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(timestamp, &err);
                }
                OutputFormat::Human => {
                    eprintln!("[{timestamp}] Read error: {err}");
                }
            }
        }
    }
}

/// Applies requested radio state changes to the Bluetooth adapter.
///
/// # Errors
///
/// Returns an error if applying any adapter control setting fails.
pub fn apply_controls(device: &BluetoothDevice, opts: &CliOptions) -> Result<(), BtError> {
    if let Some(pwr) = opts.power {
        device.set_powered_for_adapter(opts.adapter.as_deref(), pwr)?;
    }
    if let Some(disc) = opts.discoverable {
        device.set_discoverable_for_adapter(opts.adapter.as_deref(), disc)?;
    }
    if let Some(pair) = opts.pairable {
        device.set_pairable_for_adapter(opts.adapter.as_deref(), pair)?;
    }
    Ok(())
}

/// Runs the Bluetooth diagnostic utility application with the provided device handle and options.
#[must_use]
pub fn run_app(device: &BluetoothDevice, opts: &CliOptions) -> ExitCode {
    if let Err(err) = apply_controls(device, opts) {
        if opts.common.format == OutputFormat::Json {
            output_json_error(&format_timestamp(), &err);
        } else {
            eprintln!("Failed to apply Bluetooth control settings: {err}");
        }
        return ExitCode::FAILURE;
    }

    if opts.common.format == OutputFormat::Human {
        print_header();
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        }
    }

    let mut stats = BtStats::default();

    run_poll_loop(opts.common.interval, |timestamp| {
        handle_bt_iteration(device, opts, timestamp, &mut stats);
        opts.common
            .max_count
            .is_some_and(|m| stats.samples.total_reads() >= m)
    });

    if should_print_summary(
        opts.common.format,
        opts.common.max_count,
        stats.samples.total_reads(),
    ) {
        print_session_summary(&stats);
    }

    exit_code_for_reads(
        stats.samples.total_reads(),
        stats.samples.successful_reads(),
    )
}

/// Standalone CLI binary entrypoint.
pub fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "bt-info", print_usage) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let device = match BluetoothDevice::new() {
        Ok(d) => d,
        Err(err) => {
            if opts.common.format == OutputFormat::Json {
                output_json_error(&format_timestamp(), &err);
            } else {
                eprintln!("Failed to initialize Bluetooth D-Bus subsystem client: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    run_app(&device, &opts)
}
