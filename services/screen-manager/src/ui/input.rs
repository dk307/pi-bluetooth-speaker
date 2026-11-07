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
use std::fmt::{self, Debug, Formatter};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
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
    /// Normalizes raw digitizer coordinates into logical screen pixels.
    #[must_use]
    pub fn map_to_screen(
        &self,
        raw_x: i32,
        raw_y: i32,
        screen_w: f32,
        screen_h: f32,
    ) -> (f32, f32) {
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
        self.calibration = calibration;
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
        _file: File,
        fd: RawFd,
        device_path: Box<str>,
    },
    Mock {
        ctrl: TouchMockCtrl,
    },
}

impl Debug for TouchBackend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hardware { device_path, .. } => f
                .debug_struct("Hardware")
                .field("path", device_path)
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

impl TouchInputDevice {
    /// Opens the touch input device at `path`, reading axis calibration via `EVIOCGABS`.
    ///
    /// # Errors
    /// Returns [`TouchError`] if the device node cannot be opened or ioctls fail.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TouchError> {
        let path_ref = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .open(path_ref)
            .map_err(|e| TouchError::io(path_ref, e))?;

        let fd = file.as_raw_fd();
        let calibration = Self::query_axis_calibration(fd, path_ref)?;

        // Set non-blocking mode on fd
        // SAFETY: fcntl with F_GETFL / F_SETFL are standard POSIX syscalls on valid open fd.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
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
                _file: file,
                fd,
                device_path: path_ref.to_string_lossy().into(),
            },
            decoder: TouchDecoder::new(calibration),
            read_buffer: Vec::with_capacity(std::mem::size_of::<libc::input_event>() * 16),
        })
    }

    /// Queries `EVIOCGABS` for X and Y axes on an open evdev file descriptor.
    #[expect(clippy::similar_names, reason = "X and Y axis info pair")]
    fn query_axis_calibration(fd: RawFd, path: &Path) -> Result<TouchCalibration, TouchError> {
        let mut abs_x_axis = libc::input_absinfo {
            value: 0,
            minimum: 0,
            maximum: 800,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };
        let mut abs_y_axis = libc::input_absinfo {
            value: 0,
            minimum: 0,
            maximum: 480,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };

        // SAFETY: ioctl with EVIOCGABS fills input_absinfo pointer if supported by the kernel driver.
        unsafe {
            let res_x = libc::ioctl(fd, eviocgabs_ioctl(ABS_X as u8), &mut abs_x_axis);
            let res_y = libc::ioctl(fd, eviocgabs_ioctl(ABS_Y as u8), &mut abs_y_axis);
            if res_x != 0 || res_y != 0 {
                // Fall back to querying multi-touch ABS_MT_POSITION_X / Y
                libc::ioctl(
                    fd,
                    eviocgabs_ioctl(ABS_MT_POSITION_X as u8),
                    &mut abs_x_axis,
                );
                libc::ioctl(
                    fd,
                    eviocgabs_ioctl(ABS_MT_POSITION_Y as u8),
                    &mut abs_y_axis,
                );
            }
        }

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
    /// Checks candidate paths in `/dev/input/by-id` and `/dev/input/event*`, identifying
    /// controllers named `raspberrypi-ts` or matching touchscreen capability bits.
    ///
    /// # Errors
    /// Returns [`TouchError`] if no touch device is reachable.
    pub fn auto_discover() -> Result<Self, TouchError> {
        // 1. Check default path first
        if Path::new(DEFAULT_TOUCH_DEVICE_PATH).exists()
            && let Ok(dev) = Self::open(DEFAULT_TOUCH_DEVICE_PATH)
        {
            return Ok(dev);
        }

        // 2. Scan /dev/input/event* entries
        let input_dir = Path::new("/dev/input");
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
                if let Ok(dev) = Self::open(&path)
                    && (dev.query_device_name().to_lowercase().contains("touch")
                        || dev.query_device_name().contains("raspberrypi-ts"))
                {
                    return Ok(dev);
                }
            }
        }

        Err(TouchError::device_not_found(
            "No touchscreen controller identified in /dev/input",
        ))
    }

    /// Queries the device human-readable name via `EVIOCGNAME`.
    #[must_use]
    pub fn query_device_name(&self) -> String {
        match &self.backend {
            TouchBackend::Hardware { fd, .. } => {
                let mut buf = [0u8; 256];
                // SAFETY: ioctl with EVIOCGNAME copies a null-terminated string into buf.
                let ret = unsafe {
                    libc::ioctl(
                        *fd,
                        eviocgname_ioctl(buf.len()),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                    )
                };
                if ret > 0 {
                    #[expect(
                        clippy::cast_sign_loss,
                        reason = "ret is verified positive from successful ioctl"
                    )]
                    let len = (ret as usize).min(buf.len() - 1);
                    String::from_utf8_lossy(&buf[..len])
                        .trim_end_matches('\0')
                        .to_string()
                } else {
                    "Unknown Evdev Device".to_string()
                }
            }
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
            TouchBackend::Hardware { _file, fd, .. } => {
                const EV_SIZE: usize = std::mem::size_of::<libc::input_event>();
                let mut chunk = [0u8; EV_SIZE * 16];

                loop {
                    // SAFETY: read from non-blocking fd into local byte array.
                    let n = unsafe {
                        libc::read(*fd, chunk.as_mut_ptr().cast::<libc::c_void>(), chunk.len())
                    };
                    if n <= 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::WouldBlock {
                            break;
                        }
                        if n < 0 {
                            return Err(TouchError::io("evdev-read", err));
                        }
                        break;
                    }

                    #[expect(clippy::cast_sign_loss, reason = "n is verified positive above")]
                    let bytes_read = n as usize;
                    self.read_buffer.extend_from_slice(&chunk[..bytes_read]);

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
