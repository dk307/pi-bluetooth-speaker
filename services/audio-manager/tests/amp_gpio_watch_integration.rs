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

#[test]
fn mock_gpio_wake_calls() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    assert_eq!(gpio_mock.wake_calls(), 0);
    gpio.wake().unwrap();
    assert_eq!(gpio_mock.wake_calls(), 1);
    gpio.wake().unwrap();
    assert_eq!(gpio_mock.wake_calls(), 2);
}

#[test]
fn mock_gpio_reset_calls() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    assert_eq!(gpio_mock.reset_calls(), 0);
    gpio.reset().unwrap();
    assert_eq!(gpio_mock.reset_calls(), 1);
    gpio.reset().unwrap();
    assert_eq!(gpio_mock.reset_calls(), 2);
}

#[test]
fn mock_gpio_power_up_calls() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    assert_eq!(gpio_mock.power_up_calls(), 0);
    gpio.power_up().unwrap();
    assert_eq!(gpio_mock.power_up_calls(), 1);
    gpio.power_up().unwrap();
    assert_eq!(gpio_mock.power_up_calls(), 2);
}

#[test]
fn mock_gpio_mute_pin_state() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    assert!(!gpio_mock.mute_pin_low()); // Default is HIGH (unmuted)
    gpio.set_hardware_mute(true).unwrap();
    assert!(gpio_mock.mute_pin_low());
    gpio.set_hardware_mute(false).unwrap();
    assert!(!gpio_mock.mute_pin_low());
}

#[test]
fn mock_gpio_booster_state() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    assert!(!gpio_mock.booster_on());
    gpio.power_up().unwrap();
    assert!(gpio_mock.booster_on());
    gpio.reset().unwrap();
    assert!(!gpio_mock.booster_on());
}

#[test]
fn mock_gpio_amp_awake_state() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    let awake1 = gpio_mock.amp_awake();
    eprintln!("Initial awake 1: {}", awake1);
    let awake2 = gpio_mock.amp_awake();
    eprintln!("Initial awake 2: {}", awake2);
    let awake3 = gpio_mock.amp_awake();
    eprintln!("Initial awake 3: {}", awake3);
    let val = gpio_mock.amp_awake();
    eprintln!("Direct call: {}", val);
    if val {
        panic!("val is true!");
    }
    gpio.wake().unwrap();
    assert!(gpio_mock.amp_awake());
    gpio.reset().unwrap();
    // reset() performs a full reset cycle (HIGH pulse then LOW), which
    // wakes the amp per hardware behavior.
    assert!(gpio_mock.amp_awake());
    // To get back to asleep, we need another reset or explicit sleep.
    // For test purposes, we verify the cycle completed.
    gpio.reset().unwrap();
    assert!(gpio_mock.amp_awake());
}

#[test]
fn mock_gpio_mute_calls() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    assert_eq!(gpio_mock.mute_calls(), 0);
    gpio.set_hardware_mute(true).unwrap();
    assert_eq!(gpio_mock.mute_calls(), 1);
    gpio.set_hardware_mute(false).unwrap();
    assert_eq!(gpio_mock.mute_calls(), 2);
}

#[test]
fn mock_gpio_reset_hook() {
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    let hook_called = Arc::new(AtomicBool::new(false));
    let hook_called_clone = Arc::clone(&hook_called);

    gpio_mock.set_reset_hook(Some(Arc::new(move || {
        hook_called_clone.store(true, Ordering::SeqCst);
    })));

    gpio.reset().unwrap();
    assert!(hook_called.load(Ordering::SeqCst));

    hook_called.store(false, Ordering::SeqCst);
    gpio_mock.set_reset_hook(None);
    gpio.reset().unwrap();
    assert!(!hook_called.load(Ordering::SeqCst));
}
