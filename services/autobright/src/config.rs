//! CLI and on-disk configuration resolution for the autobright daemon.
//!
//! CLI flags override any values found in the optional JSON configuration
//! file. The fully resolved configuration — including the brightness policy
//! subsystem — is logged at `info` level at boot so a running daemon's
//! behaviour is diagnosable from `journalctl` without debug logging.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use speaker_common::diag::{CliParse, next_arg_value};
use speaker_hardware::Config as BrightnessConfig;
use tracing::info;

use crate::error::AutobrightError;

/// Default kernel backlight device directory name.
pub const DEFAULT_BACKLIGHT_DEVICE: &str = "rpi_backlight";
/// Default tick interval in milliseconds.
pub const DEFAULT_TICK_MS: u64 = 50;
/// Default idle period before a manual bias is cleared, in seconds.
pub const DEFAULT_IDLE_RESET_SECS: u64 = 600;
/// Default run duration (`0` runs forever).
pub const DEFAULT_DURATION_SECS: u64 = 0;
/// Default on-disk configuration path.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/autobright/config.json";

/// Raw CLI options; `None` fields fall back to file values, then defaults.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AutobrightOptions {
    /// Backlight sysfs device name override.
    pub backlight_device: Option<Box<str>>,
    /// Tick interval override in milliseconds.
    pub tick_ms: Option<u64>,
    /// Idle manual-bias reset period override in seconds.
    pub idle_reset_secs: Option<u64>,
    /// Bounded run duration override in seconds (`0` runs forever).
    pub duration_secs: Option<u64>,
    /// Explicit JSON configuration file path.
    pub config: Option<PathBuf>,
}

/// Fully resolved daemon configuration after CLI/file/default precedence.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Kernel backlight device name.
    pub backlight_device: Box<str>,
    /// Tick interval in milliseconds.
    pub tick_ms: u64,
    /// Idle manual-bias reset period in seconds.
    pub idle_reset_secs: u64,
    /// Bounded run duration in seconds (`0` runs forever).
    pub duration_secs: u64,
    /// Configuration file that supplied overrides, if any.
    pub config_path: Option<PathBuf>,
    /// Pure brightness policy configuration.
    pub policy: BrightnessConfig,
}

/// Optional on-disk overrides, all fields default to absent.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    backlight_device: Option<String>,
    tick_ms: Option<u64>,
    idle_reset_secs: Option<u64>,
    duration_secs: Option<u64>,
    ema_alpha: Option<f32>,
    hyst_up: Option<f32>,
    hyst_down: Option<f32>,
    debounce_up: Option<f32>,
    debounce_down: Option<f32>,
    rate_up: Option<f32>,
    rate_down: Option<f32>,
    b_min: Option<f32>,
    gamma_min: Option<f32>,
    gamma_max: Option<f32>,
    pwm_gamma: Option<f32>,
}

/// Prints the autobright service usage text.
pub fn print_usage(bin_name: &str) {
    println!("DietPi Auto-Brightness Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "  -d, --backlight-device <NAME>  Backlight sysfs device (default: {DEFAULT_BACKLIGHT_DEVICE})"
    );
    println!(
        "  -t, --tick-ms <MS>            Control loop tick in milliseconds (default: {DEFAULT_TICK_MS})"
    );
    println!(
        "      --idle-reset-secs <SECS>  Clear manual bias after idle seconds (default: {DEFAULT_IDLE_RESET_SECS})"
    );
    println!(
        "      --duration-secs <SECS>    Exit after SECS seconds (default: {DEFAULT_DURATION_SECS}, forever)"
    );
    println!("  -c, --config <PATH>           JSON config file (default: {DEFAULT_CONFIG_PATH})");
    println!("  -V, --version                 Print version and exit");
    println!("  -h, --help                    Print this help text and exit");
}

/// Parses an unsigned decimal CLI value, rejecting signs and non-numeric text.
///
/// # Errors
/// Returns a descriptive user-facing error string when `val` carries a sign
/// prefix or does not parse as `u64`.
pub fn parse_u64_arg(val: &str, flag: &str) -> Result<u64, String> {
    if val.starts_with('+') || val.starts_with('-') {
        return Err(format!(
            "invalid {flag} value '{val}': sign prefixes are not accepted"
        ));
    }
    val.parse::<u64>()
        .map_err(|e| format!("invalid {flag} value '{val}': {e}"))
}

/// Parses command-line arguments into [`AutobrightOptions`].
///
/// # Errors
/// Returns a descriptive user-facing error string for a malformed argument.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<AutobrightOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let mut opts = AutobrightOptions::default();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliParse::Help),
            "-V" | "--version" => return Ok(CliParse::Version),
            "-d" | "--backlight-device" => {
                let val = next_arg_value(&mut iter, "--backlight-device")?;
                if val.is_empty() {
                    return Err("backlight device name must not be empty".to_string());
                }
                opts.backlight_device = Some(val.into_boxed_str());
            }
            "-t" | "--tick-ms" => {
                let val = next_arg_value(&mut iter, "--tick-ms")?;
                let ms = parse_u64_arg(&val, "--tick-ms")?;
                if ms == 0 {
                    return Err("invalid --tick-ms value '0': tick must be at least 1 ms".into());
                }
                opts.tick_ms = Some(ms);
            }
            "--idle-reset-secs" => {
                let val = next_arg_value(&mut iter, "--idle-reset-secs")?;
                let secs = parse_u64_arg(&val, "--idle-reset-secs")?;
                if secs == 0 {
                    return Err("invalid --idle-reset-secs value '0': use a positive period".into());
                }
                opts.idle_reset_secs = Some(secs);
            }
            "--duration-secs" => {
                let val = next_arg_value(&mut iter, "--duration-secs")?;
                opts.duration_secs = Some(parse_u64_arg(&val, "--duration-secs")?);
            }
            "-c" | "--config" => {
                opts.config = Some(PathBuf::from(next_arg_value(&mut iter, "--config")?));
            }
            _ => return Err(format!("unknown option '{arg}', use --help for usage")),
        }
    }
    Ok(CliParse::Options(opts))
}

impl ResolvedConfig {
    /// Resolves CLI options against the optional JSON file and built-in
    /// defaults, then validates the brightness policy.
    ///
    /// # Errors
    /// Returns [`AutobrightError`] when an explicit config file is missing or
    /// malformed, an I/O error occurs, or the resolved policy is invalid.
    pub fn resolve(opts: AutobrightOptions) -> Result<Self, AutobrightError> {
        let explicit = opts.config.is_some();
        let path = opts
            .config
            .as_deref()
            .unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH));
        let file = read_file_config(path, explicit)?;
        let policy = resolve_policy(&file)?;

        let resolved = Self {
            backlight_device: opts
                .backlight_device
                .or_else(|| file.backlight_device.map(Into::into))
                .unwrap_or_else(|| DEFAULT_BACKLIGHT_DEVICE.into()),
            tick_ms: opts.tick_ms.or(file.tick_ms).unwrap_or(DEFAULT_TICK_MS),
            idle_reset_secs: opts
                .idle_reset_secs
                .or(file.idle_reset_secs)
                .unwrap_or(DEFAULT_IDLE_RESET_SECS),
            duration_secs: opts
                .duration_secs
                .or(file.duration_secs)
                .unwrap_or(DEFAULT_DURATION_SECS),
            config_path: opts.config,
            policy,
        };
        resolved.log_resolved();
        Ok(resolved)
    }

    /// Logs the entire resolved configuration and subsystem states at `info`.
    pub fn log_resolved(&self) {
        let p = self.policy;
        info!(
            backlight_device = %self.backlight_device,
            tick_ms = self.tick_ms,
            idle_reset_secs = self.idle_reset_secs,
            duration_secs = self.duration_secs,
            config_path = self.config_path.as_deref().map_or("none", |p| p.to_str().unwrap_or("?")),
            ema_alpha = p.ema_alpha,
            hyst_up = p.hyst_up,
            hyst_down = p.hyst_down,
            debounce_up_secs = p.debounce_up,
            debounce_down_secs = p.debounce_down,
            rate_up = p.rate_up,
            rate_down = p.rate_down,
            b_min = p.b_min,
            gamma_min = p.gamma_min,
            gamma_max = p.gamma_max,
            pwm_gamma = p.pwm_gamma,
            "Autobright configuration resolved"
        );
    }
}

fn read_file_config(path: &Path, explicit: bool) -> Result<FileConfig, AutobrightError> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            AutobrightError::config(format!("malformed config {}: {e}", path.display()))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
            Ok(FileConfig::default())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AutobrightError::config(
            format!("config file not found: {}", path.display()),
        )),
        Err(e) => Err(e.into()),
    }
}

fn resolve_policy(file: &FileConfig) -> Result<BrightnessConfig, AutobrightError> {
    let mut policy = BrightnessConfig::default();
    if let Some(v) = file.ema_alpha {
        policy.ema_alpha = v;
    }
    if let Some(v) = file.hyst_up {
        policy.hyst_up = v;
    }
    if let Some(v) = file.hyst_down {
        policy.hyst_down = v;
    }
    if let Some(v) = file.debounce_up {
        policy.debounce_up = v;
    }
    if let Some(v) = file.debounce_down {
        policy.debounce_down = v;
    }
    if let Some(v) = file.rate_up {
        policy.rate_up = v;
    }
    if let Some(v) = file.rate_down {
        policy.rate_down = v;
    }
    if let Some(v) = file.b_min {
        policy.b_min = v;
    }
    if let Some(v) = file.gamma_min {
        policy.gamma_min = v;
    }
    if let Some(v) = file.gamma_max {
        policy.gamma_max = v;
    }
    if let Some(v) = file.pwm_gamma {
        policy.pwm_gamma = v;
    }
    policy
        .validate()
        .map_err(|e| AutobrightError::config(format!("invalid brightness policy: {e}")))?;
    Ok(policy)
}
