//! Kernel sysfs backlight adapter with an injectable mock backend.
//!
//! Wraps the Linux `/sys/class/backlight/<device>` interface used by the
//! appliance panel (`rpi_backlight`, raw range `0..=255`) behind a cheaply
//! cloneable handle. Blocking file I/O is offloaded to a Tokio blocking task
//! by the asynchronous wrappers, and every read/write fault is represented by
//! a canonical [`BacklightError`] that preserves the originating [`std::io::Error`].

use std::backtrace::Backtrace;
use std::fmt::{self, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use speaker_common::runtime::{FromJoinError, run_blocking};

/// Root sysfs directory containing kernel backlight devices.
pub const BACKLIGHT_SYSFS_BASE: &str = "/sys/class/backlight";

/// Default backlight device used by the appliance display panel.
pub const DEFAULT_BACKLIGHT_DEVICE: &str = "rpi_backlight";

/// Default raw brightness exposed by the mock backend.
const MOCK_DEFAULT_RAW: u32 = 0;

/// Default maximum raw brightness exposed by the mock backend.
const MOCK_DEFAULT_MAX: u32 = 255;

/// Internal classification of backlight failures.
#[derive(Debug)]
pub(crate) enum BacklightErrorKind {
    /// Underlying sysfs filesystem operation failed.
    Io { source: io::Error },
    /// A sysfs brightness file contained a malformed integer.
    InvalidData { message: Box<str> },
    /// An error intentionally injected through the mock controller.
    MockFault { message: Box<str> },
    /// An asynchronous worker task was cancelled during shutdown.
    Cancelled { message: Box<str> },
}

/// Canonical error returned by all backlight read and write operations.
#[derive(Debug)]
pub struct BacklightError {
    kind: BacklightErrorKind,
    backtrace: Backtrace,
}

impl BacklightError {
    fn new(kind: BacklightErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    fn invalid_data(message: impl Into<Box<str>>) -> Self {
        Self::new(BacklightErrorKind::InvalidData {
            message: message.into(),
        })
    }

    fn mock_fault(message: Box<str>) -> Self {
        Self::new(BacklightErrorKind::MockFault { message })
    }

    fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(BacklightErrorKind::Cancelled {
            message: message.into(),
        })
    }

    /// Returns `true` if the sysfs node or device was not found.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(&self.kind, BacklightErrorKind::Io { source } if source.kind() == io::ErrorKind::NotFound)
    }

    /// Returns `true` if the process lacks permission to access the sysfs node.
    #[must_use]
    pub fn is_permission_denied(&self) -> bool {
        matches!(&self.kind, BacklightErrorKind::Io { source } if source.kind() == io::ErrorKind::PermissionDenied)
    }

    /// Returns `true` if a brightness file held malformed data.
    #[must_use]
    pub fn is_invalid_data(&self) -> bool {
        matches!(self.kind, BacklightErrorKind::InvalidData { .. })
            || matches!(&self.kind, BacklightErrorKind::Io { source } if source.kind() == io::ErrorKind::InvalidData)
    }

    /// Returns `true` if this error came from the mock fault injector.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, BacklightErrorKind::MockFault { .. })
    }

    /// Returns `true` if an asynchronous worker task was cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, BacklightErrorKind::Cancelled { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BacklightErrorKind::Io { source } => write!(f, "backlight sysfs I/O error: {source}"),
            BacklightErrorKind::InvalidData { message } => {
                write!(f, "invalid backlight sysfs data: {message}")
            }
            BacklightErrorKind::MockFault { message } => {
                write!(f, "injected mock fault: {message}")
            }
            BacklightErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
        }
    }
}

speaker_common::impl_canonical_error_display!(BacklightError);

impl std::error::Error for BacklightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            BacklightErrorKind::Io { source } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for BacklightError {
    fn from(source: io::Error) -> Self {
        Self::new(BacklightErrorKind::Io { source })
    }
}

impl FromJoinError for BacklightError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

/// Mutable state shared between a mocked [`Backlight`] and its controller.
#[derive(Debug)]
struct MockState {
    raw: u32,
    max: u32,
    read_error: Option<Box<str>>,
    write_error: Option<Box<str>>,
    read_count: u64,
    write_count: u64,
    last_written: u32,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            raw: MOCK_DEFAULT_RAW,
            max: MOCK_DEFAULT_MAX,
            read_error: None,
            write_error: None,
            read_count: 0,
            write_count: 0,
            last_written: 0,
        }
    }
}

/// Backend selection for a [`Backlight`] handle.
#[derive(Debug)]
enum Backend {
    /// Real kernel sysfs nodes for a discovered backlight device.
    Sysfs {
        /// Writable raw brightness node.
        brightness_path: PathBuf,
        /// Cached maximum raw brightness read at construction time.
        max_brightness: u32,
    },
    /// In-memory backend driven by a [`BacklightMockCtrl`].
    Mock(Arc<Mutex<MockState>>),
}

/// Cloneable handle to a kernel sysfs backlight device.
#[derive(Debug, Clone)]
pub struct Backlight {
    inner: Arc<Backend>,
}

impl Backlight {
    /// Opens the default backlight device under the standard sysfs root.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] if the device directory or `max_brightness`
    /// node is missing, unreadable, or malformed.
    pub fn new() -> Result<Self, BacklightError> {
        Self::with_root(BACKLIGHT_SYSFS_BASE, DEFAULT_BACKLIGHT_DEVICE)
    }

    /// Opens a named backlight device under the standard sysfs root.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] if the named device cannot be opened or its
    /// `max_brightness` node is malformed.
    pub fn with_device(device: &str) -> Result<Self, BacklightError> {
        Self::with_root(BACKLIGHT_SYSFS_BASE, device)
    }

    /// Opens a named backlight device under a custom sysfs root.
    ///
    /// This constructor enables hermetic tests against a synthetic sysfs tree.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] if `max_brightness` is missing, unreadable,
    /// malformed, or zero.
    pub fn with_root(root: impl AsRef<Path>, device: &str) -> Result<Self, BacklightError> {
        let device_dir = root.as_ref().join(device);
        let brightness_path = device_dir.join("brightness");
        let max_brightness_path = device_dir.join("max_brightness");
        let raw = fs::read_to_string(&max_brightness_path)?;
        let max_brightness = parse_u32(&raw, &max_brightness_path, "max_brightness")?;
        if max_brightness == 0 {
            return Err(BacklightError::invalid_data(format!(
                "'{}' must be greater than zero",
                max_brightness_path.display()
            )));
        }
        Ok(Self {
            inner: Arc::new(Backend::Sysfs {
                brightness_path,
                max_brightness,
            }),
        })
    }

    /// Creates a mocked backlight paired with its [`BacklightMockCtrl`].
    #[must_use]
    pub fn new_mocked() -> (Self, BacklightMockCtrl) {
        let state = Arc::new(Mutex::new(MockState::default()));
        let ctrl = BacklightMockCtrl {
            state: Arc::clone(&state),
        };
        (
            Self {
                inner: Arc::new(Backend::Mock(state)),
            },
            ctrl,
        )
    }

    /// Returns the maximum writable raw brightness value.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn max_brightness(&self) -> u32 {
        match self.inner.as_ref() {
            Backend::Sysfs { max_brightness, .. } => *max_brightness,
            Backend::Mock(state) => state.lock().expect("mock state lock poisoned").max,
        }
    }

    /// Reads the current raw brightness value.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] if the sysfs node cannot be read or parsed,
    /// or if the mock controller has injected a read fault.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn read(&self) -> Result<u32, BacklightError> {
        match self.inner.as_ref() {
            Backend::Sysfs {
                brightness_path, ..
            } => {
                let raw = fs::read_to_string(brightness_path)?;
                parse_u32(&raw, brightness_path, "brightness")
            }
            Backend::Mock(state) => {
                let mut guard = state.lock().expect("mock state lock poisoned");
                guard.read_count += 1;
                if let Some(message) = &guard.read_error {
                    return Err(BacklightError::mock_fault(message.clone()));
                }
                Ok(guard.raw)
            }
        }
    }

    /// Writes a raw brightness value, clamping it to the device maximum.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] if the sysfs node cannot be written or if
    /// the mock controller has injected a write fault.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn write(&self, raw: u32) -> Result<(), BacklightError> {
        let clamped = raw.min(self.max_brightness());
        match self.inner.as_ref() {
            Backend::Sysfs {
                brightness_path, ..
            } => {
                fs::write(brightness_path, format!("{clamped}\n"))?;
                Ok(())
            }
            Backend::Mock(state) => {
                let mut guard = state.lock().expect("mock state lock poisoned");
                guard.write_count += 1;
                if let Some(message) = &guard.write_error {
                    return Err(BacklightError::mock_fault(message.clone()));
                }
                guard.raw = clamped;
                guard.last_written = clamped;
                Ok(())
            }
        }
    }

    /// Asynchronously reads the raw brightness on a blocking worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] on a sysfs or injected mock read failure, or
    /// if the blocking worker task is cancelled.
    ///
    /// # Panics
    ///
    /// Resumes worker thread panics to preserve the `M-PANIC-ON-BUG` contract.
    pub async fn read_async(&self) -> Result<u32, BacklightError> {
        let this = self.clone();
        run_blocking("backlight read", move || this.read()).await
    }

    /// Asynchronously writes raw brightness on a blocking worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`BacklightError`] on a sysfs or injected mock write failure, or
    /// if the blocking worker task is cancelled.
    ///
    /// # Panics
    ///
    /// Resumes worker thread panics to preserve the `M-PANIC-ON-BUG` contract.
    pub async fn write_async(&self, raw: u32) -> Result<(), BacklightError> {
        let this = self.clone();
        run_blocking("backlight write", move || this.write(raw)).await
    }
}

fn parse_u32(raw: &str, path: &Path, name: &str) -> Result<u32, BacklightError> {
    let text = raw.trim();
    text.parse::<u32>().map_err(|err| {
        BacklightError::invalid_data(format!(
            "invalid {name} value '{text}' in '{}': {err}",
            path.display()
        ))
    })
}

/// Controller for injecting readings, faults, and inspecting calls on a mock backlight.
#[derive(Debug, Clone)]
pub struct BacklightMockCtrl {
    state: Arc<Mutex<MockState>>,
}

impl BacklightMockCtrl {
    /// Overwrites the raw brightness reported by subsequent mock reads.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_raw(&self, raw: u32) {
        self.lock().raw = raw;
    }

    /// Overwrites the maximum brightness used to clamp mock writes.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_max_brightness(&self, max: u32) {
        self.lock().max = max;
    }

    /// Sets or clears the fault text returned by subsequent mock reads.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_read_error(&self, message: Option<Box<str>>) {
        self.lock().read_error = message;
    }

    /// Sets or clears the fault text returned by subsequent mock writes.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_write_error(&self, message: Option<Box<str>>) {
        self.lock().write_error = message;
    }

    /// Returns the number of mock read calls made so far.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn read_count(&self) -> u64 {
        self.lock().read_count
    }

    /// Returns the number of mock write calls made so far.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn write_count(&self) -> u64 {
        self.lock().write_count
    }

    /// Returns the last raw brightness successfully written to the mock.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn last_written(&self) -> u32 {
        self.lock().last_written
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state lock poisoned")
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Backlight>();
    assert_send_sync::<BacklightMockCtrl>();
    assert_send_sync::<BacklightError>();
};
