//! Linux Framebuffer (`/dev/fb0`) display driver for Raspberry Pi 3.
//!
//! Provides direct software rendering via Slint into the Broadcom BCM2837
//! video core's 32-bit BGRX scanout buffer (`800x480` at 32 bpp).
//!
//! # Hardware Architecture & Pixel Format
//!
//! The Raspberry Pi 3 BCM2837 framebuffer driver (`bcm2708_fb`) configures
//! `/dev/fb0` with `rgba 8/16,8/8,8/0,8/24`:
//! - Blue: byte offset 0 (bits 0..8)
//! - Green: byte offset 1 (bits 8..16)
//! - Red: byte offset 2 (bits 16..24)
//! - Reserved / Unused: byte offset 3 (bits 24..32)
//!
//! [`Bgrx8888Pixel`] matches this 4-byte little-endian format with zero padding
//! and implements [`slint::platform::software_renderer::TargetPixel`] directly,
//! allowing Slint's software renderer to render directly into video memory
//! without an intermediate pixel format conversion step.

use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::fmt::{self, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use slint::PhysicalSize;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{Platform, PlatformError};

thread_local! {
    static ACTIVE_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

/// A 32-bit little-endian BGRX pixel matching the Raspberry Pi 3 `/dev/fb0` format.
///
/// Memory layout (4 bytes, little-endian):
/// - `blue`: bits 0..8
/// - `green`: bits 8..16
/// - `red`: bits 16..24
/// - `alpha`: bits 24..32 (reserved/padding in 32bpp BGRX)
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Bgrx8888Pixel {
    /// Blue color component (0..=255).
    pub blue: u8,
    /// Green color component (0..=255).
    pub green: u8,
    /// Red color component (0..=255).
    pub red: u8,
    /// Reserved/alpha byte (typically 255 for opaque scanout).
    pub alpha: u8,
}

impl Bgrx8888Pixel {
    /// Creates a new opaque BGRX pixel from RGB components.
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self {
            blue,
            green,
            red,
            alpha: 255,
        }
    }

    /// Views a slice of [`Bgrx8888Pixel`] as raw bytes.
    #[must_use]
    pub fn as_bytes(slice: &[Self]) -> &[u8] {
        // SAFETY: Bgrx8888Pixel is repr(C) with four u8 fields, alignment 1,
        // and no uninitialized padding bytes.
        unsafe {
            std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), slice.len().saturating_mul(4))
        }
    }

    /// Mutably views a byte slice as a slice of [`Bgrx8888Pixel`].
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if `bytes.len()` is not a multiple of 4.
    pub fn try_from_bytes_mut(bytes: &mut [u8]) -> Result<&mut [Self], FramebufferError> {
        if bytes.len() % 4 != 0 {
            return Err(FramebufferError::invalid_geometry(
                0,
                0,
                format!("byte slice length {} is not a multiple of 4", bytes.len()),
            ));
        }
        // SAFETY: Bgrx8888Pixel has alignment 1, size 4, and any 4-byte sequence is valid.
        Ok(unsafe {
            std::slice::from_raw_parts_mut(
                bytes.as_mut_ptr().cast::<Bgrx8888Pixel>(),
                bytes.len() / 4,
            )
        })
    }
}

impl TargetPixel for Bgrx8888Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let inv_alpha = u16::from(u8::MAX - color.alpha);
        self.blue = ((u16::from(self.blue) * inv_alpha) / 255) as u8 + color.blue;
        self.green = ((u16::from(self.green) * inv_alpha) / 255) as u8 + color.green;
        self.red = ((u16::from(self.red) * inv_alpha) / 255) as u8 + color.red;
        self.alpha = 255;
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::new(red, green, blue)
    }

    fn background() -> Self {
        Self::new(0, 0, 0)
    }
}

/// Detailed reason for a [`FramebufferError`].
#[derive(Debug)]
pub(crate) enum FramebufferErrorKind {
    /// I/O error accessing a framebuffer device or sysfs path.
    Io {
        /// Target filesystem path.
        path: Box<str>,
        /// Underlying I/O error cause.
        source: io::Error,
    },
    /// Invalid display geometry requested.
    InvalidGeometry {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
        /// Reason for rejection.
        message: Box<str>,
    },
    /// Platform registration error.
    Platform(Box<str>),
}

/// Canonical error type for framebuffer operations.
#[derive(Debug)]
pub struct FramebufferError {
    kind: FramebufferErrorKind,
    backtrace: Backtrace,
}

impl FramebufferError {
    pub(crate) fn new(kind: FramebufferErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Creates an I/O error variant.
    #[must_use]
    pub fn io(path: impl Into<Box<str>>, source: io::Error) -> Self {
        Self::new(FramebufferErrorKind::Io {
            path: path.into(),
            source,
        })
    }

    /// Creates an invalid geometry error variant.
    #[must_use]
    pub fn invalid_geometry(width: u32, height: u32, message: impl Into<Box<str>>) -> Self {
        Self::new(FramebufferErrorKind::InvalidGeometry {
            width,
            height,
            message: message.into(),
        })
    }

    /// Creates a platform error variant.
    #[must_use]
    pub fn platform(message: impl Into<Box<str>>) -> Self {
        Self::new(FramebufferErrorKind::Platform(message.into()))
    }

    /// Returns `true` if this error was caused by a device or sysfs I/O failure.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        matches!(self.kind, FramebufferErrorKind::Io { .. })
    }

    /// Returns the underlying I/O error if this error is an I/O variant.
    #[must_use]
    pub fn io_error(&self) -> Option<&io::Error> {
        match &self.kind {
            FramebufferErrorKind::Io { source, .. } => Some(source),
            _ => None,
        }
    }

    /// Returns `true` if this error was caused by an invalid geometry request.
    #[must_use]
    pub const fn is_invalid_geometry(&self) -> bool {
        matches!(self.kind, FramebufferErrorKind::InvalidGeometry { .. })
    }

    /// Returns `true` if this error was caused by Slint platform registration.
    #[must_use]
    pub const fn is_platform(&self) -> bool {
        matches!(self.kind, FramebufferErrorKind::Platform(_))
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FramebufferErrorKind::Io { path, source } => {
                write!(f, "framebuffer I/O error on '{path}': {source}")
            }
            FramebufferErrorKind::InvalidGeometry {
                width,
                height,
                message,
            } => {
                write!(
                    f,
                    "invalid framebuffer geometry {width}x{height}: {message}"
                )
            }
            FramebufferErrorKind::Platform(msg) => {
                write!(f, "slint platform registration error: {msg}")
            }
        }
    }
}

crate::hardware::impl_canonical_error_display!(FramebufferError);

impl std::error::Error for FramebufferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            FramebufferErrorKind::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Creates an invalid-geometry error for the fixed appliance panel geometry.
fn invalid_panel_geometry(message: impl Into<Box<str>>) -> FramebufferError {
    FramebufferError::invalid_geometry(
        Framebuffer::DEFAULT_WIDTH,
        Framebuffer::DEFAULT_HEIGHT,
        message,
    )
}

/// Backend implementation for the framebuffer sink.
#[derive(Debug)]
enum Backend {
    Hardware {
        file: File,
        mmap: Option<memmap2::MmapMut>,
        dev_path: Box<str>,
        blank_path: Option<Box<str>>,
        back_buffer: Vec<Bgrx8888Pixel>,
    },
    Mock {
        buffer: Vec<Bgrx8888Pixel>,
        ctrl: FramebufferMockCtrl,
    },
}

fn get_or_create_active_window(size: Option<PhysicalSize>) -> Rc<MinimalSoftwareWindow> {
    ACTIVE_WINDOW.with(|w| {
        if let Some(existing) = &*w.borrow() {
            if let Some(s) = size {
                existing.set_size(s);
            }
            Rc::clone(existing)
        } else {
            let win = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            if let Some(s) = size {
                win.set_size(s);
            }
            *w.borrow_mut() = Some(Rc::clone(&win));
            win
        }
    })
}

/// Internal mutable state shared between a mocked [`Framebuffer`] and its controller.
#[derive(Debug, Default)]
struct MockState {
    unblank_calls: usize,
    render_calls: usize,
    unblank_error: Option<Box<str>>,
    render_error: Option<Box<str>>,
}

/// Fault-injection controller paired with a mocked [`Framebuffer`].
///
/// Allows tests to deterministically inject `unblank` and `render_frame` failures
/// so the surrounding error paths are exercised on any host without hardware.
#[derive(Debug, Clone, Default)]
pub struct FramebufferMockCtrl {
    state: Arc<Mutex<MockState>>,
}

impl FramebufferMockCtrl {
    /// Runs `apply` against the shared mock state under the state mutex.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    fn with_state<T>(&self, apply: impl FnOnce(&mut MockState) -> T) -> T {
        let mut state = self.state.lock().expect("mock state lock poisoned");
        apply(&mut state)
    }

    /// Forces all subsequent `unblank` calls to fail with an I/O error.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_unblank_error(&self, message: impl Into<Box<str>>) {
        self.with_state(|state| state.unblank_error = Some(message.into()));
    }

    /// Forces all subsequent `render_frame` calls to fail with an I/O error.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_render_error(&self, message: impl Into<Box<str>>) {
        self.with_state(|state| state.render_error = Some(message.into()));
    }

    /// Clears any injected `unblank` fault.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn clear_unblank_error(&self) {
        self.with_state(|state| state.unblank_error = None);
    }

    /// Clears any injected `render_frame` fault.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn clear_render_error(&self) {
        self.with_state(|state| state.render_error = None);
    }

    /// Takes a snapshot of the mock counters, leaving injected faults untouched.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn counters(&self) -> (usize, usize) {
        self.with_state(|state| (state.unblank_calls, state.render_calls))
    }
}

struct FramebufferPlatform;

impl Platform for FramebufferPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, PlatformError> {
        Ok(get_or_create_active_window(None) as Rc<dyn slint::platform::WindowAdapter>)
    }
}

/// Linux framebuffer display controller managing memory-mapped video RAM and sysfs unblanking.
///
/// The appliance display panel is physically fixed at 800x480 32bpp BGRX; all geometry
/// is compile-time constant and validated at startup against the sysfs `virtual_size`.
#[derive(Debug)]
pub struct Framebuffer {
    backend: Backend,
}

// Compile-time proofs for the fixed appliance panel geometry. The const
// multiplication in `SCANOUT_BYTE_LEN` is itself a compile error on overflow,
// proving the fixed 800x480x4 buffer always fits in `usize`.
const _: () = {
    assert!(Framebuffer::DEFAULT_WIDTH == 800 && Framebuffer::DEFAULT_HEIGHT == 480);
};

impl Framebuffer {
    /// Default display panel width in pixels (Freenove 4.3" DSI).
    pub const DEFAULT_WIDTH: u32 = 800;
    /// Default display panel height in pixels (Freenove 4.3" DSI).
    pub const DEFAULT_HEIGHT: u32 = 480;
    /// Default Linux framebuffer device path.
    pub const DEFAULT_FB_PATH: &'static str = "/dev/fb0";
    /// Default Linux framebuffer sysfs blank control path.
    pub const DEFAULT_BLANK_PATH: &'static str = "/sys/class/graphics/fb0/blank";
    /// Bytes per scanout pixel (32bpp BGRX).
    pub const BYTES_PER_PIXEL: usize = 4;
    /// Scanout color depth in bits per pixel (32bpp BGRX).
    pub const BITS_PER_PIXEL: u32 = Self::BYTES_PER_PIXEL as u32 * 8;
    /// Fixed appliance scanout buffer size in bytes (800x480x4).
    pub const SCANOUT_BYTE_LEN: usize =
        (Self::DEFAULT_WIDTH as usize) * (Self::DEFAULT_HEIGHT as usize) * Self::BYTES_PER_PIXEL;

    /// Connects to the primary hardware framebuffer (`/dev/fb0`) at standard 800x480 resolution.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if `/dev/fb0` cannot be opened.
    pub fn new() -> Result<Self, FramebufferError> {
        Self::open(Self::DEFAULT_FB_PATH, Some(Self::DEFAULT_BLANK_PATH))
    }

    /// Opens a specific framebuffer device path and optional sysfs blank path.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if the device cannot be opened, or if the sysfs
    /// scanout contract (`virtual_size`, `bits_per_pixel`, `stride`) is missing or does
    /// not match the fixed appliance panel geometry.
    pub fn open(
        dev_path: impl AsRef<Path>,
        blank_path: Option<impl AsRef<Path>>,
    ) -> Result<Self, FramebufferError> {
        let dev_path_ref = dev_path.as_ref();
        let dev_path_str: Box<str> = dev_path_ref.to_string_lossy().into();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev_path_ref)
            .map_err(|err| FramebufferError::io(dev_path_str.clone(), err))?;

        // Discriminate the fbdev scanout contract by file type, not file name: only
        // character devices (e.g. /dev/fb0) are validated against sysfs, while regular
        // files that merely start with "fb" keep the mmap/buffered fallback behavior.
        let file_meta = file.metadata();
        let is_fb_char_device = file_meta
            .as_ref()
            .is_ok_and(|m| m.file_type().is_char_device())
            && dev_path_ref
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|dev_name| dev_name.starts_with("fb"));

        if is_fb_char_device {
            let dev_name = dev_path_ref
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("fb0");
            // Failing closed: an fb* device whose sysfs contract is unavailable is
            // rejected rather than mmap'ing unvalidated geometry.
            let sysfs_dir = format!("/sys/class/graphics/{dev_name}");
            Self::validate_fb_scanout_contract(Path::new(&sysfs_dir), dev_name)?;
        }

        // For regular files smaller than the scanout length (e.g. truncated files),
        // avoid mmap to prevent SIGBUS.
        let too_small_regular_file = file_meta
            .is_ok_and(|m| m.file_type().is_file() && m.len() < Self::SCANOUT_BYTE_LEN as u64);

        // Attempt memory mapping; if mmap fails or file is too small,
        // fallback to file write with internal backing buffer.
        let mmap = if too_small_regular_file {
            None
        } else {
            // SAFETY: The file is opened with read and write permissions, the mapping
            // length is the fixed compile-time scanout size, and the truncated regular
            // file case above guarantees the mapping does not exceed file boundaries.
            unsafe {
                memmap2::MmapOptions::new()
                    .len(Self::SCANOUT_BYTE_LEN)
                    .map_mut(&file)
                    .ok()
            }
        };

        let blank_path_str = blank_path.map(|p| p.as_ref().to_string_lossy().into());
        let back_buffer = if mmap.is_some() {
            Vec::new()
        } else {
            vec![Bgrx8888Pixel::default(); Self::SCANOUT_BYTE_LEN / Self::BYTES_PER_PIXEL]
        };

        Ok(Self {
            backend: Backend::Hardware {
                file,
                mmap,
                dev_path: dev_path_str,
                blank_path: blank_path_str,
                back_buffer,
            },
        })
    }

    /// Validates the complete fbdev scanout contract (resolution, pixel format,
    /// stride) for an fb* device against its sysfs attributes, failing closed when
    /// any required attribute is missing.
    fn validate_fb_scanout_contract(
        sysfs_dir: &Path,
        dev_name: &str,
    ) -> Result<(), FramebufferError> {
        for (attr, validate) in [
            (
                "virtual_size",
                Self::validate_sysfs_geometry as fn(&Path, &str) -> Result<(), _>,
            ),
            ("bits_per_pixel", Self::validate_sysfs_bits_per_pixel),
            ("stride", Self::validate_sysfs_stride),
        ] {
            let attr_path = sysfs_dir.join(attr);
            if !attr_path.exists() {
                return Err(invalid_panel_geometry(format!(
                    "framebuffer '{dev_name}' does not expose required sysfs attribute '{attr}' to validate the scanout contract"
                )));
            }
            validate(&attr_path, dev_name)?;
        }
        Ok(())
    }

    fn validate_sysfs_geometry(sysfs_path: &Path, dev_name: &str) -> Result<(), FramebufferError> {
        let content = Self::read_sysfs_content(sysfs_path)?;
        let fields: Vec<&str> = content.split(',').collect();
        if fields.len() != 2 {
            return Err(invalid_panel_geometry(format!(
                "malformed sysfs virtual_size '{content}' on {dev_name}"
            )));
        }
        let dev_w = Self::parse_sysfs_value::<u32>(fields[0], "virtual_size", dev_name)?;
        let dev_h = Self::parse_sysfs_value::<u32>(fields[1], "virtual_size", dev_name)?;
        if dev_w != Self::DEFAULT_WIDTH || dev_h != Self::DEFAULT_HEIGHT {
            return Err(invalid_panel_geometry(format!(
                "requested resolution {}x{} does not match hardware scanout {dev_w}x{dev_h}",
                Self::DEFAULT_WIDTH,
                Self::DEFAULT_HEIGHT
            )));
        }
        Ok(())
    }

    fn read_sysfs_content(sysfs_path: &Path) -> Result<String, FramebufferError> {
        std::fs::read_to_string(sysfs_path)
            .map(|content| content.trim().to_string())
            .map_err(|e| FramebufferError::io(sysfs_path.to_string_lossy().to_string(), e))
    }

    fn parse_sysfs_value<T: std::str::FromStr>(
        content: &str,
        attr: &str,
        dev_name: &str,
    ) -> Result<T, FramebufferError>
    where
        <T as std::str::FromStr>::Err: std::fmt::Display,
    {
        content.parse::<T>().map_err(|e| {
            invalid_panel_geometry(format!(
                "malformed sysfs {attr} '{content}' on {dev_name}: {e}"
            ))
        })
    }

    fn validate_sysfs_bits_per_pixel(
        sysfs_path: &Path,
        dev_name: &str,
    ) -> Result<(), FramebufferError> {
        let content = Self::read_sysfs_content(sysfs_path)?;
        let bpp = Self::parse_sysfs_value::<u32>(&content, "bits_per_pixel", dev_name)?;
        if bpp != Self::BITS_PER_PIXEL {
            return Err(invalid_panel_geometry(format!(
                "framebuffer '{dev_name}' reports {bpp}bpp; the appliance scanout contract requires {}bpp 32-bit BGRX",
                Self::BITS_PER_PIXEL
            )));
        }
        Ok(())
    }

    fn validate_sysfs_stride(sysfs_path: &Path, dev_name: &str) -> Result<(), FramebufferError> {
        let content = Self::read_sysfs_content(sysfs_path)?;
        let stride = Self::parse_sysfs_value::<usize>(&content, "stride", dev_name)?;
        let expected = (Self::DEFAULT_WIDTH as usize) * Self::BYTES_PER_PIXEL;
        if stride != expected {
            return Err(invalid_panel_geometry(format!(
                "framebuffer '{dev_name}' reports stride {stride}; the appliance scanout contract requires tightly packed {expected} bytes per line"
            )));
        }
        Ok(())
    }

    /// Creates an in-memory mocked framebuffer controller for hermetic unit testing.
    ///
    /// The mock always models the fixed appliance panel geometry (800x480) and is
    /// paired with a [`FramebufferMockCtrl`] for deterministic fault injection.
    #[must_use]
    pub fn new_mocked() -> (Self, FramebufferMockCtrl) {
        let ctrl = FramebufferMockCtrl::default();
        let fb = Self {
            backend: Backend::Mock {
                buffer: vec![
                    Bgrx8888Pixel::default();
                    Self::SCANOUT_BYTE_LEN / Self::BYTES_PER_PIXEL
                ],
                ctrl: ctrl.clone(),
            },
        };
        (fb, ctrl)
    }

    /// Unblanks the physical display panel by writing `0` to the sysfs blank control file.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if writing to sysfs fails or a fault is injected.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn unblank(&mut self) -> Result<(), FramebufferError> {
        match &mut self.backend {
            Backend::Hardware { blank_path, .. } => {
                if let Some(path) = blank_path {
                    let path_ref = Path::new(&**path);
                    if path_ref.exists() {
                        let mut f = OpenOptions::new()
                            .write(true)
                            .open(path_ref)
                            .map_err(|err| FramebufferError::io(path.clone(), err))?;
                        f.write_all(b"0\n")
                            .map_err(|err| FramebufferError::io(path.clone(), err))?;
                        f.flush()
                            .map_err(|err| FramebufferError::io(path.clone(), err))?;
                    } else {
                        tracing::warn!(
                            "Sysfs display blank path '{path}' does not exist; skipping unblank"
                        );
                    }
                }
                Ok(())
            }
            Backend::Mock { ctrl, .. } => {
                let mut state = ctrl.state.lock().expect("mock state lock poisoned");
                if let Some(message) = &state.unblank_error {
                    return Err(FramebufferError::io(
                        "mock-fb",
                        io::Error::other(message.to_string()),
                    ));
                }
                state.unblank_calls += 1;
                Ok(())
            }
        }
    }

    /// Initializes Slint's platform layer and returns the associated [`MinimalSoftwareWindow`].
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if Slint platform registration fails.
    pub fn init_platform(&self) -> Result<Rc<MinimalSoftwareWindow>, FramebufferError> {
        let window = get_or_create_active_window(Some(PhysicalSize::new(
            Self::DEFAULT_WIDTH,
            Self::DEFAULT_HEIGHT,
        )));

        // Set platform once per thread; AlreadySet is expected across multiple component cycles.
        match slint::platform::set_platform(Box::new(FramebufferPlatform)) {
            Ok(()) | Err(slint::platform::SetPlatformError::AlreadySet) => Ok(window),
            Err(err) => Err(FramebufferError::platform(err.to_string())),
        }
    }

    /// Flushes raw byte buffer to file using `seek`, `write_all`, and `flush`.
    fn flush_file_buffer(
        file: &mut File,
        buf: &[u8],
        dev_path: &str,
    ) -> Result<(), FramebufferError> {
        file.seek(SeekFrom::Start(0))
            .map_err(|e| FramebufferError::io(dev_path, e))?;
        file.write_all(buf)
            .map_err(|e| FramebufferError::io(dev_path, e))?;
        file.flush()
            .map_err(|e| FramebufferError::io(dev_path, e))?;
        Ok(())
    }

    /// Renders a pending frame into the framebuffer if redraw was requested.
    ///
    /// Returns `true` if a frame was rendered, or `false` if no redraw was pending.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if flushing pixels to the hardware device fails
    /// or a fault is injected.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn render_frame(
        &mut self,
        window: &MinimalSoftwareWindow,
    ) -> Result<bool, FramebufferError> {
        let width = Self::DEFAULT_WIDTH as usize;

        match &mut self.backend {
            Backend::Hardware {
                file,
                mmap,
                dev_path,
                back_buffer,
                ..
            } => {
                if let Some(mmap_mut) = mmap {
                    let pixel_slice = Bgrx8888Pixel::try_from_bytes_mut(mmap_mut)?;
                    let did_draw = window.draw_if_needed(|renderer| {
                        renderer.render(pixel_slice, width);
                    });
                    if did_draw {
                        mmap_mut
                            .flush()
                            .map_err(|e| FramebufferError::io(dev_path.clone(), e))?;
                    }
                    Ok(did_draw)
                } else {
                    let did_draw = window.draw_if_needed(|renderer| {
                        renderer.render(back_buffer, width);
                    });
                    if did_draw {
                        Self::flush_file_buffer(
                            file,
                            Bgrx8888Pixel::as_bytes(back_buffer),
                            dev_path,
                        )?;
                    }
                    Ok(did_draw)
                }
            }
            Backend::Mock { buffer, ctrl } => {
                let mut state = ctrl.state.lock().expect("mock state lock poisoned");
                if let Some(message) = &state.render_error {
                    return Err(FramebufferError::io(
                        "mock-fb",
                        io::Error::other(message.to_string()),
                    ));
                }
                let did_draw = window.draw_if_needed(|renderer| {
                    renderer.render(buffer, width);
                });
                if did_draw {
                    state.render_calls += 1;
                }
                Ok(did_draw)
            }
        }
    }

    /// Returns the fixed display panel width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        Self::DEFAULT_WIDTH
    }

    /// Returns the fixed display panel height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        Self::DEFAULT_HEIGHT
    }

    /// Total number of pixels on the fixed appliance panel.
    #[must_use]
    pub const fn pixel_count(&self) -> usize {
        (Self::DEFAULT_WIDTH as usize) * (Self::DEFAULT_HEIGHT as usize)
    }

    /// Total scanout buffer size in bytes (4 bytes per pixel).
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        Self::SCANOUT_BYTE_LEN
    }

    /// Returns a slice of the rendered pixels.
    #[must_use]
    pub fn pixels(&self) -> &[Bgrx8888Pixel] {
        match &self.backend {
            Backend::Hardware {
                mmap: Some(mmap_mut),
                ..
            } => {
                // SAFETY: mmap is byte_len long and aligned to page boundary.
                unsafe {
                    std::slice::from_raw_parts(
                        mmap_mut.as_ptr().cast::<Bgrx8888Pixel>(),
                        self.pixel_count(),
                    )
                }
            }
            Backend::Hardware { back_buffer, .. } => back_buffer,
            Backend::Mock { buffer, .. } => buffer,
        }
    }

    /// Whether this controller is running on a mock backend.
    #[must_use]
    pub const fn is_mocked(&self) -> bool {
        matches!(self.backend, Backend::Mock { .. })
    }

    /// Returns the number of unblank operations recorded on the mock backend.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn unblank_count(&self) -> usize {
        match &self.backend {
            Backend::Hardware { .. } => 0,
            Backend::Mock { ctrl, .. } => ctrl.counters().0,
        }
    }

    /// Returns the number of frames rendered on the mock backend.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn render_count(&self) -> usize {
        match &self.backend {
            Backend::Hardware { .. } => 0,
            Backend::Mock { ctrl, .. } => ctrl.counters().1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::ScopedTempDir;
    use std::fs;

    #[test]
    fn bgrx_pixel_byte_layout_and_conversion() {
        let p = Bgrx8888Pixel::new(0x12, 0x34, 0x56);
        assert_eq!(p.red, 0x12);
        assert_eq!(p.green, 0x34);
        assert_eq!(p.blue, 0x56);
        assert_eq!(p.alpha, 255);

        let slice = [p];
        let bytes = Bgrx8888Pixel::as_bytes(&slice);
        assert_eq!(bytes, &[0x56, 0x34, 0x12, 0xFF]);

        let mut raw = [0xAA, 0xBB, 0xCC, 0xDD];
        let pixels = Bgrx8888Pixel::try_from_bytes_mut(&mut raw).unwrap();
        assert_eq!(pixels.len(), 1);
        assert_eq!(pixels[0].blue, 0xAA);
        assert_eq!(pixels[0].green, 0xBB);
        assert_eq!(pixels[0].red, 0xCC);
        assert_eq!(pixels[0].alpha, 0xDD);
    }

    #[test]
    fn bgrx_target_pixel_blending() {
        let p_rgb = <Bgrx8888Pixel as TargetPixel>::from_rgb(10, 20, 30);
        assert_eq!(p_rgb, Bgrx8888Pixel::new(10, 20, 30));

        let mut p = Bgrx8888Pixel::background();
        assert_eq!(p, Bgrx8888Pixel::new(0, 0, 0));

        // Blend with fully opaque red color
        p.blend(PremultipliedRgbaColor {
            red: 200,
            green: 100,
            blue: 50,
            alpha: 255,
        });
        assert_eq!(p.red, 200);
        assert_eq!(p.green, 100);
        assert_eq!(p.blue, 50);

        // Blend with 50% transparent overlay
        let mut bg = Bgrx8888Pixel::new(100, 100, 100);
        bg.blend(PremultipliedRgbaColor {
            red: 50,
            green: 0,
            blue: 0,
            alpha: 128,
        });
        assert!(bg.red >= 90);
    }

    #[test]
    fn mock_framebuffer_lifecycle() {
        let (mut fb, _ctrl) = Framebuffer::new_mocked();
        assert_eq!(fb.width(), 800);
        assert_eq!(fb.height(), 480);
        assert_eq!(fb.pixel_count(), 384_000);
        assert_eq!(fb.byte_len(), 1_536_000);
        assert!(fb.is_mocked());
        assert_eq!(fb.unblank_count(), 0);
        assert_eq!(fb.render_count(), 0);

        fb.unblank().unwrap();
        assert_eq!(fb.unblank_count(), 1);

        let window = fb.init_platform().unwrap();
        // Request redraw
        window.request_redraw();

        let drawn = fb.render_frame(&window).unwrap();
        assert!(drawn);
        assert_eq!(fb.render_count(), 1);

        // Subsequent render without redraw request should return false
        let drawn_again = fb.render_frame(&window).unwrap();
        assert!(!drawn_again);
        assert_eq!(fb.render_count(), 1);
    }

    #[test]
    fn mock_fault_injection_unblank_and_render() {
        let (mut fb, ctrl) = Framebuffer::new_mocked();

        ctrl.set_unblank_error("simulated unblank failure");
        let err = fb.unblank().unwrap_err();
        assert!(err.is_io());
        assert!(err.summary().contains("simulated unblank failure"));
        assert_eq!(fb.unblank_count(), 0);
        ctrl.clear_unblank_error();
        fb.unblank().unwrap();
        assert_eq!(fb.unblank_count(), 1);

        let window = fb.init_platform().unwrap();
        window.request_redraw();
        ctrl.set_render_error("simulated render failure");
        let err = fb.render_frame(&window).unwrap_err();
        assert!(err.is_io());
        assert!(err.summary().contains("simulated render failure"));
        assert_eq!(fb.render_count(), 0);
        ctrl.clear_render_error();
        assert!(fb.render_frame(&window).unwrap());
        assert_eq!(fb.render_count(), 1);

        assert_eq!(ctrl.counters(), (1, 1));
    }

    #[test]
    fn framebuffer_nonexistent_device() {
        let err = Framebuffer::open("/dev/nonexistent_fb_device_12345", None::<&Path>).unwrap_err();
        assert!(err.is_io());
        assert!(err.summary().contains("framebuffer I/O error"));
    }

    #[test]
    fn framebuffer_constants_and_hardware_counters() {
        assert_eq!(Framebuffer::DEFAULT_WIDTH, 800);
        assert_eq!(Framebuffer::DEFAULT_HEIGHT, 480);
        assert_eq!(Framebuffer::DEFAULT_FB_PATH, "/dev/fb0");
        assert_eq!(
            Framebuffer::DEFAULT_BLANK_PATH,
            "/sys/class/graphics/fb0/blank"
        );

        let temp = ScopedTempDir::try_new("fb_counts_test").unwrap();
        let dev = temp.path().join("dev_fb");
        fs::write(&dev, vec![0u8; 800 * 480 * 4]).unwrap();

        let mut fb = Framebuffer::open(&dev, None::<&Path>).unwrap();
        assert_eq!(fb.unblank_count(), 0);
        assert_eq!(fb.render_count(), 0);
        fb.unblank().unwrap();
    }

    #[test]
    fn framebuffer_hardware_tempfile_mmap() {
        let temp = ScopedTempDir::try_new("fb_hw_test").unwrap();
        let dev_file = temp.path().join("fake_fb");
        let blank_file = temp.path().join("fake_blank");

        fs::write(&dev_file, vec![0u8; 800 * 480 * 4]).unwrap();
        fs::write(&blank_file, b"1\n").unwrap();

        let mut fb = Framebuffer::open(&dev_file, Some(&blank_file)).unwrap();
        assert!(!fb.is_mocked());
        assert_eq!(fb.unblank_count(), 0);

        fb.unblank().unwrap();
        let blank_content = fs::read_to_string(&blank_file).unwrap();
        assert_eq!(blank_content, "0\n");

        let window = fb.init_platform().unwrap();
        window.request_redraw();
        let drawn = fb.render_frame(&window).unwrap();
        assert!(drawn);
        assert_eq!(fb.pixels().len(), 384_000);
    }

    #[test]
    fn framebuffer_error_display_and_summary() {
        use std::error::Error as _;

        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied");
        let err = FramebufferError::io("/dev/fb0", io_err);
        assert!(err.is_io());
        assert!(!err.is_invalid_geometry());
        assert!(!err.is_platform());
        assert!(err.source().is_some());
        assert!(err.io_error().is_some());
        assert_eq!(
            err.io_error().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = err.backtrace();

        let sum = err.summary();
        assert!(sum.contains("/dev/fb0"));
        assert!(sum.contains("Permission denied"));
        let disp = err.to_string();
        assert!(disp.contains("framebuffer I/O error on '/dev/fb0'"));

        let plat_err = FramebufferError::platform("failed to set context");
        assert!(plat_err.is_platform());
        assert!(!plat_err.is_io());
        assert!(plat_err.source().is_none());
        assert!(plat_err.io_error().is_none());
        assert!(
            plat_err
                .summary()
                .contains("slint platform registration error")
        );

        let geom_err = FramebufferError::invalid_geometry(800, 480, "test mismatch");
        assert!(geom_err.is_invalid_geometry());
        assert!(!geom_err.is_io());
        assert!(geom_err.source().is_none());
        assert!(geom_err.io_error().is_none());
    }

    #[test]
    fn try_from_bytes_mut_validation() {
        let mut valid_bytes = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let pixels = Bgrx8888Pixel::try_from_bytes_mut(&mut valid_bytes).unwrap();
        assert_eq!(pixels.len(), 2);

        let mut invalid_bytes = [1u8, 2, 3];
        let err = Bgrx8888Pixel::try_from_bytes_mut(&mut invalid_bytes).unwrap_err();
        assert!(err.is_invalid_geometry());
        assert!(err.summary().contains("is not a multiple of 4"));
    }

    #[test]
    fn fb_named_regular_file_skips_sysfs_contract() {
        let temp = ScopedTempDir::try_new("fb_named_file_test").unwrap();
        let dev = temp.path().join("fb0");
        fs::write(&dev, vec![0u8; 800 * 480 * 4]).unwrap();
        let fb = Framebuffer::open(&dev, None::<&Path>).unwrap();
        assert!(!fb.is_mocked());
        assert_eq!(fb.pixel_count(), 384_000);
        assert_eq!(fb.byte_len(), 1_536_000);
    }

    #[test]
    fn init_platform_idempotent_window_reuse() {
        let (fb, _ctrl) = Framebuffer::new_mocked();
        let win1 = fb.init_platform().unwrap();
        let win2 = fb.init_platform().unwrap();
        assert!(Rc::ptr_eq(&win1, &win2));
    }

    #[test]
    fn framebuffer_platform_window_adapter_fallback() {
        let platform = FramebufferPlatform;
        let adapter = slint::platform::Platform::create_window_adapter(&platform);
        let _ = adapter.unwrap();
    }

    #[test]
    fn validate_sysfs_geometry_cases() {
        let temp = ScopedTempDir::try_new("sysfs_geom_test").unwrap();
        let valid = temp.path().join("valid");
        fs::write(&valid, "800,480\n").unwrap();
        Framebuffer::validate_sysfs_geometry(&valid, "fb0").unwrap();

        let mismatch = temp.path().join("mismatch");
        fs::write(&mismatch, "1920,1080\n").unwrap();
        let err_mismatch = Framebuffer::validate_sysfs_geometry(&mismatch, "fb0").unwrap_err();
        assert!(err_mismatch.is_invalid_geometry());

        let malformed = temp.path().join("malformed");
        fs::write(&malformed, "800\n").unwrap();
        let err_malformed = Framebuffer::validate_sysfs_geometry(&malformed, "fb0").unwrap_err();
        assert!(err_malformed.is_invalid_geometry());

        let bad_w = temp.path().join("bad_w");
        fs::write(&bad_w, "invalid,480\n").unwrap();
        let err_bad_w = Framebuffer::validate_sysfs_geometry(&bad_w, "fb0").unwrap_err();
        assert!(err_bad_w.is_invalid_geometry());

        let bad_h = temp.path().join("bad_h");
        fs::write(&bad_h, "800,invalid\n").unwrap();
        let err_bad_h = Framebuffer::validate_sysfs_geometry(&bad_h, "fb0").unwrap_err();
        assert!(err_bad_h.is_invalid_geometry());
        let missing = temp.path().join("missing_file");
        let err_missing = Framebuffer::validate_sysfs_geometry(&missing, "fb0").unwrap_err();
        assert!(err_missing.is_io());
    }

    #[test]
    fn validate_sysfs_format_contract_cases() {
        let temp = ScopedTempDir::try_new("sysfs_fmt_test").unwrap();

        let bpp_ok = temp.path().join("bpp_ok");
        fs::write(&bpp_ok, "32\n").unwrap();
        Framebuffer::validate_sysfs_bits_per_pixel(&bpp_ok, "fb0").unwrap();

        let bpp16 = temp.path().join("bpp16");
        fs::write(&bpp16, "16\n").unwrap();
        let err_bpp = Framebuffer::validate_sysfs_bits_per_pixel(&bpp16, "fb0").unwrap_err();
        assert!(err_bpp.is_invalid_geometry());
        assert!(err_bpp.summary().contains("reports 16bpp"));

        let bpp_junk = temp.path().join("bpp_junk");
        fs::write(&bpp_junk, "notanumber\n").unwrap();
        assert!(
            Framebuffer::validate_sysfs_bits_per_pixel(&bpp_junk, "fb0")
                .unwrap_err()
                .summary()
                .contains("malformed sysfs bits_per_pixel")
        );

        let stride_ok = temp.path().join("stride_ok");
        fs::write(&stride_ok, "3200\n").unwrap();
        Framebuffer::validate_sysfs_stride(&stride_ok, "fb0").unwrap();

        let stride_padded = temp.path().join("stride_padded");
        fs::write(&stride_padded, "3840\n").unwrap();
        let err_stride = Framebuffer::validate_sysfs_stride(&stride_padded, "fb0").unwrap_err();
        assert!(err_stride.is_invalid_geometry());
        assert!(err_stride.summary().contains("stride 3840"));

        let stride_missing = temp.path().join("stride_missing");
        assert!(
            Framebuffer::validate_sysfs_stride(&stride_missing, "fb0")
                .unwrap_err()
                .is_io()
        );
    }

    #[test]
    fn fb_scanout_contract_fails_closed_on_missing_attributes() {
        let temp = ScopedTempDir::try_new("fb_contract_test").unwrap();

        // Hermetic: temp dir has no sysfs attributes at all, so the contract
        // must fail closed with a typed invalid_geometry error.
        let err = Framebuffer::validate_fb_scanout_contract(temp.path(), "fb0").unwrap_err();
        assert!(err.is_invalid_geometry());
        assert!(err.summary().contains("virtual_size"));

        // A directory with a complete, matching contract passes.
        let contract_dir = temp.path().join("contract");
        fs::create_dir(&contract_dir).unwrap();
        fs::write(contract_dir.join("virtual_size"), "800,480\n").unwrap();
        fs::write(contract_dir.join("bits_per_pixel"), "32\n").unwrap();
        fs::write(contract_dir.join("stride"), "3200\n").unwrap();
        Framebuffer::validate_fb_scanout_contract(&contract_dir, "fb0").unwrap();

        // Partial contract (missing stride) fails closed.
        let partial_dir = temp.path().join("partial");
        fs::create_dir(&partial_dir).unwrap();
        fs::write(partial_dir.join("virtual_size"), "800,480\n").unwrap();
        fs::write(partial_dir.join("bits_per_pixel"), "32\n").unwrap();
        let err_partial =
            Framebuffer::validate_fb_scanout_contract(&partial_dir, "fb0").unwrap_err();
        assert!(err_partial.is_invalid_geometry());
        assert!(err_partial.summary().contains("stride"));
    }

    #[test]
    fn framebuffer_regular_file_truncated_falls_back_to_buffered() {
        let temp = ScopedTempDir::try_new("fb_truncated_test").unwrap();
        let dev_file = temp.path().join("short_fb");
        // Only write 16 bytes (much smaller than 800 * 480 * 4)
        fs::write(&dev_file, vec![0u8; 16]).unwrap();

        let mut fb = Framebuffer::open(&dev_file, None::<&Path>).unwrap();
        assert!(!fb.is_mocked());
        let window = fb.init_platform().unwrap();
        window.request_redraw();
        let rendered = fb.render_frame(&window).unwrap();
        assert!(rendered);
    }
}
