//! `CamillaDSP` audio processing engine diagnostic and status utility.
//!
//! Provides CLI diagnostics for the `CamillaDSP` ALSA crossover engine on the
//! Raspberry Pi 3 appliance. Displays systemd service states, connection properties,
//! engine state, CPU load, rate adjust ratio, buffer fill levels, master volume/mute,
//! digital clipped sample counters, and real-time dBFS level meters for capture and
//! playback channels.

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use crate::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts, run_poll_loop,
    should_print_summary,
};
use crate::hardware::{
    DEFAULT_DSP_HOST, DEFAULT_DSP_PORT, DspDevice, DspError, DspHardwareInfo, DspHost, DspPort,
    DspStatus, format_dbfs_meter, format_load_meter,
};

/// Default polling interval between status checks (1.0 second).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Command-line configuration parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    /// Shared polling and formatting options (`-1`, `-R`, `-n`, `-i`, `-v`, `-j`).
    pub common: CommonCliOptions,
    /// Target `CamillaDSP` WebSocket host address.
    pub host: DspHost,
    /// Target `CamillaDSP` WebSocket TCP port number.
    pub port: DspPort,
    /// Whether to reset the digital clipped samples counter.
    pub reset_clipped: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            host: DspHost::default(),
            port: DspPort::default(),
            reset_clipped: false,
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
    println!("CamillaDSP Pipeline Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once                 Read once and exit (default)");
    println!("  -R, --repeat               Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>        Read COUNT times and exit");
    println!("  -i, --interval <SECS>      Polling interval in seconds (default: 1.0)");
    println!("  -H, --host <HOST>          WebSocket host address (default: {DEFAULT_DSP_HOST})");
    println!("  -p, --port <PORT>          WebSocket port number (default: {DEFAULT_DSP_PORT})");
    println!("  -c, --reset-clipped        Reset the digital clipped samples counter and exit");
    println!(
        "  -v, --verbose              Display extended hardware diagnostics (human format only)"
    );
    println!("  -j, --json                 Stream readings in newline-delimited JSON");
    println!("  -V, --version              Print semantic version and exit");
    println!("  -h, --help                 Display this help text");
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
            "-H" | "--host" => {
                let val = next_arg_value(args, "--host")?;
                opts.host = DspHost::parse_str(&val)?;
                Ok(true)
            }
            "-p" | "--port" => {
                let val = next_arg_value(args, "--port")?;
                opts.port = DspPort::parse_str(&val)?;
                Ok(true)
            }
            "-c" | "--reset-clipped" => {
                opts.reset_clipped = true;
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

/// Diagnostic session statistics for `CamillaDSP` status readings.
#[derive(Debug, Default)]
pub struct DspStats {
    /// Read success/failure counter.
    pub samples: NumericStats,
    /// CPU processing load percentage statistics.
    pub load_stats: NumericStats,
    /// Buffer fill level sample statistics.
    pub buffer_stats: NumericStats,
    /// Maximum peak dBFS observed for each capture channel.
    pub max_capture_peaks: Vec<f32>,
    /// Maximum peak dBFS observed for each playback channel.
    pub max_playback_peaks: Vec<f32>,
    /// Maximum clipped samples counter observed.
    pub max_clipped_samples: u64,
    /// Count of readings where clipping was present.
    pub clipped_reads: usize,
}

impl DspStats {
    /// Records a successful status read.
    pub fn record_success(&mut self, status: &DspStatus) {
        self.samples.record_success_no_sample();
        if status.processing_load.is_finite() {
            self.load_stats.record_success(status.processing_load);
        }
        let buf_f32 = status.buffer_level as f32;
        if buf_f32.is_finite() {
            self.buffer_stats.record_success(buf_f32);
        }

        if status.clipped_samples > self.max_clipped_samples {
            self.max_clipped_samples = status.clipped_samples;
        }
        if status.clipped_samples > 0 {
            self.clipped_reads += 1;
        }

        // Track max peaks
        update_max_peaks(&mut self.max_capture_peaks, &status.capture.peaks);
        update_max_peaks(&mut self.max_playback_peaks, &status.playback.peaks);
    }

    /// Records a failed read iteration.
    pub fn record_error(&mut self) {
        self.samples.record_error();
    }
}

/// Updates destination vector with the element-wise maximum peaks.
pub fn update_max_peaks(dst: &mut Vec<f32>, src: &[f32]) {
    if dst.len() != src.len() {
        *dst = src.to_vec();
        return;
    }
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        if *s > *d {
            *d = *s;
        }
    }
}

/// Prints the hardware metadata banner to stdout.
pub fn print_header(hw: &DspHardwareInfo, host: &DspHost, port: DspPort) {
    println!("=========================================================");
    println!("               CAMILLADSP PIPELINE STATUS                ");
    println!("=========================================================");
    println!("Pipeline Hardware Info:");
    println!("  • Engine       : CamillaDSP ALSA Crossover Pipeline");
    println!("  • WebSocket    : {}:{}", host.as_str(), port.as_u16());
    println!("  • Service Unit : {}", hw.service_unit);
    println!("  • Config File  : {}", hw.config_path);
    println!("  • Log File     : {}", hw.log_path);
    println!("  • Capture Dev  : {}", hw.capture_device);
    println!("  • Playback Dev : {}", hw.playback_device);
    println!(
        "  • Crossover    : {} Hz (Sub LPF / Coax HPF) | Infrasonic: {} Hz",
        hw.crossover_freq_hz, hw.infrasonic_hpf_hz
    );
    println!("---------------------------------------------------------");
}

/// Prints session summary statistics to stdout.
pub fn print_session_summary(stats: &DspStats) {
    let s = &stats.samples;
    println!("\n--- CamillaDSP Diagnostic Session Summary ---");
    println!("Total reads     : {}", s.total_reads());
    println!("Successful      : {}", s.successful_reads());
    if s.total_reads() > 0 {
        let errors = s.errors();
        println!(
            "Read errors     : {errors} ({:.2}% error rate)",
            s.error_rate_percent()
        );
    }
    if stats.load_stats.successful_reads() > 0 {
        println!(
            "Processing Load : min: {:.2}%, max: {:.2}%, avg: {:.2}%",
            stats.load_stats.min().unwrap_or(0.0),
            stats.load_stats.max().unwrap_or(0.0),
            stats.load_stats.average_f32().unwrap_or(0.0)
        );
    }
    if stats.buffer_stats.successful_reads() > 0 {
        println!(
            "Buffer Fill     : min: {:.0}, max: {:.0}, avg: {:.0} samples",
            stats.buffer_stats.min().unwrap_or(0.0),
            stats.buffer_stats.max().unwrap_or(0.0),
            stats.buffer_stats.average_f32().unwrap_or(0.0)
        );
    }
    if !stats.max_capture_peaks.is_empty() {
        let peaks_str: Vec<String> = stats
            .max_capture_peaks
            .iter()
            .enumerate()
            .map(|(i, p)| format!("Ch{i}: {p:.1} dBFS"))
            .collect();
        println!("Max Capture     : {}", peaks_str.join(", "));
    }
    if !stats.max_playback_peaks.is_empty() {
        let peaks_str: Vec<String> = stats
            .max_playback_peaks
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let name = if i == 0 {
                    "Sub"
                } else if i == 1 {
                    "Coax"
                } else {
                    "Ch"
                };
                format!("{name}: {p:.1} dBFS")
            })
            .collect();
        println!("Max Playback    : {}", peaks_str.join(", "));
    }
    println!("Clipped Reads   : {}", stats.clipped_reads);
    println!("Max Clipped     : {}", stats.max_clipped_samples);
}

/// Formats and outputs status information as newline-delimited JSON.
pub fn output_json_status(timestamp: &str, status: &DspStatus) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "service": {
            "name": "camilladsp.service",
            "state": status.service.to_string(),
            "is_active": status.service.is_active(),
        },
        "host": status.host.as_str(),
        "port": status.port.as_u16(),
        "version": status.version,
        "config_title": status.config_title,
        "config_path": status.config_path,
        "capture_rate": status.capture_rate,
        "state": status.state.as_str(),
        "processing_load_percent": status.processing_load,
        "resampler_load_percent": status.resampler_load,
        "rate_adjust": status.rate_adjust,
        "rate_adjust_ppm": status.rate_adjust_ppm(),
        "buffer_level": status.buffer_level,
        "buffer_capacity": status.buffer_capacity,
        "buffer_fill_percent": status.buffer_fill_percent(),
        "volume_db": status.volume_db,
        "muted": status.muted,
        "clipped_samples": status.clipped_samples,
        "stop_reason": status.stop_reason.as_str(),
        "capture": {
            "peaks": status.capture.peaks,
            "rms": status.capture.rms,
        },
        "playback": {
            "peaks": status.playback.peaks,
            "rms": status.playback.rms,
        },
    });
    println!("{obj}");
}

/// Formats and outputs an error as JSON.
pub fn output_json_error(timestamp: &str, host: &DspHost, port: DspPort, err: &DspError) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "host": host.as_str(),
            "port": port.as_u16(),
            "error": err.summary(),
        })
    );
}

/// Formats and outputs clipped counter reset confirmation as JSON.
pub fn output_json_reset(timestamp: &str, host: &DspHost, port: DspPort) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "host": host.as_str(),
            "port": port.as_u16(),
            "clipped_samples_reset": true,
        })
    );
}

/// Resets clipped samples counter on the device.
#[must_use]
pub fn handle_reset_clipped(dsp: &DspDevice, opts: &CliOptions) -> ExitCode {
    match dsp.reset_clipped_samples() {
        Ok(()) => {
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_reset(&format_timestamp(), dsp.host(), dsp.port());
                }
                OutputFormat::Human => {
                    println!("Success: Reset CamillaDSP clipped samples counter.");
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(&format_timestamp(), dsp.host(), dsp.port(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("Failed to reset clipped samples: {err}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// Prints detailed human-readable status for a single read.
pub fn print_human_detailed(status: &DspStatus) {
    println!("System Service           : {}", status.service);
    if let Some(ref ver) = status.version {
        println!("DSP Engine Version       : {ver}");
    }
    if let Some(ref title) = status.config_title {
        println!("Active Config Title      : {title}");
    }
    if let Some(rate) = status.capture_rate {
        println!("Measured Capture Rate    : {rate} Hz");
    }

    let load_bar = format_load_meter(status.processing_load, 14);
    println!(
        "Processing State         : {} ({})",
        status.state,
        status.state.description()
    );
    println!(
        "Processing Load          : {:5.2}% {load_bar}",
        status.processing_load
    );
    if let Some(res_load) = status.resampler_load {
        println!("Resampler Load           : {res_load:.2}%");
    }
    println!(
        "Rate Adjust Ratio        : {:.6} ({:+.1} ppm)",
        status.rate_adjust,
        status.rate_adjust_ppm()
    );
    println!(
        "Buffer Fill              : {} / {} samples ({:.1}%)",
        status.buffer_level,
        status.buffer_capacity,
        status.buffer_fill_percent()
    );

    if let Some(vol) = status.volume_db {
        let mute_str = status
            .muted
            .map_or("", |m| if m { " (MUTED)" } else { " (unmuted)" });
        println!("Master Volume            : {vol:.1} dB{mute_str}");
    }

    println!("Clipped Samples Counter  : {}", status.clipped_samples);
    if !status.stop_reason.is_none() {
        println!("Stop Reason              : {}", status.stop_reason);
    }

    // Capture channels (L/R)
    let cap_peaks_str: Vec<String> = status
        .capture
        .peaks
        .iter()
        .map(|p| format!("{p:.1}"))
        .collect();
    println!(
        "Capture Peak (L/R)       : [{}] dBFS",
        cap_peaks_str.join(", ")
    );
    for i in 0..status.capture.channels() {
        let label = if i == 0 {
            "Ch0 (L)  "
        } else if i == 1 {
            "Ch1 (R)  "
        } else {
            "Channel  "
        };
        let p = status.capture.peak_or_floor(i);
        let r = status.capture.rms_or_floor(i);
        let bar = format_dbfs_meter(p, 14);
        println!("  • {label}: {bar} {p:5.1} dBFS (RMS: {r:5.1} dBFS)");
    }

    // Playback channels (Sub/Coax)
    let play_peaks_str: Vec<String> = status
        .playback
        .peaks
        .iter()
        .map(|p| format!("{p:.1}"))
        .collect();
    println!(
        "Playback Peak (Sub/Coax) : [{}] dBFS",
        play_peaks_str.join(", ")
    );
    for i in 0..status.playback.channels() {
        let label = if i == 0 {
            "Ch0 (Sub) "
        } else if i == 1 {
            "Ch1 (Coax)"
        } else {
            "Channel   "
        };
        let p = status.playback.peak_or_floor(i);
        let r = status.playback.rms_or_floor(i);
        let bar = format_dbfs_meter(p, 14);
        println!("  • {label}: {bar} {p:5.1} dBFS (RMS: {r:5.1} dBFS)");
    }
}

/// Executes a single status read iteration.
pub fn handle_dsp_iteration(
    dsp: &DspDevice,
    opts: &CliOptions,
    timestamp: &str,
    session_stats: &mut DspStats,
) {
    match dsp.read_status() {
        Ok(status) => {
            session_stats.record_success(&status);
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_status(timestamp, &status);
                }
                OutputFormat::Human => {
                    if opts.common.max_count == Some(1) {
                        print_human_detailed(&status);
                        if status.has_clipping() {
                            let bin = env::args().next().unwrap_or_else(|| "dsp-info".into());
                            println!("\nTip: Reset clipping counter with: {bin} --reset-clipped");
                        }
                        println!("=========================================================");
                    } else {
                        let cap_str = if status.capture.channels() >= 2 {
                            format!(
                                "{:.1}/{:.1} dBFS",
                                status.capture.peak_or_floor(0),
                                status.capture.peak_or_floor(1)
                            )
                        } else {
                            "N/A".into()
                        };
                        let play_str = if status.playback.channels() >= 2 {
                            format!(
                                "{:.1}/{:.1} dBFS",
                                status.playback.peak_or_floor(0),
                                status.playback.peak_or_floor(1)
                            )
                        } else {
                            "N/A".into()
                        };

                        println!(
                            "[{timestamp}] State: {:7} | Load: {:4.1}% | Buffer: {:4.1}% | Clip: {} | Cap: {cap_str} | Play: {play_str}",
                            status.state.as_str(),
                            status.processing_load,
                            status.buffer_fill_percent(),
                            status.clipped_samples
                        );
                        if opts.common.verbose {
                            print_human_detailed(&status);
                            println!("---------------------------------------------------------");
                        }
                    }
                }
            }
        }
        Err(err) => {
            session_stats.record_error();
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(timestamp, dsp.host(), dsp.port(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("[{timestamp}] Read error: {err}");
                }
            }
        }
    }
}

/// Runs the main diagnostic application loop with the configured device and options.
#[must_use]
pub fn run_app(dsp: &DspDevice, opts: &CliOptions) -> ExitCode {
    if opts.reset_clipped {
        return handle_reset_clipped(dsp, opts);
    }

    if opts.common.format == OutputFormat::Human {
        print_header(&dsp.hardware_info(), dsp.host(), dsp.port());
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        }
    }

    let mut stats = DspStats::default();

    run_poll_loop(opts.common.interval, |timestamp| {
        handle_dsp_iteration(dsp, opts, timestamp, &mut stats);
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
    let opts = match resolve_cli_opts(parse_cli_args(), "dsp-info", print_usage) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let dsp = DspDevice::with_host_and_port(opts.host.clone(), opts.port);
    run_app(&dsp, &opts)
}
