//! Offscreen software-rendering framebuffer harness for hermetic UI testing.
//!
//! This module is **not** a display driver. Production rendering goes through
//! Slint's `backend-linuxkms` (DRM scanout, see `docs/kms_femtovg_migration_plan.md`);
//! this module only provides an in-memory 800x480 BGRX surface plus a custom
//! [`FramebufferPlatform`] so integration tests can render Slint scenes with the
//! software renderer and inspect the resulting pixels (`capture_region`, `pixels`)
//! on any host without `/dev/fb0` or `/dev/dri`.
//!
//! # Pixel format
//!
//! [`Bgrx8888Pixel`] matches the little-endian 32-bit BGRX layout
//! (`rgba 8/16,8/8,8/0,8/24`) and implements
//! [`slint::platform::software_renderer::TargetPixel`] directly, so the software
//! renderer composites straight into the harness buffer with no conversion step.

use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::fmt::{self, Formatter};
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::PhysicalSize;
use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
    TargetPixel,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};

thread_local! {
    static ACTIVE_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    /// Whether the primary test window has already been claimed by a component.
    /// The first component created after [`Framebuffer::init_platform`] binds to the
    /// primary window; later components receive independent throwaway windows.
    static ACTIVE_WINDOW_CLAIMED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// Exit flag for the hermetic test event loop. Written by [`TestEventLoopProxy`]
    /// (through `slint::quit_event_loop()`, which resolves the *calling* thread's
    /// flag) and read by [`FramebufferPlatform::run_event_loop`] on the loop thread.
    static TEST_LOOP_QUIT: AtomicBool = const { AtomicBool::new(false) };
}

/// Closures queued through `slint::invoke_from_event_loop()` from any thread and
/// drained by the running test event loop.
static TEST_INVOKE_QUEUE: Mutex<Vec<Box<dyn FnOnce() + Send>>> = Mutex::new(Vec::new());

/// Process-wide latch ensuring only the FIRST platform registration contributes an
/// event-loop proxy: `set_platform` stores the proxy in a global `OnceCell` and
/// fails with `AlreadySet` — before claiming the calling thread's Slint context —
/// when a second proxy is offered. Later threads register without a proxy and
/// still get their own context; they signal quit through this single global
/// proxy, which reads the invoking thread's flag at call time.
static TEST_PROXY_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Flags the test event loop running on the *calling* thread to exit.
///
/// `slint::quit_event_loop()` is the normal path; this direct flag is the
/// fallback for contexts where no event-loop proxy was registered.
pub fn signal_test_loop_quit() {
    TEST_LOOP_QUIT.with(|flag| flag.store(true, Ordering::SeqCst));
}

/// Stateless event-loop proxy for the hermetic test backend. It reads the
/// calling thread's quit flag at invocation time, so the single process-global
/// proxy registered by the first `set_platform` call correctly signals whichever
/// thread invokes `quit_event_loop()`.
#[derive(Debug, Clone, Copy)]
struct TestEventLoopProxy;

impl slint::platform::EventLoopProxy for TestEventLoopProxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        signal_test_loop_quit();
        Ok(())
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), slint::EventLoopError> {
        TEST_INVOKE_QUEUE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}

/// A 32-bit little-endian BGRX pixel matching the appliance panel format.
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
    /// Reserved/alpha byte (typically 255 for opaque pixels).
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
    /// I/O error while rendering into the harness buffer or a platform failure detail.
    Io {
        /// Target filesystem path (or pseudo-path such as `mock-fb`).
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

/// Canonical error type for framebuffer harness operations.
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

    /// Returns `true` if this error was caused by a device or file I/O failure.
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

/// Internal mutable state shared between a [`Framebuffer`] and its controller.
#[derive(Debug, Default)]
struct MockState {
    render_calls: usize,
    render_error: Option<Box<str>>,
}

/// Fault-injection controller paired with a [`Framebuffer`].
///
/// Allows tests to deterministically inject `render_frame` failures so the
/// surrounding error paths are exercised on any host without hardware.
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

    /// Forces all subsequent `render_frame` calls to fail with an I/O error.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_render_error(&self, message: impl Into<Box<str>>) {
        self.with_state(|state| state.render_error = Some(message.into()));
    }

    /// Clears any injected `render_frame` fault.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn clear_render_error(&self) {
        self.with_state(|state| state.render_error = None);
    }

    /// Returns the number of successful `render_frame` calls recorded so far.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn counters(&self) -> usize {
        self.with_state(|state| state.render_calls)
    }
}

/// Slint platform adapter providing one primary offscreen window plus throwaway
/// secondary windows for any additional components created on the same thread.
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
        // Secondary components get their own offscreen window.
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        Ok(window)
    }

    /// Hermetic event loop for integration tests: advances Slint timers and
    /// animations, drains the cross-thread invoke queue, and sleeps until the
    /// next timer — with NO rendering (tests drive [`Framebuffer::render_frame`]
    /// explicitly to inspect pixels). Exits when the calling thread's quit flag
    /// is set via `slint::quit_event_loop()` or [`signal_test_loop_quit`].
    fn run_event_loop(&self) -> Result<(), PlatformError> {
        TEST_LOOP_QUIT.with(|flag| flag.store(false, Ordering::SeqCst));
        loop {
            slint::platform::update_timers_and_animations();
            let queued: Vec<Box<dyn FnOnce() + Send>> = TEST_INVOKE_QUEUE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain(..)
                .collect();
            for event in queued {
                event();
            }
            if TEST_LOOP_QUIT.with(|flag| flag.load(Ordering::SeqCst)) {
                break;
            }
            let wait = slint::platform::duration_until_next_timer_update()
                .unwrap_or(Duration::from_millis(50))
                .clamp(Duration::from_millis(1), Duration::from_millis(50));
            std::thread::sleep(wait);
        }
        Ok(())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        if TEST_PROXY_REGISTERED.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(Box::new(TestEventLoopProxy))
        }
    }
}

/// Offscreen render harness managing the in-memory pixel buffer and the thread-local
/// Slint platform registration used by integration tests.
///
/// The buffer geometry is the fixed appliance panel (800x480 32bpp BGRX) and is
/// compile-time constant.
#[derive(Debug)]
pub struct Framebuffer {
    buffer: Vec<Bgrx8888Pixel>,
    ctrl: FramebufferMockCtrl,
    force_full_redraw: core::cell::Cell<bool>,
}

// Compile-time proofs for the fixed appliance panel geometry. The const
// multiplication in `SCANOUT_BYTE_LEN` is itself a compile error on overflow,
// proving the fixed 800x480x4 buffer always fits in `usize`.
const _: () = {
    assert!(Framebuffer::DEFAULT_WIDTH == 800 && Framebuffer::DEFAULT_HEIGHT == 480);
};

/// Line-by-line render target into the cached harness pixels.
struct FbLineBuffer<'a> {
    pixels: &'a mut [Bgrx8888Pixel],
    stride: usize,
}

impl<'a> FbLineBuffer<'a> {
    /// Renders directly into the cached RAM `pixels` buffer.
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
    /// Bytes per pixel (32bpp BGRX).
    pub const BYTES_PER_PIXEL: usize = 4;
    /// Color depth in bits per pixel (32bpp BGRX).
    pub const BITS_PER_PIXEL: u32 = Self::BYTES_PER_PIXEL as u32 * 8;
    /// Fixed panel buffer size in bytes (800x480x4).
    pub const SCANOUT_BYTE_LEN: usize =
        (Self::DEFAULT_WIDTH as usize) * (Self::DEFAULT_HEIGHT as usize) * Self::BYTES_PER_PIXEL;

    /// Creates an in-memory framebuffer harness paired with a [`FramebufferMockCtrl`]
    /// for deterministic fault injection.
    ///
    /// The buffer always models the fixed appliance panel geometry (800x480).
    #[must_use]
    pub fn new_mocked() -> (Self, FramebufferMockCtrl) {
        let ctrl = FramebufferMockCtrl::default();
        let fb = Self {
            buffer: vec![Bgrx8888Pixel::default(); Self::SCANOUT_BYTE_LEN / Self::BYTES_PER_PIXEL],
            ctrl: ctrl.clone(),
            force_full_redraw: core::cell::Cell::new(false),
        };
        (fb, ctrl)
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
        // next component binds to the primary test window.
        ACTIVE_WINDOW_CLAIMED.with(|cell| cell.set(false));

        // Set platform once per thread; AlreadySet is expected across multiple component cycles.
        match slint::platform::set_platform(Box::new(FramebufferPlatform)) {
            Ok(()) | Err(slint::platform::SetPlatformError::AlreadySet) => Ok(window),
            Err(err) => Err(FramebufferError::platform(err.to_string())),
        }
    }

    /// Renders a pending frame into the harness buffer if redraw was requested.
    ///
    /// Returns `true` if a frame was rendered, or `false` if no redraw was pending.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if a fault is injected via [`FramebufferMockCtrl`].
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn render_frame(
        &mut self,
        window: &MinimalSoftwareWindow,
    ) -> Result<bool, FramebufferError> {
        let width = Self::DEFAULT_WIDTH as usize;
        let force_full = self.force_full_redraw.replace(false);
        let Self { buffer, ctrl, .. } = self;

        let mut state = ctrl.state.lock().expect("mock state lock poisoned");
        if let Some(message) = &state.render_error {
            return Err(FramebufferError::io(
                "mock-fb",
                io::Error::other(message.to_string()),
            ));
        }
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
        if did_draw {
            state.render_calls += 1;
        }
        Ok(did_draw)
    }

    /// Renders one frame in which Slint repaints the WHOLE window regardless of its
    /// dirty state. Used when stale pixels must be cleared (e.g. a scene change that
    /// leaves no partial dirty region).
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`] if a fault is injected via [`FramebufferMockCtrl`].
    pub fn render_frame_full_redraw(
        &mut self,
        window: &MinimalSoftwareWindow,
    ) -> Result<bool, FramebufferError> {
        self.force_full_redraw.set(true);
        self.render_frame(window)
    }

    /// Copies a rectangular region of the current buffer content.
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
                format!("capture region ({x}, {y}, {w}, {h}) exceeds the {width}x{height} panel"),
            ));
        }
        // Hold the state mutex for the snapshot's lifetime; the buffer is read through
        // `&self`, the guard only serializes concurrent mock mutation.
        let _state = self.ctrl.state.lock().map_err(|poisoned| {
            FramebufferError::io(
                "mock-fb",
                io::Error::other(format!("mock state lock poisoned: {poisoned}")),
            )
        })?;
        let mut snapshot = Vec::with_capacity(w * h);
        for line in 0..h {
            let start = (y + line) * width + x;
            snapshot.extend_from_slice(&self.buffer[start..start + w]);
        }
        Ok(snapshot.into_boxed_slice())
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

    /// Total buffer size in bytes (4 bytes per pixel).
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        Self::SCANOUT_BYTE_LEN
    }

    /// Returns a slice of the rendered harness buffer.
    #[must_use]
    pub fn shadow_buffer(&self) -> &[Bgrx8888Pixel] {
        &self.buffer
    }

    /// Returns a slice of the rendered pixels.
    #[must_use]
    pub fn pixels(&self) -> &[Bgrx8888Pixel] {
        &self.buffer
    }

    /// Returns the number of frames rendered so far.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    #[must_use]
    pub fn render_count(&self) -> usize {
        self.ctrl
            .state
            .lock()
            .expect("mock state lock poisoned")
            .render_calls
    }
}

/// Internal framebuffer test helpers exported strictly under the `test-util` feature
/// for external integration testing per Microsoft Pragmatic Rust Guidelines (`M-TEST-UTIL`).
#[cfg(feature = "test-util")]
pub mod test_helpers {
    pub use super::{FramebufferPlatform, signal_test_loop_quit};
}
