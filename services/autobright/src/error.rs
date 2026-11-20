//! Canonical error type for the autobright daemon.
//!
//! Every fallible daemon operation maps into [`AutobrightError`], which keeps
//! the underlying foreign error as a typed source (never a pre-stringified
//! message) and provides a single-line [`AutobrightError::summary`] for
//! machine-oriented telemetry.

use std::backtrace::Backtrace;
use std::fmt;
use std::io;

use speaker_hardware::{BacklightError, LightError};

/// Canonical error for configuration, D-Bus, and hardware failures.
#[derive(Debug)]
pub struct AutobrightError {
    kind: AutobrightErrorKind,
    backtrace: Backtrace,
}

/// Private cause taxonomy behind [`AutobrightError`].
#[derive(Debug)]
enum AutobrightErrorKind {
    /// Invalid resolved configuration.
    Config(Box<str>),
    /// A user-supplied CLI argument failed validation.
    InvalidArgument(Box<str>),
    /// Ambient light sensor fault.
    Light(Box<LightError>),
    /// Backlight sysfs fault.
    Backlight(Box<BacklightError>),
    /// D-Bus transport or method fault.
    Dbus(Box<zbus::Error>),
    /// Filesystem fault while reading configuration.
    Io(io::Error),
    /// An aborted background task (expected during shutdown).
    Cancelled(Box<str>),
    /// Deterministic fault injected by a test double.
    MockFault(Box<str>),
}

impl AutobrightError {
    fn new(kind: AutobrightErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Builds a resolved-configuration error.
    #[must_use]
    pub fn config(message: impl Into<Box<str>>) -> Self {
        Self::new(AutobrightErrorKind::Config(message.into()))
    }

    /// Builds a user-facing CLI argument error.
    #[must_use]
    pub fn invalid_argument(message: impl Into<Box<str>>) -> Self {
        Self::new(AutobrightErrorKind::InvalidArgument(message.into()))
    }

    /// Builds a cancellation error carrying task context.
    #[must_use]
    pub fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(AutobrightErrorKind::Cancelled(message.into()))
    }

    /// Builds a deterministic fault for host test doubles.
    #[must_use]
    pub fn mock_fault(message: impl Into<Box<str>>) -> Self {
        Self::new(AutobrightErrorKind::MockFault(message.into()))
    }

    /// Returns `true` when the cause is invalid configuration.
    #[must_use]
    pub const fn is_config(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::Config(_))
    }

    /// Returns `true` when the cause is an invalid CLI argument.
    #[must_use]
    pub const fn is_invalid_argument(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::InvalidArgument(_))
    }

    /// Returns `true` when the cause is an ambient sensor fault.
    #[must_use]
    pub const fn is_light(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::Light(_))
    }

    /// Returns `true` when the cause is a backlight fault.
    #[must_use]
    pub const fn is_backlight(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::Backlight(_))
    }

    /// Returns `true` when the cause is a D-Bus fault.
    #[must_use]
    pub const fn is_dbus(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::Dbus(_))
    }

    /// Returns `true` when the cause is an aborted task.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::Cancelled(_))
    }

    /// Returns `true` when the cause is an injected mock fault.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, AutobrightErrorKind::MockFault(_))
    }

    fn fmt_kind(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AutobrightErrorKind::Config(msg) => write!(f, "invalid configuration: {msg}"),
            AutobrightErrorKind::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            AutobrightErrorKind::Light(e) => write!(f, "ambient sensor error: {e}"),
            AutobrightErrorKind::Backlight(e) => write!(f, "backlight error: {e}"),
            AutobrightErrorKind::Dbus(e) => write!(f, "D-Bus error: {e}"),
            AutobrightErrorKind::Io(e) => write!(f, "I/O error: {e}"),
            AutobrightErrorKind::Cancelled(msg) => write!(f, "task cancelled: {msg}"),
            AutobrightErrorKind::MockFault(msg) => write!(f, "mock fault: {msg}"),
        }
    }
}

speaker_common::impl_canonical_error_display!(AutobrightError);

impl std::error::Error for AutobrightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AutobrightErrorKind::Light(e) => Some(e.as_ref()),
            AutobrightErrorKind::Backlight(e) => Some(e.as_ref()),
            AutobrightErrorKind::Dbus(e) => Some(e.as_ref()),
            AutobrightErrorKind::Io(e) => Some(e),
            AutobrightErrorKind::Config(_)
            | AutobrightErrorKind::InvalidArgument(_)
            | AutobrightErrorKind::Cancelled(_)
            | AutobrightErrorKind::MockFault(_) => None,
        }
    }
}

impl From<LightError> for AutobrightError {
    fn from(err: LightError) -> Self {
        Self::new(AutobrightErrorKind::Light(Box::new(err)))
    }
}

impl From<BacklightError> for AutobrightError {
    fn from(err: BacklightError) -> Self {
        Self::new(AutobrightErrorKind::Backlight(Box::new(err)))
    }
}

impl From<zbus::Error> for AutobrightError {
    fn from(err: zbus::Error) -> Self {
        Self::new(AutobrightErrorKind::Dbus(Box::new(err)))
    }
}

impl From<zbus::fdo::Error> for AutobrightError {
    fn from(err: zbus::fdo::Error) -> Self {
        Self::from(zbus::Error::from(err))
    }
}

impl From<io::Error> for AutobrightError {
    fn from(err: io::Error) -> Self {
        Self::new(AutobrightErrorKind::Io(err))
    }
}
