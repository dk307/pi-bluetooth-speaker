//! Blurred backdrop snapshot for the full-screen overlays.
//!
//! Slint 1.18 has no backdrop-blur primitive, so the "frosted glass" effect is
//! produced here: the frame that an overlay is about to cover is captured from
//! the framebuffer shadow buffer, converted to premultiplied RGBA, blurred with
//! [`libblur`] and published back to Slint as an [`slint::Image`].
//!
//! The capture happens exactly once per "overlay open" period, because the
//! shadow buffer still holds the clean, overlay-free frame at the moment an
//! overlay is opened.

use std::backtrace::Backtrace;
use std::fmt::{self, Debug, Formatter};

use libblur::{AnisotropicRadius, BlurError, BlurImageMut, FastBlurChannels, ThreadingPolicy};

use super::framebuffer::Bgrx8888Pixel;

/// Height of the top status bar, in pixels.
///
/// Single source of truth for the overlay inset: [`AppShell::new`] seeds the
/// Slint `overlay_top_inset` property from this constant at startup, so the
/// `60px` literal in `appwindow.slint` is only a design-preview fallback.
/// Overlays only ever cover the viewport area below the top bar, so the
/// backdrop snapshot starts at this row.
pub const OVERLAY_TOP_INSET: u32 = 60;

/// Blur radius, in pixels, applied to the captured backdrop.
pub const BACKDROP_BLUR_RADIUS: u32 = 10;

/// A blurred, premultiplied RGBA snapshot of the screen region covered by an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlurredBackdrop {
    /// Snapshot width in pixels.
    pub width: u32,
    /// Snapshot height in pixels.
    pub height: u32,
    /// Premultiplied RGBA bytes; `width * height * 4` entries long.
    pub pixels: Vec<u8>,
}

/// Converts a BGRX pixel slice into premultiplied RGBA bytes.
///
/// The framebuffer alpha byte is unused by the `BGRX` scanout format, so the
/// alpha channel of the snapshot is set to fully opaque.
#[must_use]
pub fn bgrx_to_rgba(pixels: &[Bgrx8888Pixel]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for pixel in pixels {
        rgba.extend_from_slice(&[pixel.red, pixel.green, pixel.blue, 255]);
    }
    rgba
}

/// Extracts the region below `top_inset` from a full-frame BGRX buffer and returns
/// its blurred, premultiplied RGBA representation.
///
/// `frame` must hold exactly `frame_width * frame_height` pixels.
///
/// # Errors
///
/// Returns [`BackdropError::geometry`] when the buffer length does not match the
/// announced geometry or when `top_inset` covers the whole frame, and
/// [`BackdropError::blur`] when the blur backend rejects the image.
pub fn blur_region(
    frame: &[Bgrx8888Pixel],
    frame_width: u32,
    frame_height: u32,
    top_inset: u32,
    radius: u32,
) -> Result<BlurredBackdrop, BackdropError> {
    let expected = (frame_width as usize) * (frame_height as usize);
    if frame.len() != expected {
        return Err(BackdropError::geometry(
            format!("{expected} pixels"),
            format!("captured frame has {} pixels", frame.len()),
        ));
    }
    if top_inset >= frame_height {
        return Err(BackdropError::geometry(
            format!("top inset < {frame_height}"),
            format!("top inset {top_inset} leaves no rows to capture"),
        ));
    }

    let width = frame_width;
    let height = frame_height - top_inset;
    let row_offset = usize::try_from(top_inset).unwrap_or(usize::MAX)
        * usize::try_from(frame_width).unwrap_or(usize::MAX);
    let mut rgba = bgrx_to_rgba(&frame[row_offset..]);
    blur_premultiplied_rgba(&mut rgba, width, height, radius)?;
    Ok(BlurredBackdrop {
        width,
        height,
        pixels: rgba,
    })
}

/// Applies a stack blur to a premultiplied RGBA buffer in place.
///
/// The buffer must hold at least `width * height * 4` bytes; a shorter buffer
/// is rejected by the blur backend.
///
/// # Errors
///
/// Returns [`BackdropError::blur`] when the blur backend rejects the buffer,
/// for example because it is shorter than the announced geometry.
pub fn blur_premultiplied_rgba(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    radius: u32,
) -> Result<(), BackdropError> {
    let mut image = BlurImageMut::borrow(rgba, width, height, FastBlurChannels::Channels4);
    libblur::stack_blur(
        &mut image,
        AnisotropicRadius::new(radius),
        ThreadingPolicy::Single,
    )
    .map_err(BackdropError::blur)
}

/// Action the renderer must take after an overlay state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackdropAction {
    /// Keep the currently published backdrop.
    Keep,
    /// Capture and publish a new backdrop.
    Capture,
    /// Drop the published backdrop.
    Clear,
}

/// Tracks the overlay backdrop lifecycle across overlay open/close periods.
///
/// The backdrop is captured once when an overlay opens and kept until every
/// overlay has closed again, so that switching between overlays (for example
/// from the Volume HUD to the codec panel) reuses the same snapshot.
#[derive(Debug, Default)]
pub struct OverlayBackdropTracker {
    active: bool,
}

impl OverlayBackdropTracker {
    /// Creates a tracker in the "no overlay on screen" state.
    #[must_use]
    pub const fn new() -> Self {
        Self { active: false }
    }

    /// Reports the current overlay state and returns the action the renderer must take.
    pub fn on_overlay_state_changed(&mut self, overlay_active: bool) -> BackdropAction {
        let action = match (self.active, overlay_active) {
            (false, true) => BackdropAction::Capture,
            (true, false) => BackdropAction::Clear,
            _ => BackdropAction::Keep,
        };
        self.active = overlay_active;
        action
    }

    /// Returns `true` while an overlay is on screen.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }
}

/// Detailed reason for a [`BackdropError`].
#[derive(Debug)]
pub(crate) enum BackdropErrorKind {
    /// The captured buffer did not match the expected geometry.
    Geometry {
        expected: Box<str>,
        message: Box<str>,
    },
    /// The blur backend rejected the operation.
    Blur { source: BlurError },
}

/// Error returned by the overlay backdrop pipeline.
#[derive(Debug)]
pub struct BackdropError {
    kind: BackdropErrorKind,
    backtrace: Backtrace,
}

impl BackdropError {
    /// Creates a geometry mismatch error.
    pub(crate) fn geometry(expected: impl Into<Box<str>>, message: impl Into<Box<str>>) -> Self {
        Self::new(BackdropErrorKind::Geometry {
            expected: expected.into(),
            message: message.into(),
        })
    }

    /// Creates a blur backend error.
    pub(crate) fn blur(source: BlurError) -> Self {
        Self::new(BackdropErrorKind::Blur { source })
    }

    fn new(kind: BackdropErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns `true` when the captured buffer did not match the expected geometry.
    #[must_use]
    pub fn is_geometry(&self) -> bool {
        matches!(self.kind, BackdropErrorKind::Geometry { .. })
    }

    /// Returns `true` when the blur backend rejected the operation.
    #[must_use]
    pub fn is_blur(&self) -> bool {
        matches!(self.kind, BackdropErrorKind::Blur { .. })
    }

    /// Returns the captured backtrace.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BackdropErrorKind::Geometry { expected, message } => {
                write!(
                    f,
                    "overlay backdrop geometry mismatch (expected {expected}): {message}"
                )
            }
            BackdropErrorKind::Blur { source } => {
                write!(f, "overlay backdrop blur failed: {source}")
            }
        }
    }
}

speaker_common::impl_canonical_error_display!(BackdropError);

impl std::error::Error for BackdropError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            BackdropErrorKind::Blur { source } => Some(source),
            BackdropErrorKind::Geometry { .. } => None,
        }
    }
}
