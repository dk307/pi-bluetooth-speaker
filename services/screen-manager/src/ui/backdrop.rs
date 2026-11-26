//! Blurred backdrop snapshot for the full-screen overlays.
//!
//! When an overlay is about to open, [`AppShell`] captures a
//! [`slint::Window::take_snapshot`] of the clean scene, blurs it with
//! [`libblur`] (stronger behind the overlay than in the top bar outside it),
//! and publishes it back to Slint as an [`slint::Image`].

use std::backtrace::Backtrace;
use std::fmt::{self, Debug, Formatter};

use libblur::{AnisotropicRadius, BlurError, BlurImageMut, FastBlurChannels, ThreadingPolicy};

/// Height of the top status bar, in pixels.
///
/// Single source of truth for the overlay inset: [`AppShell::new`] seeds the
/// Slint `overlay_top_inset` property from this constant at startup, so the
/// `60px` literal in `appwindow.slint` is only a design-preview fallback.
pub const OVERLAY_TOP_INSET: u32 = 60;

/// Blur radius, in pixels, applied to the backdrop behind the overlay panel.
pub const BACKDROP_BLUR_RADIUS: u32 = 20;

/// Blur radius, in pixels, applied to the area outside the overlay panel (the top bar).
pub const OUTSIDE_BLUR_RADIUS: u32 = 10;

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

/// Blurs a full-frame RGBA8 snapshot, applying `outside_radius` to the area
/// outside the overlay (`0..top_inset`) and `inside_radius` to the area behind
/// the overlay (`top_inset..frame_height`).
///
/// `rgba` must hold exactly `frame_width * frame_height * 4` bytes.
///
/// # Errors
///
/// Returns [`BackdropError::geometry`] when the buffer length does not match the
/// announced geometry or when `top_inset > frame_height`, and
/// [`BackdropError::blur`] when the blur backend rejects either region.
pub fn blur_full_rgba_backdrop(
    rgba: &[u8],
    frame_width: u32,
    frame_height: u32,
    top_inset: u32,
    inside_radius: u32,
    outside_radius: u32,
) -> Result<BlurredBackdrop, BackdropError> {
    let expected = (frame_width as usize) * (frame_height as usize) * 4;
    if rgba.len() != expected {
        return Err(BackdropError::geometry(
            format!("{expected} bytes"),
            format!("captured frame has {} bytes", rgba.len()),
        ));
    }
    if top_inset > frame_height {
        return Err(BackdropError::geometry(
            format!("top inset <= {frame_height}"),
            format!("top inset {top_inset} exceeds frame height {frame_height}"),
        ));
    }

    let mut full = rgba.to_vec();
    for alpha in full.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }

    let split_byte = top_inset as usize * frame_width as usize * 4;
    if top_inset > 0 {
        blur_premultiplied_rgba(
            &mut full[..split_byte],
            frame_width,
            top_inset,
            outside_radius,
        )?;
    }
    if top_inset < frame_height {
        blur_premultiplied_rgba(
            &mut full[split_byte..],
            frame_width,
            frame_height - top_inset,
            inside_radius,
        )?;
    }

    Ok(BlurredBackdrop {
        width: frame_width,
        height: frame_height,
        pixels: full,
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
    )?;
    Ok(())
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

impl From<BlurError> for BackdropError {
    fn from(source: BlurError) -> Self {
        Self::blur(source)
    }
}
