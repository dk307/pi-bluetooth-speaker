//! Semantic two-line song-title breaking for constrained audio UI layouts.
//!
//! Long track titles rendered in a narrow two-line container (now-playing
//! cards, OLED/matrix displays, terminal panes) cannot use plain greedy
//! wrapping: it strands single-word orphans on line 2 and fractures
//! parenthetical provenance such as `(From "Brahmastra")` at the geometric
//! midpoint. This crate instead generates every plausible breakpoint,
//! scores each candidate with a tiered semantic base score (structural
//! delimiters > enclosure boundaries > collaboration connectors > clause
//! markers > plain whitespace) minus non-linear balance, orphan, dangling
//! particle, and optional overflow penalties, and returns the best split.
//!
//! The implementation is pure `std`: no regular expressions, no NLP
//! dependencies, full UTF-8 char-boundary safety, and `O(n)` time per title.
//!
//! # Example
//!
//! ```
//! use speaker_title_breaker::{split_song_title, BreakReason};
//!
//! let split = split_song_title("Kesariya (From \"Brahmastra\")");
//! assert_eq!(split.line1, "Kesariya");
//! assert_eq!(split.line2, "(From \"Brahmastra\")");
//! assert_eq!(split.reason, BreakReason::BeforeBracket);
//! ```

pub mod title_breaker;

#[doc(inline)]
pub use title_breaker::{
    BreakReason, BreakerConfig, TitleSplit, split_song_title, split_song_title_with_config,
};
