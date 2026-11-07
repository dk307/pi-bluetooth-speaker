//! Touchscreen event ingestion, calibration, and Slint window dispatching.
//!
//! Provides direct reading of Linux `evdev` touch input devices (such as the
//! Raspberry Pi official touch controller or Freenove 4.3" DSI capacitive panel
//! driven by `raspberrypi-ts`), normalizes hardware coordinates to Slint logical
//! screen space (800x480), and dispatches pointer events to [`MinimalSoftwareWindow`]
//! on the UI render thread.
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`,
//! `M-MOCKABLE-SYSCALLS`, `M-ERRORS-CANONICAL-STRUCTS`).

use std::backtrace::Backtrace;
#[cfg(test)]
use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{IntoRawFd, RawFd};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use slint::LogicalPosition;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};

/// Linux input subsystem event types.
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;

/// Linux synchronization event codes.
pub const SYN_REPORT: u16 = 0x00;

/// Linux key codes for touch contact.
pub const BTN_TOUCH: u16 = 0x14a;

/// Linux absolute axis codes for single-touch and multi-touch.
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

/// Default touch event device path on Raspberry Pi / `DietPi`.
pub const DEFAULT_TOUCH_DEVICE_PATH: &str = "/dev/input/event0";

/// Canonical appliance logical display dimensions.
pub const LOGICAL_SCREEN_WIDTH: f32 = 800.0;
pub const LOGICAL_SCREEN_HEIGHT: f32 = 480.0;

/// Generates an `_IOR` ioctl request code matching Linux `<asm-generic/ioctl.h>`.
const fn ior_ioctl(ioc_type: u8, nr: u8, size: usize) -> libc::c_ulong {
    const IOC_READ: libc::c_ulong = 2;
    ((IOC_READ) << 30)
        | (((size as libc::c_ulong) & 0x1fff) << 16)
        | ((ioc_type as libc::c_ulong) << 8)
        | (nr as libc::c_ulong)
}

/// Linux `EVIOCGNAME(len)` ioctl number.
const fn eviocgname_ioctl(len: usize) -> libc::c_ulong {
    ior_ioctl(b'E', 0x06, len)
}

/// Linux `EVIOCGABS(abs)` ioctl number.
const fn eviocgabs_ioctl(abs: u8) -> libc::c_ulong {
    ior_ioctl(b'E', 0x40 + abs, std::mem::size_of::<libc::input_absinfo>())
}

/// Constructs a Linux `libc::input_event`.
#[must_use]
#[cfg(test)]
pub fn make_input_event(type_: u16, code: u16, value: i32) -> libc::input_event {
    libc::input_event {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_,
        code,
        value,
    }
}

/// Serializes a `libc::input_event` to raw bytes matching host C struct memory layout.
#[must_use]
#[cfg(test)]
pub fn input_event_to_bytes(
    ev: &libc::input_event,
) -> [u8; std::mem::size_of::<libc::input_event>()] {
    // SAFETY: input_event is plain data with standard C layout.
    unsafe { std::mem::transmute_copy(ev) }
}

/// Abstract interface for Linux `evdev` system calls (`open`, `ioctl`, `fcntl`, `read`, `close`).
///
/// Enables complete host-based unit testing and fault injection without requiring physical
/// Linux digitizer hardware or root privileges.
pub trait EvdevSyscall: Send + Sync + Debug {
    /// Opens the input device at `path`.
    ///
    /// # Errors
    /// Returns [`io::Error`] on failure.
    fn open(&self, path: &Path) -> io::Result<RawFd>;

    /// Sets the file descriptor to non-blocking mode (`O_NONBLOCK`).
    ///
    /// # Errors
    /// Returns [`io::Error`] on failure.
    fn set_nonblocking(&self, fd: RawFd) -> io::Result<()>;

    /// Reads raw event bytes from the file descriptor.
    ///
    /// # Errors
    /// Returns [`io::Error`] on failure.
    fn read(&self, fd: RawFd, buf: &mut [u8]) -> io::Result<usize>;

    /// Queries `libc::input_absinfo` for the given absolute axis code.
    ///
    /// # Errors
    /// Returns [`io::Error`] on failure.
    fn query_abs(&self, fd: RawFd, abs_code: u16) -> io::Result<libc::input_absinfo>;

    /// Queries the human-readable device name via `EVIOCGNAME`.
    ///
    /// # Errors
    /// Returns [`io::Error`] on failure.
    fn query_name(&self, fd: RawFd) -> io::Result<String>;

    /// Closes the file descriptor.
    ///
    /// # Errors
    /// Returns [`io::Error`] on failure.
    fn close(&self, fd: RawFd) -> io::Result<()>;
}

/// Production implementation of [`EvdevSyscall`] dispatching to real POSIX / Linux syscalls.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeEvdevSyscall;

impl EvdevSyscall for NativeEvdevSyscall {
    fn open(&self, path: &Path) -> io::Result<RawFd> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(file.into_raw_fd())
    }

    fn set_nonblocking(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: standard POSIX fcntl flags query and update on valid open fd.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn read(&self, fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: read into caller-provided slice.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            #[expect(clippy::cast_sign_loss, reason = "n is non-negative")]
            Ok(n as usize)
        }
    }

    fn query_abs(&self, fd: RawFd, abs_code: u16) -> io::Result<libc::input_absinfo> {
        let mut absinfo = libc::input_absinfo {
            value: 0,
            minimum: 0,
            maximum: 0,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };
        // SAFETY: ioctl EVIOCGABS writes to input_absinfo struct.
        let res = unsafe { libc::ioctl(fd, eviocgabs_ioctl(abs_code as u8), &raw mut absinfo) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(absinfo)
        }
    }

    fn query_name(&self, fd: RawFd) -> io::Result<String> {
        let mut buf = [0u8; 256];
        // SAFETY: ioctl EVIOCGNAME writes null-terminated string into buffer.
        let ret = unsafe {
            libc::ioctl(
                fd,
                eviocgname_ioctl(buf.len()),
                buf.as_mut_ptr().cast::<libc::c_void>(),
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            #[expect(clippy::cast_sign_loss, reason = "ret is non-negative")]
            let len = (ret as usize).min(buf.len() - 1);
            Ok(String::from_utf8_lossy(&buf[..len])
                .trim_end_matches('\0')
                .to_string())
        }
    }

    fn close(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: close descriptor.
        let res = unsafe { libc::close(fd) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Configuration for a simulated evdev touch device.
#[derive(Debug, Clone)]
#[cfg(test)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Mock fault injection flags require individual toggles"
)]
pub struct MockDeviceConfig {
    /// Human readable name returned by `EVIOCGNAME`.
    pub name: String,
    /// Absolute info returned for `ABS_X` or `ABS_MT_POSITION_X`.
    pub abs_x: libc::input_absinfo,
    /// Absolute info returned for `ABS_Y` or `ABS_MT_POSITION_Y`.
    pub abs_y: libc::input_absinfo,
    /// Pending raw event stream bytes to return on `read`.
    pub read_events: Vec<u8>,
    /// Optional failure to inject on `open`.
    pub fail_open: Option<io::ErrorKind>,
    /// Whether `set_nonblocking` should fail.
    pub fail_nonblocking: bool,
    /// Optional failure to inject on `read`.
    pub fail_read: Option<io::ErrorKind>,
    /// Whether `query_abs` should fail.
    pub fail_abs: bool,
    /// Whether single-touch `query_abs` should fail while MT succeeds.
    pub fail_st_abs_only: bool,
    /// Whether `query_name` should fail.
    pub fail_name: bool,
}

#[cfg(test)]
impl Default for MockDeviceConfig {
    fn default() -> Self {
        Self {
            name: "raspberrypi-ts".to_string(),
            abs_x: libc::input_absinfo {
                value: 0,
                minimum: 0,
                maximum: 800,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
            abs_y: libc::input_absinfo {
                value: 0,
                minimum: 0,
                maximum: 480,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
            read_events: Vec::new(),
            fail_open: None,
            fail_nonblocking: false,
            fail_read: None,
            fail_abs: false,
            fail_st_abs_only: false,
            fail_name: false,
        }
    }
}

/// Simulated evdev syscall backend for deterministic testing.
#[derive(Debug)]
#[cfg(test)]
pub struct MockEvdevSyscall {
    devices: Mutex<HashMap<RawFd, MockDeviceConfig>>,
    path_configs: Mutex<HashMap<PathBuf, MockDeviceConfig>>,
    next_fd: AtomicI32,
    closed_fds: Mutex<Vec<RawFd>>,
}

#[cfg(test)]
impl Default for MockEvdevSyscall {
    fn default() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
            path_configs: Mutex::new(HashMap::new()),
            next_fd: AtomicI32::new(100),
            closed_fds: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl MockEvdevSyscall {
    /// Creates a new empty [`MockEvdevSyscall`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a device config mapped to a filesystem path.
    ///
    /// # Panics
    /// Panics if the internal path configs mutex is poisoned.
    pub fn register_path(&self, path: impl Into<PathBuf>, config: MockDeviceConfig) {
        let mut map = self.path_configs.lock().expect("path_configs poisoned");
        map.insert(path.into(), config);
    }

    /// Pushes raw event bytes to an open descriptor's read queue.
    ///
    /// # Panics
    /// Panics if the internal devices mutex is poisoned.
    pub fn push_raw_events(&self, fd: RawFd, bytes: &[u8]) {
        let mut devs = self.devices.lock().expect("devices poisoned");
        if let Some(dev) = devs.get_mut(&fd) {
            dev.read_events.extend_from_slice(bytes);
        }
    }

    /// Checks if a file descriptor has been closed.
    ///
    /// # Panics
    /// Panics if the internal closed fds mutex is poisoned.
    #[must_use]
    pub fn is_closed(&self, fd: RawFd) -> bool {
        let closed = self.closed_fds.lock().expect("closed_fds poisoned");
        closed.contains(&fd)
    }
}

#[cfg(test)]
impl EvdevSyscall for MockEvdevSyscall {
    fn open(&self, path: &Path) -> io::Result<RawFd> {
        let configs = self.path_configs.lock().expect("path_configs poisoned");
        if let Some(cfg) = configs.get(path) {
            if let Some(kind) = cfg.fail_open {
                return Err(io::Error::new(kind, "simulated open error"));
            }
            let fd = self.next_fd.fetch_add(1, Ordering::SeqCst);
            let mut devs = self.devices.lock().expect("devices poisoned");
            devs.insert(fd, cfg.clone());
            Ok(fd)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "device path not found",
            ))
        }
    }

    fn set_nonblocking(&self, fd: RawFd) -> io::Result<()> {
        let devs = self.devices.lock().expect("devices poisoned");
        if let Some(dev) = devs.get(&fd) {
            if dev.fail_nonblocking {
                return Err(io::Error::other("simulated fcntl error"));
            }
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        }
    }

    fn read(&self, fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
        let mut devs = self.devices.lock().expect("devices poisoned");
        if let Some(dev) = devs.get_mut(&fd) {
            if let Some(kind) = dev.fail_read {
                return Err(io::Error::new(kind, "simulated read error"));
            }
            if dev.read_events.is_empty() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
            }
            let to_read = dev.read_events.len().min(buf.len());
            let chunk: Vec<u8> = dev.read_events.drain(..to_read).collect();
            buf[..to_read].copy_from_slice(&chunk);
            Ok(to_read)
        } else {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        }
    }

    fn query_abs(&self, fd: RawFd, abs_code: u16) -> io::Result<libc::input_absinfo> {
        let devs = self.devices.lock().expect("devices poisoned");
        if let Some(dev) = devs.get(&fd) {
            if dev.fail_abs {
                return Err(io::Error::other("simulated abs ioctl error"));
            }
            if dev.fail_st_abs_only && (abs_code == ABS_X || abs_code == ABS_Y) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "st abs ioctl unsupported",
                ));
            }
            match abs_code {
                ABS_X | ABS_MT_POSITION_X => Ok(dev.abs_x),
                ABS_Y | ABS_MT_POSITION_Y => Ok(dev.abs_y),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported abs code",
                )),
            }
        } else {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        }
    }

    fn query_name(&self, fd: RawFd) -> io::Result<String> {
        let devs = self.devices.lock().expect("devices poisoned");
        if let Some(dev) = devs.get(&fd) {
            if dev.fail_name {
                return Err(io::Error::other("simulated name ioctl error"));
            }
            Ok(dev.name.clone())
        } else {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        }
    }

    fn close(&self, fd: RawFd) -> io::Result<()> {
        let mut devs = self.devices.lock().expect("devices poisoned");
        devs.remove(&fd);
        let mut closed = self.closed_fds.lock().expect("closed_fds poisoned");
        closed.push(fd);
        Ok(())
    }
}

/// Internal classification of a [`TouchError`].
#[derive(Debug)]
pub(crate) enum TouchErrorKind {
    /// I/O error accessing a device node or sysfs path.
    Io { path: Box<str>, source: io::Error },
    /// No compatible touch input device was detected during auto-scan.
    DeviceNotFound(Box<str>),
    /// Deterministic test fault injected by mock controller.
    MockFault(Box<str>),
    /// Invalid configuration or calibration parameters.
    InvalidConfiguration(Box<str>),
}

/// Canonical error type for touch input operations.
#[derive(Debug)]
pub struct TouchError {
    kind: TouchErrorKind,
    backtrace: Backtrace,
}

impl TouchError {
    fn new(kind: TouchErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Creates an I/O error associated with a specific device path.
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Self::new(TouchErrorKind::Io {
            path: path.as_ref().to_string_lossy().into(),
            source,
        })
    }

    /// Creates a device-not-found error.
    pub fn device_not_found(msg: impl Into<Box<str>>) -> Self {
        Self::new(TouchErrorKind::DeviceNotFound(msg.into()))
    }

    /// Creates a mock fault error.
    pub fn mock_fault(msg: impl Into<Box<str>>) -> Self {
        Self::new(TouchErrorKind::MockFault(msg.into()))
    }

    /// Creates an invalid configuration error.
    pub fn invalid_config(msg: impl Into<Box<str>>) -> Self {
        Self::new(TouchErrorKind::InvalidConfiguration(msg.into()))
    }

    /// Returns whether this error is an I/O failure.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        matches!(self.kind, TouchErrorKind::Io { .. })
    }

    /// Returns whether this error indicates no device was found.
    #[must_use]
    pub const fn is_device_not_found(&self) -> bool {
        matches!(self.kind, TouchErrorKind::DeviceNotFound(_))
    }

    /// Returns whether this error represents an injected mock fault.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, TouchErrorKind::MockFault(_))
    }

    /// Returns captured backtrace.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for TouchError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TouchErrorKind::Io { path, source } => {
                write!(f, "Touch I/O error on '{path}': {source}")
            }
            TouchErrorKind::DeviceNotFound(msg) => {
                write!(f, "Touch device not found: {msg}")
            }
            TouchErrorKind::MockFault(msg) => {
                write!(f, "Injected touch mock fault: {msg}")
            }
            TouchErrorKind::InvalidConfiguration(msg) => {
                write!(f, "Invalid touch configuration: {msg}")
            }
        }
    }
}

impl std::error::Error for TouchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            TouchErrorKind::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Axis coordinate mapping configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchCalibration {
    /// Minimum hardware report value on X axis.
    pub min_x: f32,
    /// Maximum hardware report value on X axis.
    pub max_x: f32,
    /// Minimum hardware report value on Y axis.
    pub min_y: f32,
    /// Maximum hardware report value on Y axis.
    pub max_y: f32,
    /// Whether to invert the X axis (0° vs 180° rotation).
    pub invert_x: bool,
    /// Whether to invert the Y axis (0° vs 180° rotation).
    pub invert_y: bool,
    /// Whether to swap X and Y axes (portrait vs landscape).
    pub swap_xy: bool,
}

impl Default for TouchCalibration {
    fn default() -> Self {
        Self {
            min_x: 0.0,
            max_x: LOGICAL_SCREEN_WIDTH,
            min_y: 0.0,
            max_y: LOGICAL_SCREEN_HEIGHT,
            invert_x: false,
            invert_y: false,
            swap_xy: false,
        }
    }
}

impl TouchCalibration {
    /// Returns true if all axis limit coordinates are finite numbers.
    #[must_use]
    pub fn is_finite(&self) -> bool {
        self.min_x.is_finite()
            && self.max_x.is_finite()
            && self.min_y.is_finite()
            && self.max_y.is_finite()
    }

    /// Normalizes raw digitizer coordinates into logical screen pixels.
    #[must_use]
    pub fn map_to_screen(
        &self,
        raw_x: i32,
        raw_y: i32,
        screen_w: f32,
        screen_h: f32,
    ) -> (f32, f32) {
        if !self.is_finite() || !screen_w.is_finite() || !screen_h.is_finite() {
            return (0.0, 0.0);
        }

        let (x_in, y_in) = if self.swap_xy {
            (raw_y as f32, raw_x as f32)
        } else {
            (raw_x as f32, raw_y as f32)
        };

        let span_x = (self.max_x - self.min_x).abs().max(1.0);
        let span_y = (self.max_y - self.min_y).abs().max(1.0);

        let norm_x = ((x_in - self.min_x) / span_x).clamp(0.0, 1.0);
        let norm_y = ((y_in - self.min_y) / span_y).clamp(0.0, 1.0);

        let final_x = if self.invert_x { 1.0 - norm_x } else { norm_x };
        let final_y = if self.invert_y { 1.0 - norm_y } else { norm_y };

        (final_x * screen_w, final_y * screen_h)
    }
}

/// Normalized touch action emitted by [`TouchDecoder`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchAction {
    /// Initial finger contact pressed down at logical coordinates.
    Down { x: f32, y: f32 },
    /// Active finger movement to logical coordinates.
    Move { x: f32, y: f32 },
    /// Contact released up at logical coordinates.
    Up { x: f32, y: f32 },
}

/// Single-touch state machine translating raw `input_event` streams to [`TouchAction`]s.
#[derive(Debug, Default)]
pub struct TouchDecoder {
    calibration: TouchCalibration,
    last_raw_x: i32,
    last_raw_y: i32,
    pending_raw_x: Option<i32>,
    pending_raw_y: Option<i32>,
    pending_pressed: Option<bool>,
    last_dispatched_x: f32,
    last_dispatched_y: f32,
    is_currently_pressed: bool,
}

impl TouchDecoder {
    /// Creates a new decoder with given axis calibration.
    #[must_use]
    pub fn new(calibration: TouchCalibration) -> Self {
        Self {
            calibration,
            ..Default::default()
        }
    }

    /// Returns a reference to the active calibration profile.
    #[must_use]
    pub const fn calibration(&self) -> &TouchCalibration {
        &self.calibration
    }

    /// Mutably updates the calibration profile.
    pub fn set_calibration(&mut self, calibration: TouchCalibration) {
        if calibration.is_finite() {
            self.calibration = calibration;
        } else {
            tracing::warn!(?calibration, "Rejected non-finite touch calibration");
        }
    }

    /// Processes a single raw Linux `libc::input_event` and emits a [`TouchAction`] if complete.
    pub fn process_event(&mut self, ev: &libc::input_event) -> Option<TouchAction> {
        match ev.type_ {
            EV_ABS => match ev.code {
                ABS_X | ABS_MT_POSITION_X => {
                    self.pending_raw_x = Some(ev.value);
                }
                ABS_Y | ABS_MT_POSITION_Y => {
                    self.pending_raw_y = Some(ev.value);
                }
                ABS_MT_TRACKING_ID => {
                    // -1 indicates touch release in Linux MT protocol B; >= 0 indicates active contact.
                    self.pending_pressed = Some(ev.value >= 0);
                }
                _ => {}
            },
            EV_KEY => {
                if ev.code == BTN_TOUCH {
                    self.pending_pressed = Some(ev.value != 0);
                }
            }
            EV_SYN if ev.code == SYN_REPORT => {
                return self.finalize_syn_packet();
            }
            _ => {}
        }
        None
    }

    fn finalize_syn_packet(&mut self) -> Option<TouchAction> {
        let (screen_w, screen_h) = (LOGICAL_SCREEN_WIDTH, LOGICAL_SCREEN_HEIGHT);

        let mut coords_changed = false;
        if let Some(rx) = self.pending_raw_x {
            self.last_raw_x = rx;
            coords_changed = true;
        }
        if let Some(ry) = self.pending_raw_y {
            self.last_raw_y = ry;
            coords_changed = true;
        }

        if coords_changed {
            let (sx, sy) = self.calibration.map_to_screen(
                self.last_raw_x,
                self.last_raw_y,
                screen_w,
                screen_h,
            );
            self.last_dispatched_x = sx;
            self.last_dispatched_y = sy;
        }

        let current_x = self.last_dispatched_x;
        let current_y = self.last_dispatched_y;

        let action = match (self.is_currently_pressed, self.pending_pressed) {
            (false, Some(true)) => {
                self.is_currently_pressed = true;
                Some(TouchAction::Down {
                    x: current_x,
                    y: current_y,
                })
            }
            (true, Some(false)) => {
                self.is_currently_pressed = false;
                Some(TouchAction::Up {
                    x: current_x,
                    y: current_y,
                })
            }
            (true, Some(true) | None) => {
                // Motion event while held down
                coords_changed.then_some(TouchAction::Move {
                    x: current_x,
                    y: current_y,
                })
            }
            (false, _) => None,
        };

        // Reset pending delta flags for next syn interval
        self.pending_raw_x = None;
        self.pending_raw_y = None;
        self.pending_pressed = None;

        action
    }
}

/// In-memory state of a mocked touch input controller.
#[derive(Debug, Default)]
struct MockTouchState {
    events: Vec<TouchAction>,
    down_count: usize,
    move_count: usize,
    up_count: usize,
    injected_fault: Option<Box<str>>,
}

/// Fault-injection and event simulation controller for unit and integration testing.
#[derive(Debug, Clone, Default)]
pub struct TouchMockCtrl {
    state: Arc<Mutex<MockTouchState>>,
}

impl TouchMockCtrl {
    /// Injects a Touch Down event at given coordinates.
    ///
    /// # Panics
    /// Panics if the internal mock state mutex has been poisoned by a prior test panic.
    pub fn send_touch_down(&self, x: f32, y: f32) {
        let mut state = self.state.lock().expect("mock state poisoned");
        state.down_count += 1;
        state.events.push(TouchAction::Down { x, y });
    }

    /// Injects a Touch Move event at given coordinates.
    ///
    /// # Panics
    /// Panics if the internal mock state mutex has been poisoned by a prior test panic.
    pub fn send_touch_move(&self, x: f32, y: f32) {
        let mut state = self.state.lock().expect("mock state poisoned");
        state.move_count += 1;
        state.events.push(TouchAction::Move { x, y });
    }

    /// Injects a Touch Up event at given coordinates.
    ///
    /// # Panics
    /// Panics if the internal mock state mutex has been poisoned by a prior test panic.
    pub fn send_touch_up(&self, x: f32, y: f32) {
        let mut state = self.state.lock().expect("mock state poisoned");
        state.up_count += 1;
        state.events.push(TouchAction::Up { x, y });
    }

    /// Injects a fault message that causes subsequent operations to fail.
    ///
    /// # Panics
    /// Panics if the internal mock state mutex has been poisoned by a prior test panic.
    pub fn set_fault(&self, fault: impl Into<Box<str>>) {
        let mut state = self.state.lock().expect("mock state poisoned");
        state.injected_fault = Some(fault.into());
    }

    /// Clears any injected fault.
    ///
    /// # Panics
    /// Panics if the internal mock state mutex has been poisoned by a prior test panic.
    pub fn clear_fault(&self) {
        let mut state = self.state.lock().expect("mock state poisoned");
        state.injected_fault = None;
    }

    /// Returns event dispatch counters: `(down, move, up)`.
    ///
    /// # Panics
    /// Panics if the internal mock state mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn counters(&self) -> (usize, usize, usize) {
        let state = self.state.lock().expect("mock state poisoned");
        (state.down_count, state.move_count, state.up_count)
    }
}

/// Touch input backend implementation.
enum TouchBackend {
    Hardware {
        fd: RawFd,
        device_path: Box<str>,
        syscall: Arc<dyn EvdevSyscall>,
    },
    Mock {
        ctrl: TouchMockCtrl,
    },
}

impl Debug for TouchBackend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hardware {
                device_path, fd, ..
            } => f
                .debug_struct("Hardware")
                .field("path", device_path)
                .field("fd", fd)
                .finish(),
            Self::Mock { .. } => f.debug_struct("Mock").finish(),
        }
    }
}

/// Unified touch input reader and Slint window dispatcher.
#[derive(Debug)]
pub struct TouchInputDevice {
    backend: TouchBackend,
    decoder: TouchDecoder,
    read_buffer: Vec<u8>,
}

impl Drop for TouchInputDevice {
    fn drop(&mut self) {
        if let TouchBackend::Hardware {
            fd, ref syscall, ..
        } = self.backend
            && fd >= 0
        {
            let _ = syscall.close(fd);
        }
    }
}

/// Returns true if a device name matches recognized touchscreen hardware.
fn is_touchscreen_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("touch") || lower.contains("raspberrypi-ts")
}

impl TouchInputDevice {
    /// Opens the touch input device at `path` using production POSIX syscalls.
    ///
    /// # Errors
    /// Returns [`TouchError`] if the device node cannot be opened or ioctls fail.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TouchError> {
        Self::open_with(path, Arc::new(NativeEvdevSyscall))
    }

    /// Opens the touch input device at `path` using the injected [`EvdevSyscall`] implementation.
    ///
    /// # Errors
    /// Returns [`TouchError`] if opening, configuring, or axis calibration fails.
    pub fn open_with(
        path: impl AsRef<Path>,
        syscall: Arc<dyn EvdevSyscall>,
    ) -> Result<Self, TouchError> {
        let path_ref = path.as_ref();
        let fd = syscall
            .open(path_ref)
            .map_err(|e| TouchError::io(path_ref, e))?;

        let calibration = match Self::query_axis_calibration(fd, path_ref, &*syscall) {
            Ok(c) => c,
            Err(e) => {
                let _ = syscall.close(fd);
                return Err(e);
            }
        };

        if let Err(e) = syscall.set_nonblocking(fd) {
            let _ = syscall.close(fd);
            return Err(TouchError::io(path_ref, e));
        }

        tracing::info!(
            path = %path_ref.display(),
            min_x = calibration.min_x,
            max_x = calibration.max_x,
            min_y = calibration.min_y,
            max_y = calibration.max_y,
            "Opened Linux evdev touch device with calibrated bounds"
        );

        Ok(Self {
            backend: TouchBackend::Hardware {
                fd,
                device_path: path_ref.to_string_lossy().into(),
                syscall,
            },
            decoder: TouchDecoder::new(calibration),
            read_buffer: Vec::with_capacity(std::mem::size_of::<libc::input_event>() * 16),
        })
    }

    /// Queries `EVIOCGABS` for X and Y axes on an open evdev file descriptor.
    #[expect(clippy::similar_names, reason = "X and Y axis info pair")]
    fn query_axis_calibration(
        fd: RawFd,
        path: &Path,
        syscall: &dyn EvdevSyscall,
    ) -> Result<TouchCalibration, TouchError> {
        let (abs_x_axis, abs_y_axis) =
            match (syscall.query_abs(fd, ABS_X), syscall.query_abs(fd, ABS_Y)) {
                (Ok(x), Ok(y)) => (x, y),
                _ => {
                    // Fall back to querying multi-touch ABS_MT_POSITION_X / Y
                    match (
                        syscall.query_abs(fd, ABS_MT_POSITION_X),
                        syscall.query_abs(fd, ABS_MT_POSITION_Y),
                    ) {
                        (Ok(x), Ok(y)) => (x, y),
                        (Err(e), _) | (_, Err(e)) => return Err(TouchError::io(path, e)),
                    }
                }
            };

        let min_x = abs_x_axis.minimum as f32;
        let max_x = if abs_x_axis.maximum > 0 {
            abs_x_axis.maximum as f32
        } else {
            LOGICAL_SCREEN_WIDTH
        };
        let min_y = abs_y_axis.minimum as f32;
        let max_y = if abs_y_axis.maximum > 0 {
            abs_y_axis.maximum as f32
        } else {
            LOGICAL_SCREEN_HEIGHT
        };

        if min_x >= max_x || min_y >= max_y {
            return Err(TouchError::invalid_config(format!(
                "Invalid touch axis limits on '{}': X [{}, {}], Y [{}, {}]",
                path.display(),
                min_x,
                max_x,
                min_y,
                max_y
            )));
        }

        Ok(TouchCalibration {
            min_x,
            max_x,
            min_y,
            max_y,
            invert_x: false,
            invert_y: false,
            swap_xy: false,
        })
    }

    /// Automatically discovers and opens an active touch device.
    ///
    /// # Errors
    /// Returns [`TouchError`] if no touch device is reachable.
    pub fn auto_discover() -> Result<Self, TouchError> {
        let syscall: Arc<dyn EvdevSyscall> = Arc::new(NativeEvdevSyscall);
        Self::auto_discover_with(&syscall)
    }

    /// Automatically discovers a touch device using the specified syscall implementation.
    ///
    /// # Errors
    /// Returns [`TouchError`] if no touch device is reachable.
    pub fn auto_discover_with(syscall: &Arc<dyn EvdevSyscall>) -> Result<Self, TouchError> {
        Self::auto_discover_in(Path::new("/dev/input"), syscall)
    }

    /// Automatically discovers a touch device scanning inside `input_dir`.
    ///
    /// # Errors
    /// Returns [`TouchError`] if no touch device is reachable.
    pub fn auto_discover_in(
        input_dir: &Path,
        syscall: &Arc<dyn EvdevSyscall>,
    ) -> Result<Self, TouchError> {
        let default_path = input_dir.join("event0");
        // 1. Check default path first
        if let Ok(dev) = Self::open_with(&default_path, Arc::clone(syscall))
            && is_touchscreen_name(&dev.query_device_name())
        {
            return Ok(dev);
        }

        // 2. Scan input_dir event* entries
        if let Ok(entries) = std::fs::read_dir(input_dir) {
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|s| s.starts_with("event"))
                })
                .collect();
            candidates.sort();

            for path in candidates {
                if let Ok(dev) = Self::open_with(&path, Arc::clone(syscall))
                    && is_touchscreen_name(&dev.query_device_name())
                {
                    return Ok(dev);
                }
            }
        }

        Err(TouchError::device_not_found(format!(
            "No touchscreen controller identified in {}",
            input_dir.display()
        )))
    }

    /// Queries the device human-readable name via `EVIOCGNAME`.
    #[must_use]
    pub fn query_device_name(&self) -> String {
        match &self.backend {
            TouchBackend::Hardware { fd, syscall, .. } => match syscall.query_name(*fd) {
                Ok(name) if !name.is_empty() => name,
                _ => "Unknown Evdev Device".to_string(),
            },
            TouchBackend::Mock { .. } => "Mock Touchscreen Device".to_string(),
        }
    }

    /// Creates an in-memory mocked touch device paired with a [`TouchMockCtrl`].
    #[must_use]
    pub fn new_mocked() -> (Self, TouchMockCtrl) {
        let ctrl = TouchMockCtrl::default();
        let dev = Self {
            backend: TouchBackend::Mock { ctrl: ctrl.clone() },
            decoder: TouchDecoder::default(),
            read_buffer: Vec::new(),
        };
        (dev, ctrl)
    }

    /// Returns the underlying raw file descriptor if backed by hardware.
    #[must_use]
    pub fn as_raw_fd(&self) -> Option<RawFd> {
        match &self.backend {
            TouchBackend::Hardware { fd, .. } => Some(*fd),
            TouchBackend::Mock { .. } => None,
        }
    }

    /// Updates the coordinate calibration profile.
    pub fn set_calibration(&mut self, calibration: TouchCalibration) {
        self.decoder.set_calibration(calibration);
    }

    /// Reads pending touch events from hardware or mock controller.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    ///
    /// # Errors
    /// Returns [`TouchError`] on I/O error or injected mock fault.
    pub fn poll_actions(&mut self) -> Result<Vec<TouchAction>, TouchError> {
        let mut actions = Vec::new();

        match &mut self.backend {
            TouchBackend::Hardware { fd, syscall, .. } => {
                const EV_SIZE: usize = std::mem::size_of::<libc::input_event>();
                let mut chunk = [0u8; EV_SIZE * 16];

                loop {
                    match syscall.read(*fd, &mut chunk) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            self.read_buffer.extend_from_slice(&chunk[..n]);

                            while self.read_buffer.len() >= EV_SIZE {
                                // SAFETY: input_event is plain data with standard C layout.
                                let ev: libc::input_event = unsafe {
                                    std::ptr::read_unaligned(
                                        self.read_buffer.as_ptr().cast::<libc::input_event>(),
                                    )
                                };
                                self.read_buffer.drain(..EV_SIZE);

                                if let Some(action) = self.decoder.process_event(&ev) {
                                    actions.push(action);
                                }
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(TouchError::io("evdev-read", e)),
                    }
                }
            }
            TouchBackend::Mock { ctrl } => {
                let mut state = ctrl.state.lock().expect("mock state poisoned");
                if let Some(ref fault) = state.injected_fault {
                    return Err(TouchError::mock_fault(fault.clone()));
                }
                actions.append(&mut state.events);
            }
        }

        Ok(actions)
    }

    /// Dispatches all pending touch actions to the given Slint [`MinimalSoftwareWindow`].
    ///
    /// # Panics
    /// Must be called strictly on the UI render thread where the window handle was initialized.
    ///
    /// # Errors
    /// Returns [`TouchError`] if polling actions fails.
    pub fn dispatch_to_window(
        &mut self,
        window: &MinimalSoftwareWindow,
    ) -> Result<usize, TouchError> {
        let actions = self.poll_actions()?;
        let count = actions.len();

        for action in actions {
            match action {
                TouchAction::Down { x, y } => {
                    let position = LogicalPosition::new(x, y);
                    window.dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                }
                TouchAction::Move { x, y } => {
                    let position = LogicalPosition::new(x, y);
                    window.dispatch_event(WindowEvent::PointerMoved { position });
                }
                TouchAction::Up { x, y } => {
                    let position = LogicalPosition::new(x, y);
                    window.dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                    // Immediately clear sticky hover state on touchscreen release
                    window.dispatch_event(WindowEvent::PointerExited);
                }
            }
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn evdev_open_and_poll_raw_hardware_events() {
        let mock = Arc::new(MockEvdevSyscall::new());
        let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;
        let device_path = PathBuf::from("/dev/input/event0");
        mock.register_path(&device_path, MockDeviceConfig::default());

        let mut dev = TouchInputDevice::open_with(&device_path, Arc::clone(&syscall))
            .expect("open_with failed");
        assert_eq!(dev.query_device_name(), "raspberrypi-ts");
        let fd = dev.as_raw_fd().expect("expected raw fd");
        assert!(fd >= 100);

        // Push Down, Move, Up events
        let mut event_bytes = Vec::new();
        // Down (ABS_X, ABS_Y, BTN_TOUCH 1, SYN)
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(EV_ABS, ABS_X, 100)));
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(EV_ABS, ABS_Y, 120)));
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_KEY, BTN_TOUCH, 1,
        )));
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_SYN, SYN_REPORT, 0,
        )));

        // Move (ABS_X, SYN)
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(EV_ABS, ABS_X, 150)));
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_SYN, SYN_REPORT, 0,
        )));

        // Up (BTN_TOUCH 0, SYN)
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_KEY, BTN_TOUCH, 0,
        )));
        event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_SYN, SYN_REPORT, 0,
        )));

        mock.push_raw_events(fd, &event_bytes);

        let actions = dev.poll_actions().expect("poll_actions failed");
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0], TouchAction::Down { x: 100.0, y: 120.0 });
        assert_eq!(actions[1], TouchAction::Move { x: 150.0, y: 120.0 });
        assert_eq!(actions[2], TouchAction::Up { x: 150.0, y: 120.0 });

        // Second poll encounters WouldBlock and returns empty
        assert_eq!(
            dev.poll_actions().expect("poll empty"),
            Vec::<TouchAction>::new()
        );

        // Drop dev and verify descriptor closed
        drop(dev);
        assert!(mock.is_closed(fd));
    }

    #[test]
    fn evdev_partial_chunk_buffering() {
        let mock = Arc::new(MockEvdevSyscall::new());
        let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;
        let device_path = PathBuf::from("/dev/input/event1");
        mock.register_path(&device_path, MockDeviceConfig::default());

        let mut dev = TouchInputDevice::open_with(&device_path, Arc::clone(&syscall))
            .expect("open_with failed");
        let fd = dev.as_raw_fd().unwrap();

        let ev1 = make_input_event(EV_ABS, ABS_X, 200);
        let ev1_bytes = input_event_to_bytes(&ev1);
        let split_idx = 10;
        assert!(split_idx < ev1_bytes.len());

        // Push partial bytes
        mock.push_raw_events(fd, &ev1_bytes[..split_idx]);
        assert_eq!(
            dev.poll_actions().expect("poll partial"),
            Vec::<TouchAction>::new()
        );

        // Push remaining bytes + syn
        let mut rest = Vec::new();
        rest.extend_from_slice(&ev1_bytes[split_idx..]);
        rest.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_KEY, BTN_TOUCH, 1,
        )));
        rest.extend_from_slice(&input_event_to_bytes(&make_input_event(
            EV_SYN, SYN_REPORT, 0,
        )));

        mock.push_raw_events(fd, &rest);
        let actions = dev.poll_actions().expect("poll assembled");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], TouchAction::Down { x: 200.0, y: 0.0 });
    }

    #[test]
    fn evdev_open_and_ioctl_fault_injections() {
        let mock = Arc::new(MockEvdevSyscall::new());
        let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;

        // 1. Open failure
        let p_not_found = PathBuf::from("/dev/input/missing");
        let err = TouchInputDevice::open_with(&p_not_found, Arc::clone(&syscall)).unwrap_err();
        assert!(err.is_io());

        let p_denied = PathBuf::from("/dev/input/denied");
        mock.register_path(
            &p_denied,
            MockDeviceConfig {
                fail_open: Some(io::ErrorKind::PermissionDenied),
                ..Default::default()
            },
        );
        let err = TouchInputDevice::open_with(&p_denied, Arc::clone(&syscall)).unwrap_err();
        assert!(err.is_io());

        // 2. Nonblocking fcntl failure
        let p_nb_fail = PathBuf::from("/dev/input/nb_fail");
        mock.register_path(
            &p_nb_fail,
            MockDeviceConfig {
                fail_nonblocking: true,
                ..Default::default()
            },
        );
        let err = TouchInputDevice::open_with(&p_nb_fail, Arc::clone(&syscall)).unwrap_err();
        assert!(err.is_io());

        // 3. ABS ioctl complete failure
        let p_abs_fail = PathBuf::from("/dev/input/abs_fail");
        mock.register_path(
            &p_abs_fail,
            MockDeviceConfig {
                fail_abs: true,
                ..Default::default()
            },
        );
        let err = TouchInputDevice::open_with(&p_abs_fail, Arc::clone(&syscall)).unwrap_err();
        assert!(err.is_io());

        // 4. Invalid calibration bounds (min >= max)
        let p_bad_bounds = PathBuf::from("/dev/input/bad_bounds");
        mock.register_path(
            &p_bad_bounds,
            MockDeviceConfig {
                abs_x: libc::input_absinfo {
                    value: 0,
                    minimum: 800,
                    maximum: 800,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
                ..Default::default()
            },
        );
        let err = TouchInputDevice::open_with(&p_bad_bounds, Arc::clone(&syscall)).unwrap_err();
        assert!(format!("{err}").contains("Invalid touch axis limits"));

        // 5. ST abs fails, MT abs succeeds (fallback branch)
        let p_mt_fallback = PathBuf::from("/dev/input/mt_fallback");
        mock.register_path(
            &p_mt_fallback,
            MockDeviceConfig {
                fail_st_abs_only: true,
                ..Default::default()
            },
        );
        let dev = TouchInputDevice::open_with(&p_mt_fallback, Arc::clone(&syscall))
            .expect("MT fallback should succeed");
        assert_eq!(dev.decoder.calibration.max_x, 800.0);

        // 6. query_name failure fallback
        let p_bad_name = PathBuf::from("/dev/input/bad_name");
        mock.register_path(
            &p_bad_name,
            MockDeviceConfig {
                fail_name: true,
                ..Default::default()
            },
        );
        let dev_bad_name = TouchInputDevice::open_with(&p_bad_name, Arc::clone(&syscall)).unwrap();
        assert_eq!(dev_bad_name.query_device_name(), "Unknown Evdev Device");

        // 7. read error handling
        let p_read_err = PathBuf::from("/dev/input/read_err");
        mock.register_path(
            &p_read_err,
            MockDeviceConfig {
                fail_read: Some(io::ErrorKind::BrokenPipe),
                ..Default::default()
            },
        );
        let mut dev_read_err =
            TouchInputDevice::open_with(&p_read_err, Arc::clone(&syscall)).unwrap();
        let err = dev_read_err.poll_actions().unwrap_err();
        assert!(err.is_io());
    }

    #[test]
    fn evdev_auto_discover_scenarios() {
        let temp = tempfile::tempdir().expect("tempdir");
        let temp_path = temp.path();

        let mock = Arc::new(MockEvdevSyscall::new());
        let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;

        // Scenario 1: Empty directory -> NotFound
        let err = TouchInputDevice::auto_discover_in(temp_path, &syscall).unwrap_err();
        assert!(err.is_device_not_found());

        // Scenario 2: event0 is a Keyboard, event1 is TouchScreen
        let ev0 = temp_path.join("event0");
        let ev1 = temp_path.join("event1");
        std::fs::write(&ev0, b"").unwrap();
        std::fs::write(&ev1, b"").unwrap();

        mock.register_path(
            &ev0,
            MockDeviceConfig {
                name: "Standard Keyboard".to_string(),
                ..Default::default()
            },
        );
        mock.register_path(
            &ev1,
            MockDeviceConfig {
                name: "Freenove TouchScreen Device".to_string(),
                ..Default::default()
            },
        );

        let dev = TouchInputDevice::auto_discover_in(temp_path, &syscall)
            .expect("auto_discover should find event1");
        assert_eq!(dev.query_device_name(), "Freenove TouchScreen Device");

        // Scenario 3: event0 is directly a touchscreen
        mock.register_path(
            &ev0,
            MockDeviceConfig {
                name: "raspberrypi-ts".to_string(),
                ..Default::default()
            },
        );
        let dev0 = TouchInputDevice::auto_discover_in(temp_path, &syscall)
            .expect("auto_discover should find event0 directly");
        assert_eq!(dev0.query_device_name(), "raspberrypi-ts");
    }

    #[test]
    fn native_evdev_syscall_with_pipes() {
        let native = NativeEvdevSyscall;
        let mut fds = [0; 2];
        // SAFETY: pipe initializes the 2-element c_int array with valid file descriptors.
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(ret, 0);
        let [rfd, wfd] = fds;

        // Test set_nonblocking on pipe
        native
            .set_nonblocking(rfd)
            .expect("set_nonblocking on pipe");

        // Read on empty nonblocking pipe -> WouldBlock
        let mut buf = [0u8; 16];
        let err = native.read(rfd, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        // Write to pipe and read
        let data = b"touch-data";
        // SAFETY: writing into a valid OS pipe descriptor with accurate length.
        let written = unsafe { libc::write(wfd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
        assert_eq!(written, 10);

        let n = native.read(rfd, &mut buf).expect("read from pipe");
        assert_eq!(&buf[..n], data);

        // Close pipe fds
        native.close(rfd).expect("close rfd");
        native.close(wfd).expect("close wfd");
    }

    #[test]
    fn touch_error_comprehensive() {
        let err_io = TouchError::io(
            "/dev/input/event0",
            io::Error::new(io::ErrorKind::NotFound, "not found"),
        );
        assert!(err_io.is_io());
        assert!(!err_io.is_device_not_found());
        assert!(!err_io.is_mock_fault());
        let _ = err_io.backtrace();
        assert!(format!("{err_io}").contains("Touch I/O error"));
        assert!(err_io.source().is_some());

        let err_dnf = TouchError::device_not_found("No input found");
        assert!(err_dnf.is_device_not_found());
        assert!(format!("{err_dnf}").contains("Touch device not found"));
        assert!(err_dnf.source().is_none());

        let err_cfg = TouchError::invalid_config("Zero span axis");
        assert!(format!("{err_cfg}").contains("Invalid touch configuration"));
        assert!(err_cfg.source().is_none());
    }

    #[test]
    fn multi_touch_protocol_b_tracking_id() {
        let mut decoder = TouchDecoder::new(TouchCalibration::default());

        // MT Tracking ID >= 0 indicates down
        decoder.process_event(&make_input_event(EV_ABS, ABS_MT_POSITION_X, 300));
        decoder.process_event(&make_input_event(EV_ABS, ABS_MT_POSITION_Y, 200));
        decoder.process_event(&make_input_event(EV_ABS, ABS_MT_TRACKING_ID, 42));
        let down = decoder.process_event(&make_input_event(EV_SYN, SYN_REPORT, 0));
        assert_eq!(down, Some(TouchAction::Down { x: 300.0, y: 200.0 }));

        // MT Tracking ID -1 indicates release (Up)
        decoder.process_event(&make_input_event(EV_ABS, ABS_MT_TRACKING_ID, -1));
        let up = decoder.process_event(&make_input_event(EV_SYN, SYN_REPORT, 0));
        assert_eq!(up, Some(TouchAction::Up { x: 300.0, y: 200.0 }));
    }

    #[test]
    fn production_wrappers_and_calibration_setters() {
        // Test production open on nonexistent path
        let err = TouchInputDevice::open("/dev/input/nonexistent-test-node").unwrap_err();
        assert!(err.is_io());

        // Test set_calibration on device
        let (mut dev, _) = TouchInputDevice::new_mocked();
        dev.set_calibration(TouchCalibration {
            invert_x: true,
            invert_y: true,
            swap_xy: true,
            ..Default::default()
        });
        assert!(dev.decoder.calibration().swap_xy);

        // Test NaN calibration rejection
        let nan_cal = TouchCalibration {
            min_x: f32::NAN,
            ..Default::default()
        };
        assert!(!nan_cal.is_finite());
        assert_eq!(nan_cal.map_to_screen(100, 100, 800.0, 480.0), (0.0, 0.0));
        dev.set_calibration(nan_cal);
        assert!(dev.decoder.calibration().is_finite());

        // NativeEvdevSyscall error paths with invalid fd (-1)
        let native = NativeEvdevSyscall;
        let _ = native.set_nonblocking(-1).unwrap_err();
        let _ = native.read(-1, &mut [0u8; 8]).unwrap_err();
        let _ = native.query_abs(-1, ABS_X).unwrap_err();
        let _ = native.query_name(-1).unwrap_err();
        let _ = native.close(-1).unwrap_err();
    }
}
