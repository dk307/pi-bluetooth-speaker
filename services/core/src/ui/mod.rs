//! User interface components, views, and Slint declarative bindings.
//!
//! Exposes ahead-of-time (AOT) compiled Slint UI components including
//! [`MainWindow`] and [`SplashScreen`], alongside the physical Linux
//! framebuffer (`/dev/fb0`) display driver.

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::restriction,
    clippy::semicolon_outside_block,
    trivial_numeric_casts,
    missing_debug_implementations,
    unused_qualifications
)]
mod generated {
    slint::include_modules!();
}

pub mod framebuffer;

#[doc(inline)]
pub use framebuffer::{Bgrx8888Pixel, Framebuffer, FramebufferError, FramebufferErrorKind};
#[doc(inline)]
pub use generated::{MainWindow, SplashScreen};
#[doc(inline)]
pub use slint::ComponentHandle;
