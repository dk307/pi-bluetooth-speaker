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

use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::RefCell;
use std::fmt::{self, Display, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::rc::Rc;

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
    /// # Panics
    ///
    /// Panics if `bytes.len()` is not a multiple of 4.
    #[must_use]
    pub fn from_bytes_mut(bytes: &mut [u8]) -> &mut [Self] {
        assert!(
            bytes.len() % 4 == 0,
            "byte slice length must be a multiple of 4"
        );
        // SAFETY: Bgrx8888Pixel has alignment 1, size 4, and any 4-byte sequence is valid.
        unsafe {
            std::slice::from_raw_parts_mut(
                bytes.as_mut_ptr().cast::<Bgrx8888Pixel>(),
                bytes.len() / 4,
            )
        }
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
pub enum FramebufferErrorKind {
    /// I/O error accessing a framebuffer device or sysfs path.
    Io {
        /// Target filesystem path.
        path: Box<str>,
        /// Descriptive error message.
        message: Box<str>,
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

    pub(crate) fn io(path: impl Into<Box<str>>, message: impl Into<Box<str>>) -> Self {
        Self::new(FramebufferErrorKind::Io {
            path: path.into(),
            message: message.into(),
        })
    }

    pub(crate) fn invalid_geometry(width: u32, height: u32, message: impl Into<Box<str>>) -> Self {
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

    /// Returns the underlying error kind.
    #[must_use]
    pub const fn kind(&self) -> &FramebufferErrorKind {
        &self.kind
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FramebufferErrorKind::Io { path, message } => {
                write!(f, "framebuffer I/O error on '{path}': {message}")
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

    /// Single-line summary without backtrace.
    #[must_use]
    pub fn summary(&self) -> String {
        struct Summary<'a>(&'a FramebufferError);
        impl Display for Summary<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                self.0.fmt_kind(f)
            }
        }
        Summary(self).to_string()
    }
}

impl Display for FramebufferError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_kind(f)?;
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\nBacktrace:\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl std::error::Error for FramebufferError {}

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
        unblank_calls: usize,
        render_calls: usize,
    },
}

struct FramebufferPlatform;

impl Platform for FramebufferPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, PlatformError> {
        ACTIVE_WINDOW.with(|w| {
            if let Some(active) = &*w.borrow() {
                Ok(Rc::clone(active) as Rc<dyn slint::platform::WindowAdapter>)
            } else {
                let new_win = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
                Ok(new_win as Rc<dyn slint::platform::WindowAdapter>)
            }
        })
    }
}

/// Linux framebuffer display controller managing memory-mapped video RAM and sysfs unblanking.
#[derive(Debug)]
pub struct Framebuffer {
    backend: Backend,
    width: u32,
    height: u32,
}

impl Framebuffer {
    /// Default display panel width in pixels (Freenove 4.3" DSI).
    pub const DEFAULT_WIDTH: u32 = 800;
    /// Default display panel height in pixels (Freenove 4.3" DSI).
    pub const DEFAULT_HEIGHT: u32 = 480;
    /// Default Linux framebuffer device path.
    pub const DEFAULT_FB_PATH: &'static str = "/dev/fb0";
    /// Default Linux framebuffer sysfs blank control path.
    pub const DEFAULT_BLANK_PATH: &'static str = "/sys/class/graphics/fb0/blank";

    /// Connects to the primary hardware framebuffer (`/dev/fb0`) at standard 800x480 resolution.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if `/dev/fb0` cannot be opened.
    pub fn new() -> Result<Self, FramebufferError> {
        Self::open(
            Self::DEFAULT_FB_PATH,
            Some(Self::DEFAULT_BLANK_PATH),
            Self::DEFAULT_WIDTH,
            Self::DEFAULT_HEIGHT,
        )
    }

    /// Opens a specific framebuffer device path and optional sysfs blank path.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if dimensions are zero or the device cannot be opened.
    pub fn open(
        dev_path: impl AsRef<Path>,
        blank_path: Option<impl AsRef<Path>>,
        width: u32,
        height: u32,
    ) -> Result<Self, FramebufferError> {
        if width == 0 || height == 0 {
            return Err(FramebufferError::invalid_geometry(
                width,
                height,
                "width and height must both be non-zero",
            ));
        }

        let dev_path_ref = dev_path.as_ref();
        let dev_path_str: Box<str> = dev_path_ref.to_string_lossy().into();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev_path_ref)
            .map_err(|err| FramebufferError::io(dev_path_str.clone(), err.to_string()))?;

        let byte_len = usize::try_from(width)
            .map_err(|err| FramebufferError::invalid_geometry(width, height, err.to_string()))?
            .saturating_mul(usize::try_from(height).map_err(|err| {
                FramebufferError::invalid_geometry(width, height, err.to_string())
            })?)
            .saturating_mul(4);

        // Attempt memory mapping; if mmap fails (e.g., standard temp file in tests),
        // fallback to file write with internal backing buffer.
        //
        // SAFETY: The file is opened with read and write permissions, byte_len is computed
        // from valid width * height * 4, and the mapping does not exceed device boundaries.
        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .len(byte_len)
                .map_mut(&file)
                .ok()
        };

        let blank_path_str = blank_path.map(|p| p.as_ref().to_string_lossy().into());
        let pixel_count = (width as usize).saturating_mul(height as usize);
        let back_buffer = vec![Bgrx8888Pixel::default(); pixel_count];

        Ok(Self {
            backend: Backend::Hardware {
                file,
                mmap,
                dev_path: dev_path_str,
                blank_path: blank_path_str,
                back_buffer,
            },
            width,
            height,
        })
    }

    /// Creates an in-memory mocked framebuffer controller for hermetic unit testing.
    #[must_use]
    pub fn new_mocked(width: u32, height: u32) -> Self {
        let pixel_count = (width as usize).saturating_mul(height as usize);
        Self {
            backend: Backend::Mock {
                buffer: vec![Bgrx8888Pixel::default(); pixel_count],
                unblank_calls: 0,
                render_calls: 0,
            },
            width,
            height,
        }
    }

    /// Unblanks the physical display panel by writing `0` to the sysfs blank control file.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if writing to sysfs fails.
    pub fn unblank(&mut self) -> Result<(), FramebufferError> {
        match &mut self.backend {
            Backend::Hardware { blank_path, .. } => {
                if let Some(path) = blank_path {
                    let path_ref = Path::new(&**path);
                    if path_ref.exists() {
                        let mut f = OpenOptions::new()
                            .write(true)
                            .open(path_ref)
                            .map_err(|err| FramebufferError::io(path.clone(), err.to_string()))?;
                        f.write_all(b"0\n")
                            .map_err(|err| FramebufferError::io(path.clone(), err.to_string()))?;
                        f.flush()
                            .map_err(|err| FramebufferError::io(path.clone(), err.to_string()))?;
                    }
                }
                Ok(())
            }
            Backend::Mock { unblank_calls, .. } => {
                *unblank_calls += 1;
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
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(PhysicalSize::new(self.width, self.height));

        ACTIVE_WINDOW.with(|w| {
            *w.borrow_mut() = Some(Rc::clone(&window));
        });

        // Set platform once per thread; if already set, ignore the AlreadySet error.
        let _ = slint::platform::set_platform(Box::new(FramebufferPlatform));

        Ok(window)
    }

    /// Renders a pending frame into the framebuffer if redraw was requested.
    ///
    /// Returns `true` if a frame was rendered, or `false` if no redraw was pending.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if flushing pixels to the hardware device fails.
    pub fn render_frame(
        &mut self,
        window: &MinimalSoftwareWindow,
    ) -> Result<bool, FramebufferError> {
        let width = self.width as usize;
        let mut render_err: Option<FramebufferError> = None;

        let rendered = match &mut self.backend {
            Backend::Hardware {
                file,
                mmap,
                dev_path,
                back_buffer,
                ..
            } => {
                let dev_path_clone = dev_path.clone();
                if let Some(mmap_mut) = mmap {
                    let pixel_slice = Bgrx8888Pixel::from_bytes_mut(mmap_mut);
                    let did_draw = window.draw_if_needed(|renderer| {
                        renderer.render(pixel_slice, width);
                    });
                    if did_draw {
                        if let Err(e) = mmap_mut.flush() {
                            render_err = Some(FramebufferError::io(dev_path_clone, e.to_string()));
                        }
                    }
                    did_draw
                } else {
                    let did_draw = window.draw_if_needed(|renderer| {
                        renderer.render(back_buffer, width);
                    });
                    if did_draw {
                        if let Err(e) = file.seek(SeekFrom::Start(0)) {
                            render_err =
                                Some(FramebufferError::io(dev_path_clone.clone(), e.to_string()));
                        } else if let Err(e) = file.write_all(Bgrx8888Pixel::as_bytes(back_buffer))
                        {
                            render_err =
                                Some(FramebufferError::io(dev_path_clone.clone(), e.to_string()));
                        } else if let Err(e) = file.flush() {
                            render_err = Some(FramebufferError::io(dev_path_clone, e.to_string()));
                        }
                    }
                    did_draw
                }
            }
            Backend::Mock {
                buffer,
                render_calls,
                ..
            } => {
                let did_draw = window.draw_if_needed(|renderer| {
                    renderer.render(buffer, width);
                });
                if did_draw {
                    *render_calls += 1;
                }
                did_draw
            }
        };

        if let Some(err) = render_err {
            return Err(err);
        }

        Ok(rendered)
    }

    /// Returns display width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Returns display height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Total number of pixels.
    #[must_use]
    pub fn pixel_count(&self) -> usize {
        (self.width as usize).saturating_mul(self.height as usize)
    }

    /// Total scanout buffer size in bytes (4 bytes per pixel).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.pixel_count().saturating_mul(4)
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

    /// Returns the number of unblank operations recorded.
    #[must_use]
    pub const fn unblank_count(&self) -> usize {
        match self.backend {
            Backend::Hardware { .. } => 0,
            Backend::Mock { unblank_calls, .. } => unblank_calls,
        }
    }

    /// Returns the number of frames rendered.
    #[must_use]
    pub const fn render_count(&self) -> usize {
        match self.backend {
            Backend::Hardware { .. } => 0,
            Backend::Mock { render_calls, .. } => render_calls,
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
        let pixels = Bgrx8888Pixel::from_bytes_mut(&mut raw);
        assert_eq!(pixels.len(), 1);
        assert_eq!(pixels[0].blue, 0xAA);
        assert_eq!(pixels[0].green, 0xBB);
        assert_eq!(pixels[0].red, 0xCC);
        assert_eq!(pixels[0].alpha, 0xDD);
    }

    #[test]
    fn bgrx_target_pixel_blending() {
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
        let mut fb = Framebuffer::new_mocked(800, 480);
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
    fn framebuffer_invalid_geometry() {
        let err = Framebuffer::open("/dev/null", None::<&Path>, 0, 480).unwrap_err();
        assert!(matches!(
            err.kind(),
            FramebufferErrorKind::InvalidGeometry { .. }
        ));
        assert!(err.summary().contains("invalid framebuffer geometry"));

        let err_h = Framebuffer::open("/dev/null", None::<&Path>, 800, 0).unwrap_err();
        assert!(matches!(
            err_h.kind(),
            FramebufferErrorKind::InvalidGeometry { .. }
        ));
    }

    #[test]
    fn framebuffer_nonexistent_device() {
        let err = Framebuffer::open("/dev/nonexistent_fb_device_12345", None::<&Path>, 800, 480)
            .unwrap_err();
        assert!(matches!(err.kind(), FramebufferErrorKind::Io { .. }));
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

        let mut fb = Framebuffer::open(&dev, None::<&Path>, 800, 480).unwrap();
        assert_eq!(fb.unblank_count(), 0);
        assert_eq!(fb.render_count(), 0);
        fb.unblank().unwrap();
    }

    #[test]
    fn framebuffer_hardware_tempfile_fallback() {
        let temp = ScopedTempDir::try_new("fb_hw_test").unwrap();
        let dev_file = temp.path().join("fake_fb");
        let blank_file = temp.path().join("fake_blank");

        fs::write(&dev_file, vec![0u8; 800 * 480 * 4]).unwrap();
        fs::write(&blank_file, b"1\n").unwrap();

        let mut fb = Framebuffer::open(&dev_file, Some(&blank_file), 800, 480).unwrap();
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
        let err = FramebufferError::io("/dev/fb0", "Permission denied");
        let sum = err.summary();
        assert!(sum.contains("/dev/fb0"));
        assert!(sum.contains("Permission denied"));
        let disp = err.to_string();
        assert!(disp.contains("framebuffer I/O error on '/dev/fb0'"));

        let plat_err = FramebufferError::platform("failed to set context");
        assert!(
            plat_err
                .summary()
                .contains("slint platform registration error")
        );
    }
}
