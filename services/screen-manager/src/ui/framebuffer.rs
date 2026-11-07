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
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::PhysicalSize;
use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
    TargetPixel,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};

/// `FBIO_WAITFORVSYNC` ioctl request number (`_IOW('F', 0x20, u32)`), blocks until the
/// display controller's next vertical blanking interval.
const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4004_4620;

thread_local! {
    static ACTIVE_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    /// Whether the primary appliance window has already been claimed by a component.
    /// The first component created after [`Framebuffer::init_platform`] binds to the
    /// primary window; later components (offscreen marquee strip rasterizers) receive
    /// independent throwaway windows.
    static ACTIVE_WINDOW_CLAIMED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The most recently created secondary (offscreen) window, kept as a weak reference so
    /// callers can bind freshly created offscreen components to their own window.
    static LAST_SECONDARY_WINDOW: RefCell<Option<std::rc::Weak<MinimalSoftwareWindow>>> =
        const { RefCell::new(None) };
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
        if !bytes.len().is_multiple_of(4) {
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

speaker_common::impl_canonical_error_display!(FramebufferError);

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

/// Output synchronization strategy selected when a hardware device is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanoutSync {
    /// Real fbdev character device (`bcm2708_fb`): rendering targets the cached RAM
    /// shadow buffer and only the dirty rectangle is blitted into the uncached scanout
    /// mapping, optionally paced to the hardware vertical blanking interval
    /// (`vsync` probe succeeded).
    CharDevice { vsync: bool },
    /// Regular file mapping (host test/dev fallback): legacy buffered line path with
    /// an `msync` flush to persist the mapping.
    FileMapped,
}

/// Phase timing of the most recent [`Framebuffer::render_frame`] call.
///
/// Aggregated by the render loop to attribute CPU cost to Slint scene drawing, the
/// hardware vertical blanking wait, and the scanout blit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTiming {
    /// Slint `draw_if_needed` scene traversal and rasterization duration.
    pub draw_us: u64,
    /// `FBIO_WAITFORVSYNC` blocking duration (zero when unsupported or not drawn).
    pub vsync_us: u64,
    /// Dirty-rectangle shadow-buffer-to-scanout copy (and msync flush) duration.
    pub blit_us: u64,
    /// Dirty bounding-box area in pixels (zero when nothing was drawn).
    pub dirty_px: u64,
    /// Dirty bounding box `(x, y, width, height)` in pixels.
    pub dirty_box: (u32, u32, u32, u32),
    /// Rust-side marquee row composition duration (zero when no overlay is active).
    pub compose_us: u64,
}

impl FrameTiming {
    /// Total non-blocking CPU time attributable to this frame.
    #[must_use]
    pub const fn cpu_us(&self) -> u64 {
        self.draw_us + self.compose_us + self.blit_us
    }
}

/// A single marquee row to be composed by Rust directly into the shadow buffer.
///
/// Produced per render-loop iteration from the shared [`MarqueeCompose`] Slint global
/// (see `crate::ui::shell`); consumed by [`Framebuffer::set_marquee_rows`].
#[derive(Debug, Clone)]
pub struct MarqueeRowSpec {
    /// Viewport rectangle `(x, y, width, height)` in framebuffer pixels.
    pub rect: (u32, u32, u32, u32),
    /// Current leftward scroll distance in whole pixels (`0..cycle_px`).
    pub scroll_px: u32,
    /// Tile advance distance in pixels (strip width plus the inter-copy gap).
    pub cycle_px: u32,
    /// Premultiplied RGBA strip pixels in row-major order.
    pub strip: Arc<[crate::ui::marquee::RgbaPremulPixel]>,
    /// Strip width in pixels (`strip.len() / strip_h`).
    pub strip_w: u32,
    /// Stable monotonic strip identity assigned by the strip cache. Unlike heap
    /// addresses this survives reallocation and never collides after re-rasterization,
    /// so it is the sole identity signal used by the composer.
    pub strip_id: u64,
}

/// Identifies when a row's composed content must be refreshed: scroll moved or the
/// strip pixel buffer was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowFingerprint {
    scroll_px: u32,
    strip_id: u64,
}

/// Per-row overlay state: clean background snapshot plus the last composed fingerprint.
#[derive(Debug)]
struct MarqueeRowState {
    rect: (u32, u32, u32, u32),
    /// Clean background snapshot in BGRX pixels, `width * height`, row-major.
    background: Box<[Bgrx8888Pixel]>,
    last: Option<RowFingerprint>,
}

/// Rust-side marquee composer state owned by the framebuffer.
///
/// While active, the Slint scene keeps the marquee strip images static; every animation
/// tick this overlay restores the captured clean row background into the shadow buffer,
/// alpha-composites the scrolled strip tiles over it (opaque pixels copy directly; only
/// antialiased glyph edges blend), and extends the scanout blit to cover the rows.
#[derive(Debug, Default)]
pub struct MarqueeOverlay {
    rows: Vec<(MarqueeRowSpec, MarqueeRowState)>,
}

impl MarqueeOverlay {
    fn row_pixels(rect: &(u32, u32, u32, u32)) -> usize {
        let (_, _, w, h) = *rect;
        usize::saturating_mul(w as usize, h as usize)
    }

    /// Replaces the pending row specifications, retaining captured backgrounds for rows
    /// whose rectangle is unchanged.
    pub fn set_rows(&mut self, rows: Vec<MarqueeRowSpec>) {
        self.rows = rows
            .into_iter()
            .map(|spec| {
                let prev = self
                    .rows
                    .iter()
                    .find(|(existing, _)| existing.rect == spec.rect)
                    .map_or_else(
                        || MarqueeRowState {
                            rect: spec.rect,
                            background: Box::from([]),
                            last: None,
                        },
                        |(existing, state)| {
                            // An identical spec (same rect, scroll, cycle, and strip
                            // buffer) keeps the previous compose fingerprint so the
                            // composer skips unchanged rows instead of recomposing at
                            // the raw render-loop rate.
                            let unchanged = existing.scroll_px == spec.scroll_px
                                && existing.cycle_px == spec.cycle_px
                                && existing.strip_w == spec.strip_w
                                && existing.strip_id == spec.strip_id;
                            MarqueeRowState {
                                rect: state.rect,
                                background: state.background.clone(),
                                last: if unchanged { state.last } else { None },
                            }
                        },
                    );
                (spec, prev)
            })
            .collect();
    }

    /// Installs captured background snapshots (one per row, in row order).
    pub fn install_backgrounds(&mut self, backgrounds: Vec<Box<[Bgrx8888Pixel]>>) {
        for ((_, state), bg) in self.rows.iter_mut().zip(backgrounds) {
            state.background = bg;
            state.last = None;
        }
    }

    /// Forces recomposition of rows whose rect intersects a Slint-drawn dirty
    /// region. A Slint redraw rewrites the row area with the plain background
    /// (strips are invisible in compose mode), so cached fingerprints for those
    /// rows are stale until the strip is composed again.
    pub fn invalidate_rows_intersecting(&mut self, dirty: (u32, u32, u32, u32)) {
        let (dx, dy, dw, dh) = dirty;
        let (dx2, dy2) = (dx.saturating_add(dw), dy.saturating_add(dh));
        for (_, state) in &mut self.rows {
            let (rx, ry, rw, rh) = state.rect;
            let (rx2, ry2) = (rx.saturating_add(rw), ry.saturating_add(rh));
            let intersects = dx < rx2 && rx < dx2 && dy < ry2 && ry < dy2;
            if intersects {
                state.last = None;
            }
        }
    }

    /// Whether every row has a captured background snapshot.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.rows.is_empty()
            && self
                .rows
                .iter()
                .all(|(_, state)| state.background.len() == Self::row_pixels(&state.rect))
    }

    /// Composites rows whose fingerprint changed since the last frame into `back_buffer`
    /// and returns the union bounding box of the composed rows (if any).
    fn compose_changed(
        &mut self,
        back_buffer: &mut [Bgrx8888Pixel],
        width: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        let mut dirty: Option<(usize, usize, usize, usize)> = None;
        let height = back_buffer.len().checked_div(width).unwrap_or(0);
        for (spec, state) in &mut self.rows {
            let fingerprint = RowFingerprint {
                scroll_px: spec.scroll_px,
                strip_id: spec.strip_id,
            };
            if state.last == Some(fingerprint) || state.background.is_empty() {
                continue;
            }
            state.last = Some(fingerprint);
            let (rx, ry, rw, rh) = state.rect;
            let (rx, ry, rw, rh) = (rx as usize, ry as usize, rw as usize, rh as usize);
            if rw == 0 || rh == 0 || rx.saturating_add(rw) > width || ry.saturating_add(rh) > height
            {
                continue;
            }
            let strip_w = usize::try_from(spec.strip_w).unwrap_or(0);
            let strip_h = spec.strip.len().checked_div(strip_w).unwrap_or(0);
            for line in 0..rh {
                let dst = &mut back_buffer[(ry + line) * width + rx..(ry + line) * width + rx + rw];
                let bg_start = line * rw;
                let bg_end = bg_start + rw;
                match state.background.get(bg_start..bg_end) {
                    Some(bg) if bg.len() == rw => dst.copy_from_slice(bg),
                    _ => continue,
                }
                if line < strip_h {
                    compose_scanned_line(dst, spec, line);
                }
            }
            dirty = Some(match dirty {
                None => (rx, ry, rw, rh),
                Some((x, y, w, h)) => (
                    rx.min(x),
                    ry.min(y),
                    rx.saturating_add(rw).max(x.saturating_add(w)) - rx.min(x),
                    ry.saturating_add(rh).max(y.saturating_add(h)) - ry.min(y),
                ),
            });
        }
        dirty
    }
}

/// Composites one scanline of scrolled, tiled strip pixels over a freshly restored
/// background scanline.
fn compose_scanned_line(dst: &mut [Bgrx8888Pixel], spec: &MarqueeRowSpec, line: usize) {
    let strip_w = u64::from(spec.strip_w);
    let scroll = u64::from(spec.scroll_px);
    let cycle = u64::from(spec.cycle_px);
    if strip_w == 0 || cycle == 0 {
        return;
    }
    let dst_len = dst.len() as u64;
    // Strip tiles repeat every `cycle` px; tile k's left edge sits at `k * cycle - scroll`
    // in viewport coordinates. Every tile whose left edge is left of the viewport's right
    // edge contributes (clipped) content or inter-tile background gap.
    let mut k: u64 = 0;
    while k * cycle <= scroll + dst_len {
        let tile_left = k * cycle;
        k += 1;
        let (start, src_off) = if tile_left >= scroll {
            (usize::try_from(tile_left - scroll).unwrap_or(0), 0_usize)
        } else {
            (
                0_usize,
                usize::try_from(scroll - tile_left).unwrap_or(usize::MAX),
            )
        };
        if src_off >= strip_w as usize {
            continue; // tile body ended left of the viewport; only its gap remains
        }
        if start >= dst.len() {
            continue;
        }
        let strip_row_len = strip_w as usize;
        let row_base = line.saturating_mul(strip_row_len);
        for (i, dst_px) in dst[start..].iter_mut().enumerate() {
            let src_x = src_off + i;
            if src_x >= strip_row_len {
                break; // inter-tile gap: the remainder keeps the background
            }
            let Some(src) = spec.strip.get(row_base.wrapping_add(src_x)) else {
                break;
            };
            match src.alpha {
                0 => {}
                255 => {
                    dst_px.red = src.red;
                    dst_px.green = src.green;
                    dst_px.blue = src.blue;
                    dst_px.alpha = 255;
                }
                a => {
                    // Premultiplied-over blend against the opaque background.
                    let inv = u16::from(u8::MAX - a);
                    let blend = |dst_c: u8, src_c: u8| -> u8 {
                        let mixed = u16::from(dst_c) * inv / 255 + u16::from(src_c);
                        u8::try_from(mixed).unwrap_or(u8::MAX)
                    };
                    dst_px.red = blend(dst_px.red, src.red);
                    dst_px.green = blend(dst_px.green, src.green);
                    dst_px.blue = blend(dst_px.blue, src.blue);
                    dst_px.alpha = 255;
                }
            }
        }
    }
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
        sync: ScanoutSync,
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

/// Slint platform adapter for direct Linux framebuffer rendering.
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferPlatform;

impl Platform for FramebufferPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, PlatformError> {
        let claimed = ACTIVE_WINDOW_CLAIMED.with(core::cell::Cell::get);
        if !claimed {
            ACTIVE_WINDOW_CLAIMED.with(|cell| cell.set(true));
            return Ok(get_or_create_active_window(None) as Rc<dyn slint::platform::WindowAdapter>);
        }
        // Secondary components get their own offscreen window (full redraw is fine for
        // the tiny single-frame rasterizer surfaces).
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        LAST_SECONDARY_WINDOW.with(|cell| *cell.borrow_mut() = Some(Rc::downgrade(&window)));
        Ok(window)
    }
}

/// Returns the most recently created secondary (offscreen) window, if it is still alive.
#[must_use]
pub(crate) fn take_last_secondary_window() -> Option<Rc<MinimalSoftwareWindow>> {
    LAST_SECONDARY_WINDOW.with(|cell| cell.borrow_mut().take().and_then(|weak| weak.upgrade()))
}

/// Linux framebuffer display controller managing memory-mapped video RAM and sysfs unblanking.
///
/// The appliance display panel is physically fixed at 800x480 32bpp BGRX; all geometry
/// is compile-time constant and validated at startup against the sysfs `virtual_size`.
#[derive(Debug)]
pub struct Framebuffer {
    backend: Backend,
    last_timing: core::cell::Cell<Option<FrameTiming>>,
    force_full_redraw: core::cell::Cell<bool>,
    marquee_overlay: MarqueeOverlay,
}

// Compile-time proofs for the fixed appliance panel geometry. The const
// multiplication in `SCANOUT_BYTE_LEN` is itself a compile error on overflow,
// proving the fixed 800x480x4 buffer always fits in `usize`.
const _: () = {
    assert!(Framebuffer::DEFAULT_WIDTH == 800 && Framebuffer::DEFAULT_HEIGHT == 480);
};

/// Line-by-line render target into cached RAM pixels.
///
/// The hardware scanout mapping is uncached device memory, so all Slint rendering is
/// routed through the persistent cached shadow buffer ([`FbLineBuffer::direct`]) and
/// only the resulting dirty rectangle is copied into video RAM afterwards.
struct FbLineBuffer<'a> {
    pixels: &'a mut [Bgrx8888Pixel],
    stride: usize,
}

impl<'a> FbLineBuffer<'a> {
    /// Renders directly into cached RAM `pixels` (shadow buffer, mocks, file backends).
    fn direct(pixels: &'a mut [Bgrx8888Pixel], stride: usize) -> Self {
        Self { pixels, stride }
    }
}

impl LineBufferProvider for FbLineBuffer<'_> {
    type TargetPixel = Bgrx8888Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let offset = line * self.stride + range.start;
        render_fn(&mut self.pixels[offset..offset + range.len()]);
    }
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
        let back_buffer =
            vec![Bgrx8888Pixel::default(); Self::SCANOUT_BYTE_LEN / Self::BYTES_PER_PIXEL];

        // Real fbdev character devices render through the cached RAM shadow buffer and
        // pace blits to the hardware VBlank when the driver supports `FBIO_WAITFORVSYNC`.
        // Regular-file mappings keep the legacy buffered line path with an msync flush.
        let sync = if is_fb_char_device {
            ScanoutSync::CharDevice {
                vsync: Self::wait_for_vsync(&file),
            }
        } else {
            ScanoutSync::FileMapped
        };

        Ok(Self {
            last_timing: core::cell::Cell::new(None),
            force_full_redraw: core::cell::Cell::new(false),
            marquee_overlay: MarqueeOverlay::default(),
            backend: Backend::Hardware {
                file,
                mmap,
                dev_path: dev_path_str,
                blank_path: blank_path_str,
                back_buffer,
                sync,
            },
        })
    }

    /// Validates the complete scanout contract for an fb* device against its sysfs attributes,
    /// failing closed when any required attribute is missing.
    ///
    /// # Errors
    /// Returns [`FramebufferError`] if attributes are missing, malformed, or mismatch fixed panel specs.
    pub fn validate_fb_scanout_contract(
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

    /// Validates sysfs `virtual_size` matches the expected panel dimensions.
    ///
    /// # Errors
    /// Returns [`FramebufferError`] if size does not match panel specs or is malformed.
    pub fn validate_sysfs_geometry(
        sysfs_path: &Path,
        dev_name: &str,
    ) -> Result<(), FramebufferError> {
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

    /// Validates sysfs `bits_per_pixel` matches the expected 32bpp format.
    ///
    /// # Errors
    /// Returns [`FramebufferError`] if bpp does not match 32bpp or is malformed.
    pub fn validate_sysfs_bits_per_pixel(
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

    /// Validates sysfs `stride` matches tightly packed 3200 bytes per line.
    ///
    /// # Errors
    /// Returns [`FramebufferError`] if stride is not tightly packed or is malformed.
    pub fn validate_sysfs_stride(
        sysfs_path: &Path,
        dev_name: &str,
    ) -> Result<(), FramebufferError> {
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
            last_timing: core::cell::Cell::new(None),
            force_full_redraw: core::cell::Cell::new(false),
            marquee_overlay: MarqueeOverlay::default(),
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
        // A fresh platform registration cycle re-arms the primary-window claim so the
        // next component binds to the primary appliance window.
        ACTIVE_WINDOW_CLAIMED.with(|cell| cell.set(false));

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

    /// Renders one frame through the `mmap`-backed scanout path (`VSync` pacing + dirty blit).
    #[expect(
        clippy::too_many_arguments,
        reason = "all parameters are destructured backend fields of a single render call"
    )]
    fn render_mmap_frame(
        overlay: &mut MarqueeOverlay,
        last_timing: &core::cell::Cell<Option<FrameTiming>>,
        window: &MinimalSoftwareWindow,
        file: &mut File,
        mmap_mut: &mut memmap2::MmapMut,
        dev_path: &str,
        back_buffer: &mut [Bgrx8888Pixel],
        sync: ScanoutSync,
        width: usize,
        force_full: bool,
    ) -> Result<bool, FramebufferError> {
        let pixel_slice = Bgrx8888Pixel::try_from_bytes_mut(mmap_mut)?;
        let mut timing = FrameTiming::default();
        let mut dirty = (0_usize, 0_usize, 0_usize, 0_usize);
        let draw_start = Instant::now();
        if force_full {
            window.request_redraw();
        }

        let did_draw = window.draw_if_needed(|renderer| {
            if force_full {
                renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            }
            let region = renderer.render_by_line(FbLineBuffer::direct(back_buffer, width));
            if force_full {
                renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
            }
            let origin = region.bounding_box_origin();
            let size = region.bounding_box_size();
            dirty = (
                usize::try_from(origin.x).unwrap_or(0),
                usize::try_from(origin.y).unwrap_or(0),
                usize::try_from(size.width).unwrap_or(0),
                usize::try_from(size.height).unwrap_or(0),
            );
        });
        timing.draw_us = Self::elapsed_us(draw_start);
        let dirty_px = u64::try_from(dirty.2 * dirty.3).unwrap_or(u64::MAX);
        timing.dirty_px = dirty_px;
        timing.dirty_box = (
            u32::try_from(dirty.0).unwrap_or(0),
            u32::try_from(dirty.1).unwrap_or(0),
            u32::try_from(dirty.2).unwrap_or(0),
            u32::try_from(dirty.3).unwrap_or(0),
        );
        let compose_start = Instant::now();
        let slint_dirty = (
            u32::try_from(dirty.0).unwrap_or(0),
            u32::try_from(dirty.1).unwrap_or(0),
            u32::try_from(dirty.2).unwrap_or(0),
            u32::try_from(dirty.3).unwrap_or(0),
        );
        overlay.invalidate_rows_intersecting(slint_dirty);
        let marquee_dirty = overlay.compose_changed(back_buffer, width);
        timing.compose_us = Self::elapsed_us(compose_start);
        let effective = did_draw || marquee_dirty.is_some();
        let blit_box = Self::union_dirty_boxes(dirty, marquee_dirty);
        if effective {
            let (blit_x, blit_y, blit_w, blit_h) = blit_box;
            if matches!(sync, ScanoutSync::CharDevice { vsync: true }) {
                // The vsync wait itself paces this frame to the hardware refresh; the
                // boolean result is informational only.
                let vsync_start = Instant::now();
                let _ = Self::wait_for_vsync(file);
                timing.vsync_us = Self::elapsed_us(vsync_start);
            }
            let blit_start = Instant::now();
            Self::blit_dirty_rect(
                back_buffer,
                pixel_slice,
                width,
                blit_x,
                blit_y,
                blit_w,
                blit_h,
            );
            if sync == ScanoutSync::FileMapped {
                mmap_mut
                    .flush()
                    .map_err(|e| FramebufferError::io(dev_path.to_owned(), e))?;
            }
            timing.blit_us = Self::elapsed_us(blit_start);
            let marquee_px =
                u64::try_from(marquee_dirty.map_or(0, |(_, _, w, h)| w.saturating_mul(h)))
                    .unwrap_or(u64::MAX);
            timing.dirty_px = dirty_px.saturating_add(marquee_px);
            timing.dirty_box = (
                u32::try_from(blit_x).unwrap_or(0),
                u32::try_from(blit_y).unwrap_or(0),
                u32::try_from(blit_w).unwrap_or(0),
                u32::try_from(blit_h).unwrap_or(0),
            );
        }
        last_timing.set(Some(timing));
        Ok(effective)
    }

    /// Renders one frame through the regular-file mapping path (full flush after draw).
    #[expect(
        clippy::too_many_arguments,
        reason = "all parameters are destructured backend fields of a single render call"
    )]
    fn render_file_frame(
        overlay: &mut MarqueeOverlay,
        last_timing: &core::cell::Cell<Option<FrameTiming>>,
        window: &MinimalSoftwareWindow,
        file: &mut File,
        dev_path: &str,
        back_buffer: &mut [Bgrx8888Pixel],
        width: usize,
        force_full: bool,
    ) -> Result<bool, FramebufferError> {
        let mut timing = FrameTiming::default();
        let draw_start = Instant::now();
        if force_full {
            window.request_redraw();
        }
        let did_draw = window.draw_if_needed(|renderer| {
            if force_full {
                renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            }
            renderer.render_by_line(FbLineBuffer::direct(back_buffer, width));
            if force_full {
                renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
            }
        });
        timing.draw_us = Self::elapsed_us(draw_start);
        let compose_start = Instant::now();
        // The file-mapped path has no dirty tracking (it always flushes fully);
        // any Slint draw may have overwritten the row area, so invalidate all.
        if did_draw {
            overlay.invalidate_rows_intersecting((0, 0, u32::MAX, u32::MAX));
        }
        let marquee_dirty = overlay.compose_changed(back_buffer, width);
        timing.compose_us = Self::elapsed_us(compose_start);
        let effective = did_draw || marquee_dirty.is_some();
        if effective {
            Self::flush_file_buffer(file, Bgrx8888Pixel::as_bytes(back_buffer), dev_path)?;
        }
        last_timing.set(Some(timing));
        Ok(effective)
    }

    /// Renders a pending frame into the framebuffer if redraw was requested.
    ///
    /// On hardware (`/dev/fb0` character device), Slint renders into the cached RAM
    /// shadow buffer, the frame is paced to the hardware vertical blanking interval
    /// when supported, and only the dirty rectangle is blitted into the uncached
    /// scanout mapping. On regular-file mappings, the dirty rectangle is blitted and
    /// the mapping is flushed with `msync`.
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
        let force_full = self.force_full_redraw.replace(false);

        match &mut self.backend {
            Backend::Hardware {
                file,
                mmap,
                dev_path,
                back_buffer,
                sync,
                ..
            } => match mmap {
                Some(mmap_mut) => Self::render_mmap_frame(
                    &mut self.marquee_overlay,
                    &self.last_timing,
                    window,
                    file,
                    mmap_mut,
                    dev_path,
                    back_buffer,
                    *sync,
                    width,
                    force_full,
                ),
                None => Self::render_file_frame(
                    &mut self.marquee_overlay,
                    &self.last_timing,
                    window,
                    file,
                    dev_path,
                    back_buffer,
                    width,
                    force_full,
                ),
            },
            Backend::Mock { buffer, ctrl } => {
                let mut state = ctrl.state.lock().expect("mock state lock poisoned");
                if let Some(message) = &state.render_error {
                    return Err(FramebufferError::io(
                        "mock-fb",
                        io::Error::other(message.to_string()),
                    ));
                }
                let mut timing = FrameTiming::default();
                let draw_start = Instant::now();
                if force_full {
                    window.request_redraw();
                }
                let did_draw = window.draw_if_needed(|renderer| {
                    if force_full {
                        renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
                    }
                    renderer.render_by_line(FbLineBuffer::direct(buffer, width));
                    if force_full {
                        renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
                    }
                });
                timing.draw_us = Self::elapsed_us(draw_start);
                let compose_start = Instant::now();
                let marquee_dirty = self.marquee_overlay.compose_changed(buffer, width);
                timing.compose_us = Self::elapsed_us(compose_start);
                let effective = did_draw || marquee_dirty.is_some();
                if effective {
                    state.render_calls += 1;
                }
                self.last_timing.set(Some(timing));
                Ok(effective)
            }
        }
    }

    /// Renders one frame in which Slint repaints the WHOLE window regardless of its
    /// dirty state. Used for marquee background capture: with partial rendering the
    /// hidden strips leave no dirty region, so the shadow buffer would keep stale
    /// strip pixels that would be baked into the captured background (ghost text).
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] when the underlying scanout write fails.
    pub fn render_frame_full_redraw(
        &mut self,
        window: &MinimalSoftwareWindow,
    ) -> Result<bool, FramebufferError> {
        self.force_full_redraw.set(true);
        self.render_frame(window)
    }

    /// Returns the phase timing of the most recent [`render_frame`](Self::render_frame) call.
    #[must_use]
    pub fn last_frame_timing(&self) -> Option<FrameTiming> {
        self.last_timing.get()
    }

    /// Replaces the pending Rust-side marquee row specifications.
    pub fn set_marquee_rows(&mut self, rows: Vec<MarqueeRowSpec>) {
        self.marquee_overlay.set_rows(rows);
    }

    /// Whether the marquee overlay has a captured background for every row.
    #[must_use]
    pub fn marquee_overlay_ready(&self) -> bool {
        self.marquee_overlay.is_ready()
    }

    /// Installs captured background snapshots (one per row, in [`Self::set_marquee_rows`]
    /// row order) produced by [`Self::capture_region`].
    pub fn install_marquee_backgrounds(&mut self, backgrounds: Vec<Box<[Bgrx8888Pixel]>>) {
        self.marquee_overlay.install_backgrounds(backgrounds);
    }

    /// Copies a rectangular region of the current shadow-buffer content as a clean
    /// background snapshot for the marquee composer.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if the region exceeds the fixed panel bounds or the
    /// mock state mutex is poisoned.
    pub fn capture_region(
        &self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Box<[Bgrx8888Pixel]>, FramebufferError> {
        let width = Self::DEFAULT_WIDTH as usize;
        let height = Self::DEFAULT_HEIGHT as usize;
        let (x, y, w, h) = (
            usize::try_from(x).unwrap_or(0),
            usize::try_from(y).unwrap_or(0),
            usize::try_from(w).unwrap_or(0),
            usize::try_from(h).unwrap_or(0),
        );
        if w == 0 || h == 0 || x.saturating_add(w) > width || y.saturating_add(h) > height {
            return Err(FramebufferError::invalid_geometry(
                u32::try_from(x).unwrap_or(u32::MAX),
                u32::try_from(y).unwrap_or(u32::MAX),
                format!(
                    "marquee background region ({x}, {y}, {w}, {h}) exceeds the {width}x{height} panel"
                ),
            ));
        }
        let mut snapshot = Vec::with_capacity(w * h);
        match &self.backend {
            Backend::Hardware { back_buffer, .. } => {
                for line in 0..h {
                    let start = (y + line) * width + x;
                    snapshot.extend_from_slice(&back_buffer[start..start + w]);
                }
            }
            Backend::Mock { buffer, ctrl } => {
                // Hold the state mutex for the snapshot's lifetime; the buffer is read
                // through , the guard only serializes concurrent mock mutation.
                let _state = ctrl.state.lock().map_err(|poisoned| {
                    FramebufferError::io(
                        "mock-fb",
                        io::Error::other(format!("mock state lock poisoned: {poisoned}")),
                    )
                })?;
                for line in 0..h {
                    let start = (y + line) * width + x;
                    snapshot.extend_from_slice(&buffer[start..start + w]);
                }
            }
        }
        Ok(snapshot.into_boxed_slice())
    }

    /// Unions two dirty boxes expressed as `(x, y, width, height)`; zero-area boxes are
    /// treated as absent.
    fn union_dirty_boxes(
        a: (usize, usize, usize, usize),
        b: Option<(usize, usize, usize, usize)>,
    ) -> (usize, usize, usize, usize) {
        let Some((bx, by, bw, bh)) = b else {
            return a;
        };
        if bw == 0 || bh == 0 {
            return a;
        }
        if a.2 == 0 || a.3 == 0 {
            return (bx, by, bw, bh);
        }
        (
            a.0.min(bx),
            a.1.min(by),
            a.0.saturating_add(a.2).max(bx.saturating_add(bw)) - a.0.min(bx),
            a.1.saturating_add(a.3).max(by.saturating_add(bh)) - a.1.min(by),
        )
    }

    /// Blocks until the display controller's next vertical blanking interval.
    ///
    /// Issues the `FBIO_WAITFORVSYNC` ioctl on the framebuffer file descriptor so the
    /// render loop is paced by the panel's hardware refresh clock instead of drifting
    /// software timers.
    ///
    /// Returns `true` if the hardware `VBlank` wait succeeded, or `false` when the file
    /// descriptor does not support the ioctl (regular files, mocks, other fb drivers).
    #[must_use]
    pub fn wait_for_vsync(file: &File) -> bool {
        static VSYNC_CONSECUTIVE_FAILURES: AtomicUsize = AtomicUsize::new(0);
        // `FBIO_WAITFORVSYNC` copies a `u32` CRTC index from userspace; `0` selects the
        // primary display output on `bcm2708_fb`.
        let crtc: libc::c_uint = 0;
        // SAFETY: `file` is an open file descriptor and `crtc` outlives the syscall.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), FBIO_WAITFORVSYNC, &crtc) };
        if result == -1 {
            let count = VSYNC_CONSECUTIVE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(error = %std::io::Error::last_os_error(), consecutive_failures = count, "VSync ioctl failed");
            return false;
        }
        VSYNC_CONSECUTIVE_FAILURES.store(0, Ordering::Relaxed);
        result == 0
    }

    /// Microseconds elapsed since `start`.
    #[must_use]
    fn elapsed_us(start: Instant) -> u64 {
        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// Copies the dirty rectangle scanlines from the cached shadow buffer into the
    /// uncached scanout mapping (write-only, no read-modify-write bus traffic).
    fn blit_dirty_rect(
        back_buffer: &[Bgrx8888Pixel],
        scanout: &mut [Bgrx8888Pixel],
        stride: usize,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) {
        let x_end = x.saturating_add(width).min(stride);
        let y_end = y.saturating_add(height).min(scanout.len() / stride);
        if x >= x_end || y >= y_end {
            return;
        }
        for row in y..y_end {
            let offset = row * stride;
            let dest = &mut scanout[offset + x..offset + x_end];
            dest.copy_from_slice(&back_buffer[offset + x..offset + x_end]);
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

    /// Returns a slice of the cached RAM shadow render buffer.
    ///
    /// On hardware backends this is the persistent buffer Slint renders into before
    /// the dirty rectangle is blitted into the scanout mapping.
    #[must_use]
    pub fn shadow_buffer(&self) -> &[Bgrx8888Pixel] {
        match &self.backend {
            Backend::Hardware { back_buffer, .. }
            | Backend::Mock {
                buffer: back_buffer,
                ..
            } => back_buffer,
        }
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

    /// Whether this controller is targeting the physical appliance panel `/dev/fb0`.
    #[must_use]
    pub fn is_hardware_panel(&self) -> bool {
        match &self.backend {
            Backend::Hardware { dev_path, .. } => dev_path.as_ref() == Self::DEFAULT_FB_PATH,
            Backend::Mock { .. } => false,
        }
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

/// Internal framebuffer test helpers exported strictly under the `test-util` feature
/// for external integration testing per Microsoft Pragmatic Rust Guidelines (`M-TEST-UTIL`).
#[cfg(feature = "test-util")]
pub mod test_helpers {
    pub use super::FramebufferPlatform;
}
