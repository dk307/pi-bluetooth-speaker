//! Application UI Shell controller and view manager.
//!
//! Orchestrates the 800x480 appliance user interface:
//! - Top status bar ([`TopStatusBar`])
//! - Central active viewport ([`MainViewport`])
//! - Bottom navigation dock ([`NavDock`])
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`).

use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use slint::ComponentHandle;

use super::generated::MainWindow;
use super::theme::ThemePreset;

/// Shared state and interactive controller for [`MainWindow`].
pub struct AppShell {
    window: MainWindow,
    active_theme: Arc<AtomicI32>,
    active_tab: Arc<AtomicI32>,
}

impl Debug for AppShell {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppShell")
            .field("active_theme", &self.current_theme())
            .field("active_tab", &self.current_tab())
            .finish_non_exhaustive()
    }
}

impl AppShell {
    /// Initializes an [`AppShell`] controller wrapping a [`MainWindow`] component.
    #[must_use]
    pub fn new(window: MainWindow) -> Self {
        let active_theme = Arc::new(AtomicI32::new(ThemePreset::default().index()));
        let active_tab = Arc::new(AtomicI32::new(0));

        let shell = Self {
            window,
            active_theme,
            active_tab,
        };

        shell.setup_callbacks();
        shell.apply_theme(ThemePreset::default());
        shell
    }

    /// Returns a reference to the underlying Slint [`MainWindow`] handle.
    #[must_use]
    pub const fn window(&self) -> &MainWindow {
        &self.window
    }

    /// Returns the currently active theme preset.
    #[must_use]
    pub fn current_theme(&self) -> ThemePreset {
        ThemePreset::from_index(self.active_theme.load(Ordering::Acquire))
    }

    /// Returns the currently active navigation dock tab index (0 to 5).
    #[must_use]
    pub fn current_tab(&self) -> i32 {
        self.active_tab.load(Ordering::Acquire)
    }

    /// Returns whether the optical CD player service and navigation entry are enabled.
    #[must_use]
    pub fn is_cd_enabled(&self) -> bool {
        self.window.get_cd_enabled()
    }

    /// Sets whether the optical CD player service and navigation entry are enabled.
    ///
    /// If disabled while the shell is currently on the CD Player tab (tab 3), the active tab
    /// is automatically reset to the Now Playing player tab (tab 0) to prevent state desync.
    pub fn set_cd_enabled(&self, enabled: bool) {
        self.window.set_cd_enabled(enabled);
        if !enabled && self.current_tab() == 3 {
            self.set_tab(0);
        }
    }

    /// Switches the active navigation dock tab.
    ///
    /// If `tab` is 3 (CD Player) but CD Player is not enabled, falls back to tab 0.
    pub fn set_tab(&self, tab: i32) {
        let mut tab = tab.clamp(0, 5);
        if tab == 3 && !self.is_cd_enabled() {
            tab = 0;
        }
        self.active_tab.store(tab, Ordering::Release);
        self.window.set_active_tab(tab);
    }

    /// Updates the active audio source name shown in the top status bar.
    pub fn set_source(&self, source: impl Into<slint::SharedString>) {
        self.window.set_active_source(source.into());
    }

    /// Updates the track or connected device name displayed in the top status bar.
    pub fn set_track_or_device(&self, info: impl Into<slint::SharedString>) {
        self.window.set_track_or_device(info.into());
    }

    /// Updates the active audio codec badge in the top status bar.
    pub fn set_stream_codec(&self, codec: impl Into<slint::SharedString>) {
        self.window.set_stream_codec(codec.into());
    }

    /// Updates the Now Playing title, artist, and album.
    pub fn set_track_metadata(
        &self,
        title: impl Into<slint::SharedString>,
        artist: impl Into<slint::SharedString>,
        album: impl Into<slint::SharedString>,
    ) {
        self.window.set_track_title(title.into());
        self.window.set_track_artist(artist.into());
        self.window.set_track_album(album.into());
    }

    /// Updates the volume percentage (clamped to 0.0–100.0).
    pub fn set_volume(&self, volume: f32) {
        let clamped = if volume.is_finite() {
            volume.clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.window.set_volume_value(clamped);
    }

    /// Applies a theme preset to the UI shell.
    pub fn apply_theme(&self, preset: ThemePreset) {
        self.active_theme.store(preset.index(), Ordering::Release);
        preset.apply(&self.window);
    }

    /// Cycles to the next available theme preset in cyclic order.
    #[must_use]
    pub fn cycle_theme(&self) -> ThemePreset {
        let next = self.current_theme().next();
        self.apply_theme(next);
        next
    }

    fn setup_callbacks(&self) {
        let win = self.window.as_weak();
        let tab_atomic = Arc::clone(&self.active_tab);
        self.window.on_tab_selected(move |idx| {
            tab_atomic.store(idx, Ordering::Release);
            if let Some(w) = win.upgrade() {
                w.set_active_tab(idx);
            }
        });

        let win = self.window.as_weak();
        let theme_atomic = Arc::clone(&self.active_theme);
        self.window.on_select_theme(move |idx| {
            let preset = ThemePreset::from_index(idx);
            theme_atomic.store(preset.index(), Ordering::Release);
            if let Some(w) = win.upgrade() {
                preset.apply(&w);
            }
        });

        let win = self.window.as_weak();
        let theme_atomic = Arc::clone(&self.active_theme);
        self.window.on_cycle_theme(move || {
            let current = ThemePreset::from_index(theme_atomic.load(Ordering::Acquire));
            let next = current.next();
            theme_atomic.store(next.index(), Ordering::Release);
            if let Some(w) = win.upgrade() {
                next.apply(&w);
            }
        });

        self.window.on_source_clicked(|| {
            tracing::info!("Source pill clicked");
        });

        self.window.on_night_mode_clicked(|| {
            tracing::info!("Night mode clicked");
        });

        self.window.on_volume_changed(|val| {
            tracing::debug!("Volume changed to {val}%");
        });

        self.window.on_toggle_play_pause(|| {
            tracing::debug!("Play/Pause toggled");
        });
    }
}
