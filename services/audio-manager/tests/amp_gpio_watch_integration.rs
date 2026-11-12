//! Integration tests for the `AmpGpio` error-line edge watch (mock backend).
//!
//! The mock path of `watch_error_edges` is fully host-testable: it polls the
//! in-memory `error_active` flag and fires the edge callback without any
//! physical GPIO hardware.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use audio_manager::amp_gpio::AmpGpio;

/// Receives one edge notification within a generous multiple of the mock
/// edge poll interval.
fn next_edge(rx: &mpsc::Receiver<()>) -> bool {
    rx.recv_timeout(Duration::from_secs(3)).is_ok()
}

#[test]
fn mock_edge_watch_fires_on_active_error_line() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    let (tx, rx) = mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let done_watcher = Arc::clone(&done);

    let handle = std::thread::spawn(move || {
        gpio.watch_error_edges(
            move || tx.send(()).is_ok(),
            move || !done_watcher.load(Ordering::SeqCst),
        );
    });

    // Activate the error line: the watcher must report an edge and keep
    // watching while the callback returns `true`.
    gpio_mock.set_error_active(true);
    assert!(next_edge(&rx), "edge notification missing for active error");
    assert!(next_edge(&rx), "watcher must keep firing on repeat polls");

    // Deactivate, then request shutdown: the thread must exit cleanly.
    gpio_mock.set_error_active(false);
    done.store(true, Ordering::SeqCst);
    handle.join().expect("edge watch thread panicked");
}

#[test]
fn mock_edge_watch_exits_promptly_without_edges() {
    let (gpio, _gpio_mock) = AmpGpio::new_mocked();
    let done = Arc::new(AtomicBool::new(false));
    let done_watcher = Arc::clone(&done);

    let handle = std::thread::spawn(move || {
        gpio.watch_error_edges(|| true, move || !done_watcher.load(Ordering::SeqCst));
    });

    done.store(true, Ordering::SeqCst);
    handle.join().expect("edge watch thread panicked");
}

#[test]
fn mock_edge_watch_stops_when_callback_declines() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();

    // Returning `false` from the edge callback (channel closed, controller
    // loop ended) must terminate the watch even while the error stays
    // active and shutdown was not requested.
    let handle = std::thread::spawn(move || gpio.watch_error_edges(|| false, || true));
    gpio_mock.set_error_active(true);
    handle.join().expect("edge watch thread panicked");
}
