# Engineering Plan: Blocking I/O Remediation, Architecture Refactor & UI Polish (PR #20 Leftovers)

This document outlines the step-by-step execution plan for resolving the outstanding review findings from PR #20, as well as the additional UI/UX and architectural refinements requested. The plan is strictly aligned with the rules defined in `AGENTS.md` (Zero-Retry Pre-Push Gate, Blast Radius Containment, and M-Rules).

---

## 1. Objectives

1.  **Eliminate Blocking I/O:** Ensure the UI render thread (`main` / `run_render_loop`) never performs blocking D-Bus calls, heavy image decoding, or `thread::sleep` operations.
2.  **Architectural Integrity:** Consolidate fragmented state, decompose the monolithic `bluetooth.rs`, and adhere to M-Rules (M-SINGLE-ITEM-PATH, M-LINT-OVERRIDE-EXPECT, M-MOCKABLE-SYSCALLS).
3.  **UI/UX Polish:** Implement the specific UI/behavioral adjustments (placeholders, alignment, sizing, event-driven state updates).
4.  **Strict Quality Gates:** Guarantee 100% compliance with `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo llvm-cov --fail-under-lines 90 --fail-under-functions 90`, and aarch64 release build success.

---

## 2. Phase 1: Blocking I/O & Performance Hot Paths (Priorities 1 & 2)

### 2.1. Refactor D-Bus & Cover Art off the Render Thread (`shell.rs`, `core_daemon.rs`)

**Problem:** `on_toggle_play_pause` and `on_seek` currently trigger blocking `GetManagedObjects` round-trips directly from Slint event handlers, which run on the render thread. `apply_playback_to_window` also invokes `image::open` + Lanczos3 resizing on the same thread, breaching the 16.67ms frame budget.

**Solution:** Move all heavy I/O to the existing `bt-monitor` worker thread. The render thread will only handle optimistic updates and state reads from the shared atomic/mutex state.

#### Implementation Steps:
1.  **Introduce Optimistic UI Updates:**
    *   In `shell.rs`, modify `on_toggle_play_pause` and `on_seek` to apply immediate, non-blocking UI state changes (e.g., toggle `is_playing`, move the progress bar to the target `ratio`).
    *   Remove any synchronous calls to `bt.playback_state()` or `bt.player_toggle()` from these callbacks.
2.  **Command Queue Injection (`core_daemon.rs`):**
    *   Introduce a thread-safe command queue (e.g., `Arc<Mutex<VecDeque<TransportCommand>>>`).
    *   `wire_bluetooth_transport` callbacks (`on_toggle_play_pause`, `on_seek`, `on_next_track`, `on_previous_track`) will push commands (e.g., `Toggle`, `Seek(ratio)`) onto this queue instead of calling `bt` methods directly.
3.  **Worker-Side Execution (`bt_monitor_loop`):**
    *   Modify `bt_monitor_loop` to drain the command queue at the start of each polling cycle (25ms tick).
    *   Execute D-Bus calls (`player_toggle`, `player_seek`) on this worker thread.
    *   Handle cover art resolution entirely on the worker: call `image::open`, resize (Lanczos3), and encode to a temporary file (or in-memory buffer) before pushing the final `slint::Image` handle to the shared state.
4.  **Shared State Update:**
    *   Update the `shared_state` tuple (`Option<BtPlaybackState>`, `Option<BlueAlsaPcm>`) to include a cover art field (e.g., `Option<CoverArtPayload>` containing the `slint::Image` or path).
    *   `apply_shared_bt_state` (running on the render thread) will read from this shared state and update the UI window properties without doing any I/O.

### 2.2. Verify No Blocking on UI Thread
*   Audit all Slint callbacks in `shell.rs` (both wired and unwired defaults) to ensure they do not invoke blocking syscalls.
*   Ensure `slint::Image::load_from_path` is only called if the image was pre-processed on the worker thread (or handled via a lightweight background `slint::Image` handle).

---

## 3. Phase 2: DbusClient Internal Timers & State (Priority 3 & 4.7)

### 3.1. Eliminate `thread::sleep` in `bluetooth.rs` (~Lines 1306, 1684, 1734, 1895)

**Problem:** Blocking `thread::sleep` calls (`BIP_TRACK_SETTLE_MS` 650ms, ACL reconnect 1500ms) inside `DbusClient` block the bt-monitor thread, preventing it from draining the command queue or receiving D-Bus signals.

**Solution:** Replace synchronous sleeps with a state-machine timer approach.

#### Implementation Steps:
1.  **State Transition Tracking:**
    *   Define state fields (e.g., `pending_reconnect_deadline: Option<Instant>` or `bip_settle_deadline: Option<Instant>`) inside the consolidated state struct (see 3.2).
    *   When an event occurs (e.g., BIP track change detected), instead of sleeping, set a `deadline` field.
    *   Modify `bt_monitor_loop` to check for these deadlines. If the deadline is in the future, skip the action and yield; if expired, execute the D-Bus call.
2.  **Signal Processing Priority:** Ensure the worker's polling loop is never blocked for >25ms, allowing `zbus`/D-Bus signal processing (if implemented via block_on/polling) to remain responsive.

### 3.2. Consolidate Fragmented Mutex State (Priority 4.7)

**Problem:** `obex_session`, `acl_tuned`, and `socket_tuned` are separate `Arc<Mutex<...>>` fields in `DbusClient`, creating potential lock-order risks.

**Solution:** Consolidate into a single state struct.

#### Implementation Steps:
1.  Define a private struct `DbusClientState`:
    ```rust
    struct DbusClientState {
        obex_session: Option<(BtAddress, OwnedObjectPath)>,
        acl_tuned: bool,
        socket_tuned: bool,
        pending_action: Option<PendingAction>, // for the sleep-refactor timers
    }
    ```
2.  Replace the three separate `Arc<Mutex<...>>` fields with a single `Arc<Mutex<DbusClientState>>`.
3.  Refactor all method calls (`ensure_obex_session`, socket tuning logic) to acquire the single lock once per logical operation, rather than sequential locking.
4.  **Hermetic Testing:** Update existing mocks (`new_mocked()`) to reflect this unified state structure, ensuring mock error-injection paths remain valid (M-MOCKABLE-SYSCALLS).

---

## 4. Phase 3: Architectural Cleanup (Priorities 4.8 & 4.9)

### 4.1. Refactor `run_render_loop` (`core_daemon.rs` / `framebuffer.rs`)

**Problem:** The `run_render_loop` function is too long, violating the `too_many_lines` lint threshold.

**Solution:** Extract helper functions.

#### Implementation Steps:
1.  **Extract Frame Timing Logic:** Move the `TARGET_FRAME_TIME` calculation, `checked_sub`, and idle sleep logic into a `fn process_frame_timing(loop_timing: &mut FrameLoopTiming, did_render: bool, has_animations: bool)`.
2.  **Extract Worker Management:** Move the `bt_worker` spawning and shutdown/join logic into a helper struct `BtMonitorHandle` with `Drop` semantics for clean teardown.
3.  **Extract VSync & Render step:** Keep the core render loop minimal; ensure `clippy::too_many_lines` passes.

### 4.2. Split Monolithic `bluetooth.rs` (Priority 4.9)

**Problem:** `bluetooth.rs` is ~3600 lines, violating modularity standards.

**Solution:** Split into submodules.

#### Implementation Steps:
1.  Create the directory `services/core/src/hardware/bluetooth/`.
2.  Extract the cover-art engine (lines ~1481–1824) into `hardware/bluetooth/cover_art.rs`.
3.  Extract connection state and lifecycle management into `hardware/bluetooth/connection.rs`.
4.  Extract transport control (AVRCP commands, playback state) into `hardware/bluetooth/transport.rs`.
5.  Retain `hardware/bluetooth.rs` (or `hardware/bluetooth/mod.rs`) as the public facade, re-exporting the `DbusClient` and `BluetoothDevice` types to preserve backward compatibility for callers (e.g., `shell.rs` and `core_daemon.rs`).

---

## 5. Phase 4: Event-Driven Monitoring & UI Polish (Additional Tasks)

### 5.1. Event-Driven State via `PropertiesChanged`
*   **Requirement:** Replace active polling (250ms/750ms) with event-driven D-Bus signal monitoring (`org.freedesktop.DBus.Properties.PropertiesChanged`).
*   **Implementation:**
    *   Implement a `zbus` `SignalStream` listener on a dedicated D-Bus connection thread (or within the `bt-monitor` worker using async/await).
    *   Map incoming signals to shared state updates (playback status, track changes, metadata, seek position).
    *   Use the command queue (Phase 1) to trigger follow-up D-Bus calls (e.g., `GetAll` on player interface upon `PropertiesChanged` for the `TrackId`).
*   **Benefit:** Reduces D-Bus traffic, latency, and CPU usage on the BCM2837.

### 5.2. UI Placeholders & State Persistence Bugs
*   **Requirement:** Handle disconnected/no-track states gracefully.
*   **Implementation:**
    *   **"No Connection":** When `bt` device is disconnected, set track title to `"No Connection"`, and clear the singer/artist line (`""`).
    *   **"No Title":** When connected but no track is playing/metadata missing, set title to `"No Title"`, and clear the singer line (`""`).
    *   **Stale Data Fix:** Ensure `apply_shared_bt_state` clears UI fields (title, artist, cover art, progress bar, play state) when the connection is severed (i.e., when `Option<BtPlaybackState>` is `None`).
*   **Testing:** Add unit tests to `shell.rs` (or integration tests) verifying that UI state resets when the connection drops.

### 5.3. UI Layout & Visual Adjustments (Slint)

*   **Device Name (Top Status Bar):**
    *   Vertically center the Bluetooth device name text.
    *   Increase the font size relative to the current setting (use relative scaling or `M-SLINT-MATH-LAYOUT` compliant dynamic math, e.g., `font_size: parent.height * 0.5;`).
*   **Bluetooth Badge:**
    *   Remove the border/badge background around the Bluetooth icon.
    *   Vertically align the icon within the full height of its container (`VerticalLayout { alignment: center; }`).
    *   Ensure the icon scales to fill the vertical area.
*   **Song Progress Circle:**
    *   Increase the radius/size of the position indicator circle by ~20%.
    *   Use dynamic layout math: `width: parent.height * 1.2` (or similar) to avoid hardcoded pixel offsets.

### 5.4. Asset Cleanup
*   **Requirement:** Remove unused icons/pictures.
*   **Implementation:**
    *   Audit `assets/icons/` and `assets/skins/` against `.slint` file references.
    *   Remove any image not statically referenced or required by the splash screen.
    *   Update `Cargo.toml`/build scripts if any assets were manually registered.

---

## 6. Phase 5: Verification & Release Gates

Before any push, the following commands **MUST** pass in order (Zero-Retry Pre-Push Gate):

1.  **Format:** `cargo fmt --manifest-path services/core/Cargo.toml -- --check`
2.  **Lint:** `cargo clippy --manifest-path services/core/Cargo.toml --all-targets -- -D warnings`
3.  **Coverage:** `cargo llvm-cov --manifest-path services/core/Cargo.toml --target x86_64-unknown-linux-gnu --fail-under-lines 90 --fail-under-functions 90`
4.  **Cross-Compile Build:** `cargo build --config services/core/.cargo/config.toml --manifest-path services/core/Cargo.toml --target aarch64-unknown-linux-gnu --release --bins`
5.  **Live Hardware Test:** Deploy binaries to `root@192.168.2.110:/tmp/` (non-destructive). Verify:
    *   UI does not freeze on `on_toggle_play_pause`/`on_seek` while `bluetoothd` is under load.
    *   Cover art updates without stuttering the marquee/animation.
    *   Reconnection state handling (simulate disconnect -> verify "No Connection").
    *   Verify layout updates visually (device name sizing, badge removal, progress circle).

---

## 7. Milestone Summary

| Phase | Milestone | Key Risk |
| :--- | :--- | :--- |
| **1** | Command queue + worker-side I/O operational | Lock contention on `shared_state` |
| **2** | Timer-based sleeps + Consolidated `DbusClientState` | Deadlocks on lock consolidation |
| **3** | `bluetooth.rs` split + `run_render_loop` cleanup | Build breaks from module re-exports |
| **4** | Event-driven signals + UI polish live on hardware | Signal regression (missed `PropertiesChanged`) |
| **5** | All gates green + live verification | Hardware-specific rendering edge cases |
