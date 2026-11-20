//! Phone-like ambient brightness policy with hysteresis and manual gamma bias.
//!
//! Implements a pure, I/O-free control policy that converts ambient illuminance
//! into a perceptual brightness level and a hardware duty cycle. The policy
//! smooths raw lux samples with an exponential moving average, applies
//! asymmetric hysteresis and debounce around the last committed anchor,
//! slews toward the target with asymmetric rise/fall rates, and preserves a
//! user-supplied manual bias as a gamma exponent on the base curve.

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};

/// Default brightness policy configuration matching the appliance display panel.
pub const DEFAULT_CONFIG: Config = Config {
    ema_alpha: 0.2,
    hyst_up: 0.25,
    hyst_down: 0.30,
    debounce_up: 1.5,
    debounce_down: 5.0,
    rate_up: 0.40,
    rate_down: 0.15,
    b_min: 0.05,
    gamma_min: 0.30,
    gamma_max: 3.00,
    pwm_gamma: 2.20,
};

/// Exponential moving average weight applied to each fresh lux sample.
const BASE_CURVE_SLOPE: f32 = 0.103;

/// Maximum integration step accepted by [`AutoBrightness::update`] in seconds.
const MAX_DT_SECS: f32 = 0.5;

/// Largest illuminance reference that cannot trigger hysteresis chatter.
const MIN_HYSTERESIS_ANCHOR_LUX: f32 = 2.0;

/// Tolerance for treating a base curve value as degenerate in gamma calculations.
const DEGENERATE_EPSILON: f32 = 1e-6;

/// Tuning parameters for the ambient brightness policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Exponential moving average weight for incoming lux samples (`0.0..=1.0`).
    pub ema_alpha: f32,
    /// Fractional rise above the anchor required to start an upward transition.
    pub hyst_up: f32,
    /// Fractional fall below the anchor required to start a downward transition.
    pub hyst_down: f32,
    /// Seconds an upward deviation must persist before its target is committed.
    pub debounce_up: f32,
    /// Seconds a downward deviation must persist before its target is committed.
    pub debounce_down: f32,
    /// Maximum perceptual brightness increase per second.
    pub rate_up: f32,
    /// Maximum perceptual brightness decrease per second.
    pub rate_down: f32,
    /// Minimum perceptual brightness floor (`0.0..1.0`).
    pub b_min: f32,
    /// Minimum manual gamma exponent.
    pub gamma_min: f32,
    /// Maximum manual gamma exponent.
    pub gamma_max: f32,
    /// Exponent mapping perceptual brightness to the hardware duty cycle.
    pub pwm_gamma: f32,
}

impl Config {
    /// Validates that every tuning parameter is finite and within its valid range.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] identifying the first field that is non-finite or
    /// outside its documented interval.
    pub const fn validate(self) -> Result<(), ConfigError> {
        if !self.ema_alpha.is_finite()
            || !self.hyst_up.is_finite()
            || !self.hyst_down.is_finite()
            || !self.debounce_up.is_finite()
            || !self.debounce_down.is_finite()
            || !self.rate_up.is_finite()
            || !self.rate_down.is_finite()
            || !self.b_min.is_finite()
            || !self.gamma_min.is_finite()
            || !self.gamma_max.is_finite()
            || !self.pwm_gamma.is_finite()
        {
            return Err(ConfigError::not_finite(
                "configuration contains a non-finite field",
            ));
        }
        if self.ema_alpha <= 0.0 || self.ema_alpha > 1.0 {
            return Err(ConfigError::out_of_range("ema_alpha must be in (0, 1]"));
        }
        if self.hyst_up < 0.0 || self.hyst_down < 0.0 {
            return Err(ConfigError::out_of_range(
                "hyst_up and hyst_down must be non-negative",
            ));
        }
        if self.debounce_up <= 0.0 || self.debounce_down <= 0.0 {
            return Err(ConfigError::out_of_range("debounce times must be positive"));
        }
        if self.rate_up <= 0.0 || self.rate_down <= 0.0 {
            return Err(ConfigError::out_of_range("slew rates must be positive"));
        }
        if self.b_min <= 0.0 || self.b_min >= 1.0 {
            return Err(ConfigError::out_of_range("b_min must be in (0, 1)"));
        }
        if self.gamma_min <= 0.0 || self.gamma_max < self.gamma_min {
            return Err(ConfigError::out_of_range(
                "gamma_min must be positive and gamma_max >= gamma_min",
            ));
        }
        if self.pwm_gamma <= 0.0 {
            return Err(ConfigError::out_of_range("pwm_gamma must be positive"));
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        DEFAULT_CONFIG
    }
}

/// Internal classification of brightness configuration failures.
#[derive(Debug)]
pub(crate) enum ConfigErrorKind {
    /// A tuning parameter was not a finite floating-point value.
    NotFinite { detail: &'static str },
    /// A tuning parameter was finite but outside its accepted interval.
    OutOfRange { detail: &'static str },
}

/// Canonical error returned when a [`Config`] fails validation.
#[derive(Debug)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    backtrace: Backtrace,
}

impl ConfigError {
    const fn not_finite(detail: &'static str) -> Self {
        Self {
            kind: ConfigErrorKind::NotFinite { detail },
            backtrace: Backtrace::disabled(),
        }
    }

    const fn out_of_range(detail: &'static str) -> Self {
        Self {
            kind: ConfigErrorKind::OutOfRange { detail },
            backtrace: Backtrace::disabled(),
        }
    }

    /// Returns `true` if a tuning parameter was non-finite.
    #[must_use]
    pub const fn is_not_finite(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::NotFinite { .. })
    }

    /// Returns `true` if a tuning parameter was outside its accepted range.
    #[must_use]
    pub const fn is_out_of_range(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::OutOfRange { .. })
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ConfigErrorKind::NotFinite { detail } | ConfigErrorKind::OutOfRange { detail } => {
                f.write_str(detail)
            }
        }
    }
}

speaker_common::impl_canonical_error_display!(ConfigError);

impl std::error::Error for ConfigError {}

/// Direction of a deviation currently waiting for its debounce interval to expire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingDir {
    /// Illuminance is inside the hysteresis deadband.
    None,
    /// Illuminance is above the upper hysteresis threshold.
    Up,
    /// Illuminance is below the lower hysteresis threshold.
    Down,
}

/// Pure state machine converting ambient lux into a smoothed brightness duty cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoBrightness {
    cfg: Config,
    lux_ema: f32,
    lux_anchor: f32,
    b_current: f32,
    b_target: f32,
    manual_gamma: f32,
    pending_dir: PendingDir,
    pending_elapsed: f32,
}

impl AutoBrightness {
    /// Creates a policy instance anchored at the supplied initial illuminance.
    #[must_use]
    pub fn new(initial_lux: f32, cfg: Config) -> Self {
        let lux = sanitize_lux(initial_lux, 0.0);
        let b = target_for(cfg, 1.0, lux);
        Self {
            cfg,
            lux_ema: lux,
            lux_anchor: lux,
            b_current: b,
            b_target: b,
            manual_gamma: 1.0,
            pending_dir: PendingDir::None,
            pending_elapsed: 0.0,
        }
    }

    /// Computes the base logarithmic brightness curve clamped to the configured floor.
    #[must_use]
    pub fn base_curve(&self, lux: f32) -> f32 {
        base_curve_for(self.cfg, lux)
    }

    /// Computes the perceptual target brightness for a lux sample under the manual bias.
    #[must_use]
    pub fn target_from_lux(&self, lux: f32) -> f32 {
        target_for(self.cfg, self.manual_gamma, lux)
    }

    /// Applies a user brightness value as a gamma bias over the smoothed lux base.
    ///
    /// The perceptual level snaps to `user_b`; a non-finite value is ignored.
    pub fn on_manual_change(&mut self, user_b: f32) {
        if !user_b.is_finite() {
            return;
        }
        let clamped = user_b.clamp(self.cfg.b_min, 1.0);
        let base = self.base_curve(self.lux_ema);
        if base > DEGENERATE_EPSILON && (base - 1.0).abs() > DEGENERATE_EPSILON && base.is_finite()
        {
            let gamma = clamped.ln() / base.ln();
            self.manual_gamma = if gamma.is_finite() {
                gamma.clamp(self.cfg.gamma_min, self.cfg.gamma_max)
            } else {
                1.0
            };
        } else {
            self.manual_gamma = 1.0;
        }
        self.b_current = clamped;
        self.b_target = clamped;
        self.pending_dir = PendingDir::None;
        self.pending_elapsed = 0.0;
    }

    /// Clears the manual gamma bias while preserving the current brightness level.
    pub fn reset_manual_bias(&mut self) {
        self.manual_gamma = 1.0;
        self.b_target = self.target_from_lux(self.lux_ema);
    }

    /// Advances the policy by one tick and returns the hardware duty cycle.
    ///
    /// Non-finite or negative lux samples leave the exponential average unchanged,
    /// and integration steps are clamped to at most half a second.
    #[must_use]
    pub fn update(&mut self, raw_lux: f32, dt_secs: f32) -> f32 {
        let dt = if dt_secs.is_finite() {
            dt_secs.clamp(0.0, MAX_DT_SECS)
        } else {
            0.0
        };
        let lux = sanitize_lux(raw_lux, self.lux_ema);
        self.lux_ema += self.cfg.ema_alpha * (lux - self.lux_ema);
        self.lux_ema = sanitize_lux(self.lux_ema, 0.0);

        let reference = self.lux_anchor.max(MIN_HYSTERESIS_ANCHOR_LUX);
        let upper = reference * (1.0 + self.cfg.hyst_up);
        let lower = (reference * (1.0 - self.cfg.hyst_down)).max(0.0);
        let direction = if self.lux_ema > upper {
            PendingDir::Up
        } else if self.lux_ema < lower {
            PendingDir::Down
        } else {
            PendingDir::None
        };

        if direction != self.pending_dir {
            self.pending_dir = direction;
            self.pending_elapsed = 0.0;
        }
        if self.pending_dir != PendingDir::None {
            self.pending_elapsed += dt;
            let required = if self.pending_dir == PendingDir::Up {
                self.cfg.debounce_up
            } else {
                self.cfg.debounce_down
            };
            if self.pending_elapsed >= required {
                self.lux_anchor = self.lux_ema;
                self.b_target = self.target_from_lux(self.lux_ema);
                self.pending_dir = PendingDir::None;
                self.pending_elapsed = 0.0;
            }
        }

        let diff = self.b_target - self.b_current;
        let step = if diff > 0.0 {
            self.cfg.rate_up * dt
        } else {
            self.cfg.rate_down * dt
        };
        self.b_current = (self.b_current + diff.clamp(-step, step)).clamp(self.cfg.b_min, 1.0);
        self.b_current.powf(self.cfg.pwm_gamma).clamp(0.0, 1.0)
    }

    /// Returns the current smoothed perceptual brightness level.
    #[must_use]
    pub const fn brightness(&self) -> f32 {
        self.b_current
    }

    /// Returns the perceptual brightness target toward which the policy slews.
    #[must_use]
    pub const fn target(&self) -> f32 {
        self.b_target
    }

    /// Returns the active manual gamma exponent (`1.0` when unbiased).
    #[must_use]
    pub const fn manual_gamma(&self) -> f32 {
        self.manual_gamma
    }

    /// Returns the exponentially smoothed ambient illuminance in lux.
    #[must_use]
    pub const fn lux_ema(&self) -> f32 {
        self.lux_ema
    }

    /// Returns the lux anchor used as the hysteresis reference.
    #[must_use]
    pub const fn lux_anchor(&self) -> f32 {
        self.lux_anchor
    }

    /// Returns the configuration used by this policy instance.
    #[must_use]
    pub const fn config(&self) -> Config {
        self.cfg
    }
}

fn sanitize_lux(raw: f32, fallback: f32) -> f32 {
    if raw.is_finite() && raw >= 0.0 {
        raw
    } else {
        fallback
    }
}

fn base_curve_for(cfg: Config, lux: f32) -> f32 {
    // `f32::max` maps NaN and negative infinity to zero, while positive
    // infinity saturates to the configured ceiling through the final clamp.
    let safe_lux = lux.max(0.0);
    (BASE_CURVE_SLOPE * (safe_lux + 1.0).ln() + cfg.b_min).clamp(cfg.b_min, 1.0)
}

fn target_for(cfg: Config, gamma: f32, lux: f32) -> f32 {
    base_curve_for(cfg, lux).powf(gamma).clamp(cfg.b_min, 1.0)
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AutoBrightness>();
    assert_send_sync::<Config>();
    assert_send_sync::<ConfigError>();
};
