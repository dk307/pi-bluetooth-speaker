//! Bluetooth and `BlueALSA` hardware monitor and diagnostic utility.
//!
//! Provides CLI diagnostics for the Broadcom BCM43438 Bluetooth controller and the
//! `BlueALSA` audio streaming pipeline on the Raspberry Pi 3 appliance. Displays
//! systemd service states, local controller properties, connected/paired remote devices,
//! and real-time `BlueALSA` PCM audio stream telemetry.
//!
//! Conforms to Microsoft Pragmatic Rust Guidelines:
//! - Composes [`CommonCliOptions`] and [`NumericStats`] from `speaker_core::diag`
//! - Supports modular testing via testable [`run_app`] entrypoint with [`BluetoothDeviceMock`]
//! - Formats human console diagnostics and streaming newline-delimited JSON (`NDJSON`)

use std::process::ExitCode;
use std::time::Duration;

use speaker_core::diag::{
    CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads, format_timestamp,
    next_arg_value, parse_args_with_common, resolve_cli_opts, run_poll_loop, should_print_summary,
};
use speaker_core::hardware::{
    BlueAlsaPcm, BluetoothDevice, BluetoothHardwareInfo, BluetoothStatus, BtAddress, BtDevice,
    BtError,
};

/// Default polling interval between status checks (1.0 second).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

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

fn print_usage(bin_name: &str) {
    println!("Bluetooth & BlueALSA Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once                 Read once and exit");
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
    println!("  -h, --help                 Display this help text");
}

fn parse_on_off(val: &str, flag_name: &str) -> Result<bool, String> {
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
pub fn parse_cli_args_from<I>(args: I) -> Result<Option<CliOptions>, String>
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

fn parse_cli_args() -> Result<Option<CliOptions>, String> {
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

fn print_header() {
    println!("=========================================================");
    println!("               BLUETOOTH & BLUE-ALSA STATUS              ");
    println!("=========================================================");
}

fn print_session_summary(stats: &BtStats) {
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

fn print_human_services(status: &BluetoothStatus) {
    let svcs = &status.services;
    println!("Bluetooth Service (bluetoothd) : {}", svcs.bluetoothd);
    println!("BlueALSA Service (bluealsa)    : {}", svcs.bluealsa);
    println!("BlueALSA Aplay (audio forward) : {}", svcs.bluealsa_aplay);
    println!("BT Auto-Trust Agent (bt-agent) : {}", svcs.bt_agent);
    println!("---------------------------------------------------------");
}

fn print_human_adapter(status: &BluetoothStatus) {
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

fn print_human_devices(status: &BluetoothStatus, opts: &CliOptions) {
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

fn print_human_audio_pipeline(status: &BluetoothStatus, opts: &CliOptions) {
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

fn print_human_hardware_info(hw: &BluetoothHardwareInfo) {
    println!("---------------------------------------------------------");
    println!("Hardware & Driver Specifications:");
    println!("  Chip Model        : {}", hw.chip);
    println!("  Transport Bus     : {}", hw.bus);
    println!("  UART Baud Rate    : {} baud", hw.baud_rate);
    println!("  Firmware File     : {}", hw.firmware);
    println!("  Linux Driver      : {}", hw.driver);
    println!("  Default HCI Dev   : {}", hw.default_controller);
}

fn print_human_single_status(status: &BluetoothStatus, opts: &CliOptions) {
    print_human_services(status);
    print_human_adapter(status);
    print_human_devices(status, opts);
    print_human_audio_pipeline(status, opts);
    if opts.common.verbose {
        print_human_hardware_info(&status.hardware);
    }
    println!("=========================================================");
}

fn output_json_status(timestamp: &str, status: &BluetoothStatus, opts: &CliOptions) {
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
        "hardware": {
            "chip": status.hardware.chip,
            "bus": status.hardware.bus,
            "baud_rate": status.hardware.baud_rate,
            "firmware": status.hardware.firmware,
            "driver": status.hardware.driver,
            "default_controller": status.hardware.default_controller,
        }
    });
    println!("{obj}");
}

fn output_json_error(timestamp: &str, err: &BtError) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "error": err.summary(),
        })
    );
}

fn handle_bt_iteration(
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

fn apply_controls(device: &BluetoothDevice, opts: &CliOptions) -> Result<(), BtError> {
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

fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "read_bt", print_usage) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_args_defaults_and_options() {
        let args: Vec<String> = vec![];
        let opts = parse_cli_args_from(args).unwrap().unwrap();
        assert_eq!(opts.common.interval, DEFAULT_POLL_INTERVAL);
        assert!(opts.common.max_count.is_none());
        assert_eq!(opts.common.format, OutputFormat::Human);
        assert!(!opts.include_paired);

        let args = vec![
            "--once".into(),
            "-a".into(),
            "hci0".into(),
            "-d".into(),
            "AA:BB:CC:DD:EE:FF".into(),
            "-p".into(),
            "-v".into(),
            "-j".into(),
            "--power".into(),
            "on".into(),
            "--discoverable".into(),
            "off".into(),
            "--pairable".into(),
            "yes".into(),
        ];
        let opts = parse_cli_args_from(args).unwrap().unwrap();
        assert_eq!(opts.common.max_count, Some(1));
        assert_eq!(opts.adapter.as_deref(), Some("hci0"));
        assert_eq!(
            opts.target_device,
            Some(BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]))
        );
        assert!(opts.include_paired);
        assert!(opts.common.verbose);
        assert_eq!(opts.common.format, OutputFormat::Json);
        assert_eq!(opts.power, Some(true));
        assert_eq!(opts.discoverable, Some(false));
        assert_eq!(opts.pairable, Some(true));

        assert!(
            parse_cli_args_from(vec!["--help".into()])
                .unwrap()
                .is_none()
        );
        parse_cli_args_from(vec!["--power".into(), "invalid".into()]).unwrap_err();
        parse_cli_args_from(vec!["--device".into(), "bad-mac".into()]).unwrap_err();
    }

    #[test]
    fn bt_stats_session_accounting() {
        let mut stats = BtStats::default();
        let (device, _) = BluetoothDevice::new_mocked();
        let reading = device.status().unwrap();

        stats.record_success(&reading);
        assert_eq!(stats.samples.total_reads(), 1);
        assert_eq!(stats.samples.successful_reads(), 1);
        assert_eq!(stats.operational_reads, 1);
        assert_eq!(stats.streaming_reads, 1);
        assert_eq!(stats.total_connected_devices, 1);

        stats.record_error();
        assert_eq!(stats.samples.total_reads(), 2);
        assert_eq!(stats.samples.successful_reads(), 1);
        assert_eq!(stats.samples.errors(), 1);
    }

    #[test]
    fn run_app_once_human_and_json() {
        let (device, _) = BluetoothDevice::new_mocked();
        let mut opts = CliOptions::default();
        opts.common.max_count = Some(1);
        opts.common.verbose = true;
        opts.include_paired = true;

        let code = run_app(&device, &opts);
        assert_eq!(code, ExitCode::SUCCESS);

        opts.common.format = OutputFormat::Json;
        let code_json = run_app(&device, &opts);
        assert_eq!(code_json, ExitCode::SUCCESS);
    }

    #[test]
    fn run_app_multi_iterations_and_summary() {
        let (device, _) = BluetoothDevice::new_mocked();
        let mut opts = CliOptions::default();
        opts.common.max_count = Some(2);
        opts.common.interval = Duration::from_millis(1);

        let code = run_app(&device, &opts);
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn print_usage_runs_cleanly() {
        print_usage("read_bt");
    }

    #[test]
    fn parse_on_off_variations() {
        assert!(parse_on_off("on", "--power").unwrap());
        assert!(parse_on_off("true", "--power").unwrap());
        assert!(parse_on_off("1", "--power").unwrap());
        assert!(parse_on_off("yes", "--power").unwrap());

        assert!(!parse_on_off("off", "--power").unwrap());
        assert!(!parse_on_off("false", "--power").unwrap());
        assert!(!parse_on_off("0", "--power").unwrap());
        assert!(!parse_on_off("no", "--power").unwrap());

        parse_on_off("unknown", "--power").unwrap_err();
    }

    #[test]
    fn print_human_edge_cases_and_filters() {
        let (device, mock) = BluetoothDevice::new_mocked();
        let target_addr = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        let other_addr = BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

        let dev_no_meta = BtDevice {
            address: other_addr,
            name: "Other".into(),
            alias: "".into(),
            connected: false,
            paired: true,
            trusted: false,
            blocked: false,
            rssi: None,
            battery_percentage: None,
            icon: None,
            uuids: Box::default(),
        };
        mock.add_device(dev_no_meta);

        let mut opts = CliOptions::default();
        opts.common.max_count = Some(1);
        opts.include_paired = true;
        opts.common.verbose = true;
        opts.target_device = Some(target_addr);

        let status = device.status().unwrap();
        print_human_single_status(&status, &opts);
        output_json_status("12:00:00", &status, &opts);

        // Test with no adapter and no pcms
        mock.set_adapter(None);
        mock.clear_pcms();
        mock.clear_devices();
        let empty_status = device.status().unwrap();
        opts.target_device = None;
        print_human_single_status(&empty_status, &opts);
    }

    #[test]
    fn handle_bt_iteration_human_and_json_branches() {
        let (device, mock) = BluetoothDevice::new_mocked();
        let mut stats = BtStats::default();

        let mut multi_opts = CliOptions::default();
        multi_opts.common.max_count = Some(2);
        multi_opts.common.verbose = true;
        multi_opts.common.format = OutputFormat::Human;

        handle_bt_iteration(&device, &multi_opts, "12:00:00", &mut stats);

        // Multi iteration with radio off
        mock.set_powered(false);
        handle_bt_iteration(&device, &multi_opts, "12:00:01", &mut stats);

        // Multi iteration with no adapter
        mock.set_adapter(None);
        handle_bt_iteration(&device, &multi_opts, "12:00:02", &mut stats);

        // Iteration error in human and JSON format
        mock.set_fault(Some("D-Bus failure".into()));
        handle_bt_iteration(&device, &multi_opts, "12:00:03", &mut stats);

        multi_opts.common.format = OutputFormat::Json;
        handle_bt_iteration(&device, &multi_opts, "12:00:04", &mut stats);

        assert_eq!(stats.samples.errors(), 2);
    }

    #[test]
    fn print_session_summary_with_and_without_errors() {
        let mut stats = BtStats::default();
        print_session_summary(&stats);

        let (device, _) = BluetoothDevice::new_mocked();
        stats.record_success(&device.status().unwrap());
        stats.record_error();
        print_session_summary(&stats);
    }

    #[test]
    fn run_app_controls_and_filters() {
        let (device, _) = BluetoothDevice::new_mocked();
        let mut opts = CliOptions::default();
        opts.common.max_count = Some(1);
        opts.power = Some(true);
        opts.discoverable = Some(false);
        opts.pairable = Some(true);

        let code = run_app(&device, &opts);
        assert_eq!(code, ExitCode::SUCCESS);

        opts.common.format = OutputFormat::Json;
        let code_json = run_app(&device, &opts);
        assert_eq!(code_json, ExitCode::SUCCESS);
    }

    #[test]
    fn run_app_adapter_targeting() {
        let (device, _) = BluetoothDevice::new_mocked();
        let mut opts = CliOptions::default();
        opts.common.max_count = Some(1);
        opts.adapter = Some("hci0".into());
        let code = run_app(&device, &opts);
        assert_eq!(code, ExitCode::SUCCESS);

        opts.adapter = Some("hci1".into());
        let mut stats = BtStats::default();
        handle_bt_iteration(&device, &opts, "12:00:00", &mut stats);
        assert_eq!(stats.samples.errors(), 1);
    }
}
