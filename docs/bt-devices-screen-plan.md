# Implementation Plan: Bluetooth Device Management Screens

**Worktree:** `/root/bt-devices-screen` · **Branch:** `feat/bt-devices-screen` (from `origin/main`, commit `c774b61`)

## 1. Goal & Scope

Implement the three-screen Bluetooth device-management flow specified by the
user (Main Device List → Add Device / Discovery → Connecting & Pairing Passkey)
plus the Delete-Confirmation dialog, on the fixed 800×480 appliance panel.

**Decision (user-confirmed): this new screen *replaces* the existing Bluetooth
details tab (tab 2) entirely.** The details tab, its ten `bt_*` text properties,
its worker publish path, and its test file are removed — not extended. The
Bluetooth surface becomes one full-screen "screen" (not a tab) reachable from
everywhere the details tab used to be reachable.

**In scope**
- New full-screen Slint surface `ui/bluetooth_devices.slint` (list + discovery +
  pairing + delete dialog + toast + thin scrollbar + overflow cue).
- Retirement of the tab-2 details screen (§4) with a precise removal list.
- New `speaker-bluetooth` admin API: BlueZ discovery, `Device1.Connect`,
  `Device1.Disconnect`, `Adapter1.RemoveDevice`, `Device1.Pair` +
  `org.bluez.Agent1` passkey capture, all with a mock backend.
- `screen-manager` plumbing: command queue → `bt-monitor` worker → shared
  snapshot → `AppShell` → window properties (the established transport pattern).
- Icons, integration tests, coverage ≥ 90 % for `screen-manager`.

**Out of scope (explicitly)**
- No changes to `bt-manager` (its auto-trust agent stays the system default).
- No changes to the material-1.1.0 vendored library.
- No rework of Now Playing, VU meter, CD, DSP, Settings, status bar visuals, or
  the volume/DSP pipelines.
- No AVRCP/media work, no BLE GATT, no multi-adapter selection.

## 2. Spec → Implementation Mapping

| Spec item | Implementation |
|---|---|
| 800×480, fixed 88 px app bar + 392 px scroll viewport | Full-window overlay layer in `MainWindow` (D1). `BtMetrics.header_height = 88px`, `viewport_height: root.height - BtMetrics.header_height` (= 392 px). |
| Background `surface`, cards `surface-container`, active card `secondary-container` | `MaterialPalette.surface` / `surface_container` / `secondary_container` (already pushed per-theme by `ThemePreset::apply`). |
| Typography `on-surface` / `on-surface-variant` / `on-secondary-container` | `MaterialPalette.on_surface`, `on_surface_variant`, `on_secondary_container`. |
| Primary action `primary`/`on-primary` (pill, 48 px) | Reuse `FilledButton` (pill radius = `height/2`) for `+ Add Device`, `Pair`, `Connect`. |
| Destructive `error`/`on-error` (delete confirm) | New `BtDestructivePill` built on the same `BaseButton`/`Elevation` pattern with `MaterialPalette.error`. |
| Dismiss/Cancel `outline`/`secondary` | Reuse `OutlinedButton` for `Cancel` / `Disconnect`. |
| Momentum scrolling, overscroll glow disabled | `Flickable` inside the new list component (Slint has no overscroll glow by default). |
| Scrollbar 4 px wide, 8 px right margin, auto-hide after 1500 ms | New `BtScrollBar` (§5.1); 1.5 s hide `Timer` mirrors the material `ScrollBar` behaviour. |
| Bottom overflow cue: 32 px gradient `surface` 100 %→0 % + ~35 % of 4th card visible | 32 px `@linear-gradient` overlay pinned to the viewport bottom, **gated on `content_height > viewport_height`** (the spec's "35 % exposed" state is arithmetically unreachable — see deviations); card list is a plain `VerticalLayout`, so longer lists clip naturally. Scrollbar renders above the gradient. |
| Modal backdrop disables scrolling underneath | `flickable.interactive: !root.bt_delete_open` + full-screen scrim above the list (D4). |
| Cards 740 × 88, radius 20, padding 0 20, gap 12 | `BtDeviceCard`; width derived: `800 − 2×24 (list padding) − 4 (scrollbar) − 8 (margin) = 740`. |
| Discovery cards: icon + name + sub-type + `Pair` (44 px) | Same `BtDeviceCard` with `variant: pairing`. |
| Connecting/pairing: non-scrollable centered, ring + icon, `Pairing Code: 482910` (`headline-large`), helper text | `BtPairingView`; progress ring driven by an integer frame `Timer` (D6), **not** the material `CircularProgressIndicator` (it reads `animation-tick()`, violating M-NO-PERPETUAL-ANIMATION-DRIVER). |
| 30 s pairing timeout → back to Add Device + toast `Pairing timed out` | Worker-owned deadline (`BT_PAIRING_TIMEOUT`), published as `PairingStatus::TimedOut` + `toast`; render thread applies both. The blocking `Pair` call stalls the worker for the pairing duration (accepted, D9). |
| Header subtitle `Visible as "Home Speaker"` | Live adapter alias from `LinkSnapshot.adapter` (defaults to `"Home Speaker"`), reusing `BtLinkDetails::adapter_name_text`. |

### Documented deviations from the spec text
- **740 px card width is derived, not literal**: the spec's 740 px only closes
  arithmetically when the 4 px scrollbar + 8 px margin sit inside the 800 px
  panel (800 − 24 − 24 − 4 − 8 = 740). We compute it from those constants.
- Material's `padding_24` token was compacted to 20 px for this panel
  (`material_style_metrics.slint`); the spec's 24 px is honoured as a local
  `BtMetrics.list_padding_h = 24px` constant instead of the token.
- BlueZ has no "Make Active" semantic — connecting a second device simply
  connects it. We surface the spec's wording: `Make Active` when another device
  is active, `Connect` when none is (label only; both dispatch `Connect`).
- **4th-card overflow-cue arithmetic conflict (audited A1)**: with the spec's
  own numbers the "4th card ~35 % exposed" state is unreachable — card pitch is
  88 + 12 = 100 px, so four cards span 388 px ≤ 392 px viewport (~96 % visible).
  Even with a 12 px list top padding, row 4 ends at 400 px (~92 % visible). No
  constant assignment reproduces "35 % exposed" while keeping 88/12/392. We
  honour the 88 px card / 12 px gap / 392 px viewport constants and accept full
  visibility of a 4-row list; the overflow cue below is therefore **gated**.
- **Overflow cue is conditional (audited A1)**: the 32 px bottom gradient is
  only visible when the list actually overflows (`content_height >
  viewport_height`), and only above the scroll position where content remains —
  otherwise it falsely signals scrollability on the default 4-device state.
  The scrollbar renders **above** the gradient (higher z-order) so the thumb is
  never faded by the cue near the bottom (audited A10).

## 3. Architecture Decisions

**D1 — Full-window overlay layer, not a tab.**
The spec's chrome is 88 px (bar) + 392 px (viewport) = exactly 480 px, which
only fits if the screen owns the whole panel. A tab would render below the
existing 60 px top-bar slot, leaving 332 px of viewport and duplicating chrome.
Precedent: `VolumeHudOverlay` is already a stacked full-panel layer with its own
scrim. The layer is **conditionally mounted** (`if root.bt_devices_open`), so it
costs nothing (no bindings, no layout) while closed, and the underlying tab
(usually Now Playing) stays mounted underneath.

**D2 — All Bluetooth D-Bus mutations live in `speaker-bluetooth`.**
`screen-manager` is a consumer; adding BlueZ admin calls there would fork the
D-Bus client model. New modules `crates/bluetooth/src/device_admin.rs` +
`crates/bluetooth/src/pairing_agent.rs`, exposed through `BluetoothDevice`
dispatch (`Backend::Mock` / `Backend::Dbus`), with symmetric mock failure modes
(M-MOCKABLE-SYSCALLS / "Symmetric Mock Error Fidelity on Absent Entities").

**D3 — Reuse the existing `bt-monitor` worker; no second Bluetooth thread.**
One owner of the `BluetoothDevice` handle, one connection, one object server.
New `DeviceCommandQueue` (same shape as `TransportCommandQueue`) drained in the
same tick; new fields on `BtSharedUpdate`; render thread applies via
`AppShell::apply_bt_devices`.

**D4 — Delete confirmation is a modal layer inside the screen**, not a pop-up:
full-panel scrim (`MaterialPalette.background_modal`) + centered card with
`Cancel` (outline) and `Delete` (error). Scroll lock via `flickable.interactive`.
The pending delete is keyed by **MAC address** (`bt_delete_address: string`),
never by row index (audited A2): the worker republishes rows ≤ 1/s during
discovery and reorders them, so an index captured at tap time can point at a
different device by the time the user confirms. `bt_row_delete(int)` resolves
the row's address inside Slint at tap time.

**D9 — Pairing blocks the worker, bounded and documented (audited A3).**
`org.bluez.Device1.Pair` is a synchronous D-Bus call that takes up to ~30 s.
Executed on the single `bt-monitor` worker it stalls the whole monitor loop
(transport, playback position, property updates) for that duration; the
existing `BT_COMMAND_SETTLE` window (750 ms) does not cover it. **Decision:
accept the bounded stall.** Rationale: the pairing screen is modal and the
user is watching it; peer audio keeps flowing via `bluealsa` independent of
this thread; and a dedicated pairing thread would add a second `BluetoothDevice`
consumer to supervise. The worker enforces its own 30 s deadline
(`BT_PAIRING_TIMEOUT`), dispatches `CancelPairing` on user cancel, and the
stall is called out in the worker module docs. Revisit with a dedicated thread
only if live testing shows user-visible playback freezes elsewhere.

**D5 — Screen-manager owns a *temporary* pairing agent.**
`bt-manager` already registers a system-wide auto-trust `DisplayYesNo` agent
(`services/bt-manager/src/agent.rs`) with a 3 s supervisor that re-claims
"default agent" status. Rather than cross-service plumbing
("bt-manager publishes passkeys to screen-manager"), screen-manager registers
`/org/speaker/screen_agent` as the default agent **only for the duration of one
user-initiated pairing**, then unregisters; bt-manager's supervisor reclaims
default within ~3 s. This keeps the passkey in-process (no new D-Bus contract),
is host-testable on a private bus, and logs an `info!` on both hijack and
release. Risk + mitigation in §10.

**D6 — Animations obey the appliance power rules.**
- Ring spin: integer frame `Timer` (33 ms ≈ 30 fps) with `running:` bound to the
  pairing mode only — never `animation-tick()`.
- Scrollbar opacity fade + toast fade: one-shot `animate` (state transitions),
  not perpetual drivers.
- `MainWindow.is_animating` gains `|| (root.bt_devices_open &&
  (root.bt_devices_mode == 2 || root.bt_toast_visible))` so the render loop's
  60 fps branch engages exactly while an animation source is live (audited A9).
  A blanket `|| root.bt_devices_open` would pin the 60 fps branch on a fully
  static list screen. Without this term the ring Timer would dirty the window
  while `is_animating` was false and the loop would idle-sleep.

**D7 — Model = array of structs.** `property <[BtDeviceRow]> bt_device_rows`
(Slint 1.18.1 supports struct arrays; `splash.slint` already exports a struct).
Rust pushes `Vec<BtDeviceRow>` and only when the vector actually changed
(dedup), because a `Text` string change dirties the whole enclosing card in
Slint 1.9.2+ (AGENTS rule 18 — budget ~1 text update/second).

**D8 — Static spec numbers are named constants, positions are computed.**
A local `global BtMetrics` in the new file holds every spec constant (88, 392,
740, 20, 12, 48, 44, 24, 4, 8, 32). All *positions* stay dynamic
(`(parent.height - self.height) / 2`, layout centering) per M-SLINT-MATH-LAYOUT.

## 4. Retiring the Bluetooth Details Tab (tab 2)

Precise removal list. **Tab indices are NOT renumbered** (0 player, 1 vu,
3 cd, 4 dsp, 5 settings; index 2 is retired and never selectable) — renumbering
would silently change the meaning of every existing `--tab N` value for zero
functional gain (repo blast-radius rule).

`ui/viewport.slint`
- Delete the `if root.active_tab == 2` branch (lines 141–207) and the `DetailRow`
  component it alone uses.
- Delete the ten `bt_*` input properties (62–70).
- Settings-hub wiring (line 270, audited A5): `bluetooth_clicked => { root.select_tab(2); }`
  → `bluetooth_clicked => { root.open_bluetooth_devices(); }`, with a new
  `callback open_bluetooth_devices()` on `MainViewport` bound in `appwindow.slint`
  to `open_bt_devices()`. (`settings_view.slint`'s callback itself is unchanged.)

`ui/appwindow.slint`
- Delete properties `bt_connected`, `bt_device_address`, `bt_link_text`,
  `bt_rssi_text`, `bt_battery_text`, `bt_adapter_name`, `bt_adapter_address`
  (91–99). `bt_stream_format` / `bt_delay_text` survive on the KMS base
  (codec HUD #62 consumes them) and are fed by `apply_pcm_stream` instead
  of the deleted details screen.
- Replace `toggle_bluetooth_details()` with `open_bt_devices()` /
  `close_bt_devices()` (see §5; `bt_devices_back()` dropped as dead code).
- Status bar `source_clicked` / `device_clicked` → `open_bt_devices()`
  (lines 175/178). On the KMS base, `protocol_clicked` keeps the
  codec-information overlay from main #62 (the badge opens the codec HUD,
  not the devices screen).
- `app_bar_back()`: drop the `active_tab == 2` arm (the screen owns its own back
  handling).
- ScreenAppBar title: drop the `== 2 ? "Bluetooth"` arm.
- Remove the `bt_*` bindings passed into `MainViewport` (222–230).

`src/ui/shell.rs`
- Delete `apply_bt_link_details` and the text formatters it uses
  (`address_text`, `link_text`, `rssi_text`, `battery_text`,
  `stream_format_text`, `delay_text`, `adapter_address_text`) and the now-unused
  `NO_DETAIL` constant.
- **Keep** `BtLinkDetails::select` (active-device selection) and
  `adapter_name_text` (reused for the "Visible as …" subtitle).
  **(As implemented:** promoted to `#[must_use] pub fn` with a literal `"—"`
  fallback once `NO_DETAIL` was deleted, so it stays reachable and tested.)
- **Keep** `pcm_bit_depth` — it is also used by `apply_pcm_stream` (line 686)
  for the Now Playing status bar.

`src/daemon.rs`
- `BtSharedUpdate.details: Option<BtLinkDetails>` → `devices: Option<BtDevicesUpdate>`.
- `BtMonitorState.last_details` / `last_details()` → `last_devices` / `last_devices()`.
- `publish_link_state` → `publish_devices_state` (rows + discovered + adapter
  alias + pairing status + toast).
- `init_shell_with_config` gains a `bt_devices_on_startup: bool` that opens the
  screen; CLI gains `-B, --bt-devices`. **Mapping (audited A6):** `init_shell_with_config`
  sets `bt_devices_on_startup = true` when `tab == Some(2)`; `-B/--bt-devices`
  sets the same flag directly. `parse_tab_arg` itself is unchanged — it still
  returns `2` for `bt`/`bluetooth`/`2`, and only the init mapping gives that
  value its new meaning.
- `parse_tab_arg`: keep indices 0,1,3,4,5 unchanged; `bt`/`bluetooth`/`2`
  remain accepted aliases and now open the device screen via the mapping above
  (the `--help` text documents this). No external caller passes `--tab` (verified:
  no systemd unit, script, or doc does).
- **As implemented (audited A6):** `-B/--bt-devices` is parse-time sugar that
  sets `opts.tab = Some(2)`, and `init_shell_with_config` maps `Some(2)` to
  `shell.window().invoke_open_bt_devices()` instead of `set_tab(2)` — no new
  field or parameter, same observable mapping. Also as implemented:
  `BtSharedUpdate.details` was deleted outright (nothing consumed it),
  `last_details` kept its name (it feeds active-row derivation), and
  `publish_link_state` caches `BtLinkDetails::select(...)` unconditionally
  (no staging/dedup for a field no longer rendered).

`src/ui/mod.rs` — `BtLinkDetails` stays exported (trimmed shape).

Tests
- `tests/bluetooth_details_integration.rs` is **replaced** by
  `tests/bt_devices_integration.rs`: the `BtLinkDetails::select` preferred/
  fallback coverage migrates into the new file; the assertions on the removed
  `bt_*` text properties are deleted with the properties.
- `tests/daemon_integration.rs`, `tests/ui_shell_integration.rs`,
  `tests/cli_tests.rs`: update the tab assertions that reference 2/bt (§8).

## 5. Slint: `services/screen-manager/ui/bluetooth_devices.slint`

```
global BtMetrics { header_height, viewport_height (derived), card_radius,
                  card_gap, card_height, list_padding_h, scrollbar_width,
                  scrollbar_margin, cue_height, pill_height, pill_height_small }

export struct BtDeviceRow { address, name, subtitle, status, icon, connected, paired }

component BtProgressRing        // integer-frame Timer spinner, primary stroke
component BtThinScrollBar       // 4px track, 8px margin, 1.5s auto-hide, drag + wheel
component BtPill                // primary/outline/error variants, height 48 | 44
component BtIconButton          // 40x40 icon button (trash)
component BtDeviceCard          // 88px card: icon + name/subtitle/status + action + trash
component BtDevicesList         // Flickable + VerticalLayout(rows) + gated cue + scrollbar(above cue)
component BtScreenHeader        // 88px: title, subtitle (+ spinner), right pill
component BtDevicesView         // header("Bluetooth Devices", Visible as…) + list
component BtAddDeviceView       // header("Add Device", "Scanning…" + spinner, Cancel) + list
component BtPairingView         // centered: ring+icon, code box, helper, Cancel
component BtDeleteDialog        // scrim + card + Cancel/Delete
component BtToast               // bottom banner, 3s auto-hide, surface_container_highest
export component BluetoothDevicesScreen inherits Rectangle   // mode switch + layers

// Button mapping per spec (audited A7):
//   `+ Add Device`, `Pair`                 -> FilledButton (primary, pill)
//   `Make Active` / `Connect`              -> TonalButton (filled tonal, per spec)
//   `Cancel`, `Disconnect`, dialog Cancel  -> OutlinedButton (outline)
//   dialog `Delete`                        -> BtDestructivePill (error)
// Overflow cue (audited A1): visible only when content_height > viewport_height.

```

`BluetoothDevicesScreen` is a **child** component (Rectangle root) mounted by
`MainWindow`; it needs no exported Window root / Rust struct (AGENTS rule 11).

### `MainWindow` additions (`ui/appwindow.slint`)

```slint
in-out property <bool>   bt_devices_open: false;
in-out property <int>    bt_devices_mode: 0;        // 0 list, 1 add, 2 pairing
in-out property <[BtDeviceRow]> bt_device_rows: [];
in-out property <[BtDeviceRow]> bt_discovered_rows: [];
in-out property <bool>   bt_scanning: false;
in-out property <string> bt_devices_subtitle: "Visible as \"Home Speaker\"";
in-out property <string> bt_pair_device_name: "";
in-out property <string> bt_pair_code: "";
in-out property <bool>   bt_pair_spinning: false;
in-out property <bool>   bt_delete_open: false;
in-out property <string> bt_delete_device_name: "";
in-out property <string> bt_delete_address: "";   // stable key, NOT a row index (audited A2)
in-out property <bool>   bt_toast_visible: false;
in-out property <string> bt_toast_text: "";

callback bt_add_device_clicked();     callback bt_cancel_scan_clicked();
callback bt_row_action(int, bool);    // (row index, from discovery list)
callback bt_row_delete(int);          // index resolved to row.address at tap time
callback bt_pair_cancel_clicked();    callback bt_delete_confirm_clicked();
callback bt_delete_cancel_clicked();
out property <bool> is_animating: ... || (root.bt_devices_open &&
    ((root.bt_devices_mode == 2 && root.bt_pair_spinning) ||
    (root.bt_devices_mode == 1 && root.bt_scanning) || root.bt_toast_visible));
                                   // audited A9, extended A16, ring-gated A17

public function open_bt_devices()  { bt_devices_open = true; /* mode untouched */ }
public function close_bt_devices() { bt_devices_open = false; reset mode/dialog/toast;
    and pushes StopDiscovery+Refresh while bt_scanning (A18; the worker gates the
    stop on discovery_from_screen so scans stay scoped to the screen) }
// bt_devices_back() dropped as dead code (no callers); pairing/add Cancel
// buttons and the header back already cover every transition it modeled.
// Toast published with no pairing session resets mode 2 -> 1 + spinner
// (converges the optimistic Pair flip; review Tier-1 fix).
// A19: navigation mode writes are Rust-owned — `+ Add Device` sets mode 1 and
// Cancel sets mode 0 in the shell callbacks (the discovery view previously had
// no writer); one-shot toast/pairing publishes bypass the shell dedup gate; a
// refusal while a session is live rides that session's status with its toast;
// the marquee composer is suppressed while the screen is open; the toast timer
// restarts on every new toast; `PairingStatus::Idle` was removed as dead.
```

**As implemented:** `open_bt_devices()` intentionally does NOT reset
`bt_devices_mode` (re-entry while a pairing session is live keeps the pairing
view visible; mode resets in `close_bt_devices()`), and `is_animating` carries
the A16 mode-1 scanning-spinner term shown above.

Navigation inside `BluetoothDevicesScreen` is pure Slint (no Rust):
`Add Device` → mode 1 + `bt_add_device_clicked()`; `Cancel` (add) → mode 0 +
`bt_cancel_scan_clicked()`; pairing `Cancel` / timeout → mode 1 +
`bt_pair_cancel_clicked()`; list back → `close_bt_devices()`.

### Entry points (replacing the details tab)
- Status bar: source (logo), device name, and protocol/codec taps → `open_bt_devices()`.
- Settings hub `Bluetooth` tile → `open_bt_devices()`.
- CLI startup: `--tab bt|bluetooth|2` or `-B/--bt-devices` → open on boot.
- Closing returns to the previously active tab (`previous_tab` already
  maintained by `select_tab`; `open_bt_devices` does not disturb it).

## 6. Rust: `crates/bluetooth` (new files + minimal edits)

### 6.1 `src/device_admin.rs`
```rust
pub enum DeviceCommand {                    // queued by the render thread
    Refresh, StartDiscovery, StopDiscovery,
    Connect(BtAddress), Disconnect(BtAddress), Remove(BtAddress),
    Pair(BtAddress), CancelPairing, ReleaseAgent,
}
pub struct DeviceCommandQueue { ... }      // cloneable, drain()/is_empty(), mirrors TransportCommandQueue
pub enum PairingStatus {                   // published to the UI
    Idle, Connecting { name }, AwaitingConfirmation { name, passkey: u32 },
    Paired { name }, Failed { message: Box<str> }, TimedOut, Cancelled,
}
pub enum DiscoveryPhase { Idle, Scanning, Stopped }
pub struct DeviceAdmin { /* Dbus(BlockingConnection) | Mock(DeviceAdminMock) */ }
impl DeviceAdmin {
    pub fn new_mocked() -> (Self, DeviceAdminMock);
    pub fn start_discovery(&self, adapter: &str) -> Result<(), BtError>;
    pub fn stop_discovery(&self, adapter: &str) -> Result<(), BtError>;
    pub fn connect_device(&self, addr: &BtAddress) -> Result<(), BtError>;
    pub fn disconnect_device(&self, addr: &BtAddress) -> Result<(), BtError>;
    pub fn pair_device(&self, addr: &BtAddress) -> Result<(), BtError>;
    pub fn cancel_pairing(&self, addr: &BtAddress) -> Result<(), BtError>;
    pub fn remove_device(&self, adapter: &str, addr: &BtAddress) -> Result<(), BtError>;
}
pub fn device_object_path(adapter: &str, addr: &BtAddress) -> Result<ObjectPath<'_>, BtError>;
pub fn pairing_failure_message(err: &BtError) -> &'static str;   // pure, unit-tested
pub fn device_action_message(err: &BtError) -> &'static str;     // pure, unit-tested
```
- `zbus` 5.19 proxies for `org.bluez.Adapter1` (StartDiscovery/StopDiscovery/
  RemoveDevice) and `org.bluez.Device1` (Connect/Disconnect/Pair/CancelPairing),
  mirroring the existing proxy style in `bt-manager/src/agent.rs`.
- Device path helper: `/org/bluez/{adapter}/dev_{AA}_{BB}_{CC}_{DD}_{EE}_{FF}`
  (upper-case, `:`→`_`); validated against the existing
  `extract_mac_from_pcm_path` inverse convention.
- **Precondition (audited A8):** BlueZ only exposes a `Device1` object for
  devices it has *discovered* or that are already known/paired — `Connect` /
  `Pair` to an arbitrary address fails with `org.bluez.Error.DoesNotExist`.
  `DeviceCommand::Connect/Disconnect/Remove/Pair` therefore carry a discovered
  or paired address only; the UI never offers those actions outside the lists,
  and `device_action_message` maps `DoesNotExist` to a clear "device not
  available" string as the defensive fallback.
- Errors reuse `BtError` (`BtError::dbus(...)`, `invalid_address`) — **no new
  error variants** (M-ERRORS-CANONICAL-STRUCTS).
- Minimal edits in `connection.rs`: widen `BluetoothDevice::backend` +
  `Backend` to `pub(crate)`, add an `admin: DeviceAdmin` field, construct it in
  `new()/new_blocking()/with_connection()/new_mocked()`, and expose delegation
  setters on `BluetoothDeviceMock` for admin mock state.

### 6.2 `src/pairing_agent.rs`
```rust
pub struct PairingCode { pub device: BtAddress, pub passkey: u32 }   // 0..=999_999
pub struct PairingAgentHandle { /* owns its own BlockingConnection; Drop unregisters + closes */ }
impl PairingAgentHandle {
    pub fn register(codes: &Arc<Mutex<Option<PairingCode>>>) -> Result<Self, BtError>;
    pub fn unregister(self);   // idempotent, logs
}
#[zbus::interface(name = "org.bluez.Agent1")] struct ScreenPairingAgent { codes, conn }
// DisplayPasskey/RequestConfirmation -> record code + auto-confirm + set Trusted
// RequestPinCode -> record "0000" ; RequestAuthorization/AuthorizeService -> ok
// Cancel -> clear code (worker observes and publishes Cancelled)
```
The passkey is observed by the worker through the shared `Arc<Mutex<…>>` — no
polling of D-Bus, no extra thread.

**The agent owns a dedicated `BlockingConnection` (audited A4).** Registering
on `DbusClient`'s shared connection is fragile: the signal-listener thread
clones that connection out of `conn_slot` and tears down/rebuilds around it,
and `DbusClient::drop` closes it — an agent registered there can die mid-pairing,
and Drop ordering (unregister vs. connection close) becomes a hazard. A
dedicated connection is cheap, isolated from listener reconnects, trivially
testable on a private bus, and gives the agent an unconditional Drop order
(unregister on its own connection, then close). Verified:
`zbus::blocking::connection::Builder::serve_at` exists (zbus 5.19.0,
`blocking/connection/builder.rs:241`).

### 6.3 `src/lib.rs`
`#[doc(inline)] pub use device_admin::{DeviceAdmin, DeviceAdminMock, DeviceCommand,
DeviceCommandQueue, PairingStatus, DiscoveryPhase, pairing_failure_message,
device_action_message};` + `pairing_agent::{PairingAgentHandle, PairingCode}`.

## 7. Rust: `services/screen-manager`

### 7.1 `src/ui/bt_devices.rs` (new)
```rust
pub struct BtDevicesUpdate {           // worker -> render thread delta
    pub rows: Vec<BtDeviceRow>, pub discovered: Vec<BtDeviceRow>,
    pub scanning: bool, pub pairing: Option<PairingStatus>,
    pub toast: Option<Box<str>>, pub adapter_alias: Option<Box<str>>,
}
impl AppShell {
    pub fn apply_bt_devices(&self, update: &BtDevicesUpdate);   // dedup by PartialEq
}
pub fn device_row(device: &BtDevice, active_address: Option<BtAddress>) -> BtDeviceRow;
pub fn icon_slug(icon: Option<&str>) -> &'static str;           // pure, mapped table
pub fn subtitle_text(device: &BtDevice) -> String;              // "Phone · AA:BB:…"
pub fn status_text(device: &BtDevice, active: bool) -> &'static str; // Active/Paired/Available
pub fn pairing_code_text(passkey: u32) -> String;               // "482910" (6 digits, zero-padded)
```
Row/model construction is pure and host-testable; no Slint knowledge.

### 7.2 `src/ui/shell.rs`
- `AppShell` gains `bt_devices: Rc<RefCell<BtDevicesController>>` (dedup state) —
  no shadow screen state beyond what Slint already owns
  (M-SINGLE-SOURCE-OF-TRUTH-SLINT).
- `setup_navigation_callbacks` wires the new callbacks: push `DeviceCommand`s
  onto the queue and apply the **optimistic** UI transition immediately
  (mode/toast), mirroring the transport-callback pattern.
- New `wire_bluetooth_devices(&DeviceCommandQueue)` next to `wire_bluetooth_transport`.
- `BtLinkDetails::select` still picks the active device and its PCM; its result
  feeds `active_address` for the row labels.

### 7.3 `src/daemon.rs`
- `BtSharedUpdate` gains `pub devices: Option<BtDevicesUpdate>` (+ merge logic in
  `commit_update`, poisoned-mutex recovery preserved).
- `BtMonitorState` gains `devices: BtDevicesMonitorState`:
  `poll_tick` drains `DeviceCommand`s (executed before the snapshot query, same
  "command landed mid-query" guard as the transport path), then publishes
  devices/discovery/pairing deltas.
- Pairing state machine in the worker (D9):
  `Pair(addr)` → register agent (own connection) → `pair_device()` → publish
  `AwaitingConfirmation{passkey}` when the code arrives →
  `Paired`/`Failed{message}`/`TimedOut` (30 s, `BT_PAIRING_TIMEOUT`) /
  `Cancelled`.
  On any terminal state: unregister agent, `stop_discovery()` if this screen
  started it, then toast + mode change.
  **Documented stall (audited A3):** the blocking `Pair` call freezes the
  monitor loop for up to the pairing duration; this is accepted (modal pairing
  screen; peer audio unaffected) and called out in the worker module docs, with
  the worker-side 30 s deadline and `CancelPairing` bounding it.
- Row publishing suppresses RSSI-only deltas in discovery mode (audited A14):
  discovery re-publishes ≤1/s and RSSI churn would continuously hit the ~1 text
  update/second budget (AGENTS rule 18); RSSI is not displayed on this screen.
- `spawn_bt_monitor_worker` / `bt_monitor_loop` take the extra queue (used by
  `run_render_loop_with_devices` and by the existing test at
  `tests/daemon_integration.rs:886`).
- `apply_shared_bt_state` additionally applies `devices` (one call site).
- Startup: `init_shell_with_config(..., bt_devices_on_startup)`.

## 8. Icons / assets (new, MIT Material Symbols)
`assets/icons/phone.svg`, `laptop.svg`, `tablet.svg`, `headset.svg`,
`delete.svg` — viewBox-normalised to the existing icon style, provenance noted in
an SVG header comment (existing assets mix Material/SVG-Repo sources; keep the
same convention). Note (audited A11): the vendored material lib already ships
`remove.svg`; our own `delete.svg` is a conscious duplicate for asset-style
consistency with the sibling icons. BlueZ icon slug → asset mapping in
`icon_slug`: `phone|telephone → phone`, `computer|laptop|desktop → laptop`,
`tablet → tablet`, `audio-card|headset|headphones|speaker → headset`, else
`bluetooth.svg`.

## 9. Testing Plan

**`crates/bluetooth`** (private `dbus-daemon` via `speaker_common::diag::spawn_private_bus`):
- `tests/device_admin_integration.rs`: fake `org.bluez` serving `Adapter1`,
  `Device1`, `AgentManager1`, `ObjectManager` (the `bluetooth_integration.rs`
  fake pattern at ~line 2330 is the template). Table-driven cases:
  start/stop discovery, connect/disconnect happy path, `RemoveDevice` →
  `AdapterNotFound` / `Dbus` error mapping, `Pair` with an injected
  `DisplayPasskey` (assert the captured code), `Pair` failure error names
  (`AlreadyExists`, `AuthenticationFailed`, `AuthenticationRejected`),
  simulation of a not-paired `Connect`.
- Agent lifecycle test: register → fake BlueZ calls `DisplayPasskey` → assert
  code + Trusted set → unregister → assert `UnregisterAgent` observed.
- Mock-parity tests: `DeviceAdmin::new_mocked()` — discovery results, pair
  success/passkey/failure, connect-while-unpaired error, remove updates the
  mock device list (symmetric failure fidelity).
- Pure-function tests: `pairing_failure_message`, `device_action_message`,
  `device_object_path` (invalid address, signed input, wrong length).

**`services/screen-manager`**
- `tests/bt_devices_integration.rs` (new, `#![cfg(feature = "ui")]`, replaces
  `bluetooth_details_integration.rs`): `Framebuffer::new_mocked()` + `MainWindow`
  + `AppShell`; seeded `BluetoothDevice::new_mocked()` rows; drive the real
  `BtMonitorState`/publish path and the click harness from
  `bluetooth_details_integration.rs`.
  Cases: `BtLinkDetails::select` preferred/fallback (migrated); open/close;
  mode transitions (list → add → pairing → add → list); active card vs paired
  card row fields; trash → dialog open → cancel (rows untouched) → confirm
  (remove command queued); discovery filtering of already-paired devices;
  `Refresh` forces an immediate publish; passkey zero-padding; `TimedOut` toast
  + mode; transient `Err` keeps the last good rows (no placeholder flash);
  overflow cue gated on `content_height > viewport_height` (property-level
  assertion, audited A13); delete dialog keyed by address (index changes
  between tap and confirm do not alter the pending delete, audited A2).
- UI guard tests: `is_animating` true while open/pairing/scanning/toasting
  (assertions on the generated window); the dialog-scroll and cue gates are
  **source-grep** assertions because the list `Flickable` and the cue live
  inside the screen component and are unreachable from generated Rust
  (`interactive: !root.delete_open`, `visible: flick.content_height >
  flick.height;`, scrollbar rendered after the cue), plus a grep guard that the
  new file never reads `animation-tick(`. Keep guard literals out of comments
  in the guarded file so they cannot self-trip.
- `tests/daemon_integration.rs`: extend the worker test with device commands
  drained through the queue; assert the pairing deadline expiry path with a
  short injected deadline (production constant stays unconditional; tests use a
  parameterised variant per the repo rule).
- `tests/cli_tests.rs`: `--help` text, `--tab bt` / `-B/--bt-devices` mapping,
  retired `--tab 2` behaviour, backtrace-free rejections.
- Subprocess tests inherit `SPEAKER_CORE_HEADLESS=1` (already done in
  `cli_tests.rs`).

## 10. Gates (service + workspace, run in order)
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo llvm-cov -p screen-manager --target x86_64-unknown-linux-gnu --fail-under-lines 90 --fail-under-functions 90`
   (then workspace `--fail-under-lines 85`)
4. `cargo build --target aarch64-unknown-linux-gnu --release --bins`
5. After editing `.slint` files: `rm -rf target/*/{debug,release}/build/screen-manager-* target/*/{debug,release}/.fingerprint/screen-manager-*` (AGENTS rule 12), then rebuild.
6. Optional live check on `root@192.168.2.120` (deploy to `/tmp`, run, delete).

## 11. PR sequencing (3 reviewable PRs off this branch)
1. **PR-1 `speaker-bluetooth` admin + agent**: `device_admin.rs`,
   `pairing_agent.rs`, `connection.rs` field/visibility edits, mock, lib re-exports,
   `crates/bluetooth/tests/device_admin_integration.rs`.
2. **PR-2 screen-manager worker + controller**: `bt_devices.rs`, `BtSharedUpdate`
   fields, worker state machine, `shell.rs` wiring, `daemon.rs` spawn/queue.
3. **PR-3 Slint screens + details-tab retirement + assets + UI tests**:
   `bluetooth_devices.slint`, `appwindow.slint` layer, `viewport.slint` cleanup,
   icons, `tests/bt_devices_integration.rs` (replacing
   `bluetooth_details_integration.rs`), CLI flag, doc touch-ups.
   PR-2 lands with a headless/no-UI fallback so the workspace always builds.

## 12. Risks & mitigations
| Risk | Mitigation |
|---|---|
| Two default agents (bt-manager auto-trust + screen pairing agent) | Screen agent is registered **only** for one pairing attempt and unregistered on every terminal path (incl. panic-free `Drop`); `info!`/`warn!` logs on hijack/release; bt-manager's 3 s supervisor reclaims default. The hijack/release contract is distilled into `AGENTS.md` after review (repo §4.3 mandate). |
| `Pair` blocks the `bt-monitor` worker up to ~30 s (audited A3 / D9) | Accepted by decision: modal pairing screen, peer audio unaffected, worker-side 30 s deadline + `CancelPairing` bound it; documented in the worker module docs. Revisit with a dedicated thread only if live testing shows visible freezes. |
| `Device1.Pair` fails when the peer needs the agent *before* the call | Register the agent first, then call `Pair` (ordering asserted by the integration test). |
| Discovery results only arrive via `InterfacesAdded`, which the existing listener does not match | Keep-alive tick re-queries `GetManagedObjects` ≤ 1 s; row updates are deduped. If this proves too slow, extend the listener rule in a follow-up (Tier-1 fix, never silently deferred). |
| Adapter powered off / `bluetoothd` absent | `start_discovery` error → toast + stay on list; adapter errors map through `BtError::is_adapter_not_found()` → `AdapterNotFound` message. |
| `RemoveDevice` while the device streams audio | Disconnect-then-remove ordering in the worker; audio-manager volume path is untouched (`amp-volume` worker independent). |
| Removing tab 2 breaks startup scripts | No caller passes `--tab` (verified: no systemd unit, script, or doc); indices 0,1,3,4,5 keep their meaning and `bt`/`bluetooth`/`2` open the new screen. |
| Slint struct-array codegen friction | Fallback: single `[BtDeviceRow]` list plus a `mode` filter (still no fixed max). First compile in PR-3 proves it. |
| Per-frame cost on BCM2837 | Ring at 30 fps only during pairing; row text updates deduped ≤ ~1/s (RSSI-only deltas suppressed in discovery mode, A14); scrollbar/gradient use `visible` not conditional `if`; `is_animating` narrowed to live animation sources (A9/A16). |
| Overflow cue falsely signals scrollability on short lists (audited A1) | Cue gated on `content_height > viewport_height`; the spec's "35 % exposed" 4th-card state is unreachable with 88/12/392 and is documented as a deviation. |

## 13. Decisions taken (user-confirmed + audit resolutions)
1. **Entry point**: the new screen *replaces* the old details screen — retire
   tab 2; every previous entry point (status bar source/device/protocol taps,
   Settings `Bluetooth` tile, `--tab bt`) opens the new screen.
2. **Pairing confirmation**: auto-confirm after showing the passkey (matches the
   spec's Cancel-only pairing screen and bt-manager's existing behaviour).
3. **Trash on discovery rows**: hidden until paired (spec mock shows delete only
   on paired/active rows).
4. **Header subtitle**: mirror the live adapter alias, defaulting to
   `"Home Speaker"`.
5. **Pairing stall (audit A3)**: accepted as D9 — blocking `Pair` on the worker
   with a 30 s worker deadline, no dedicated pairing thread unless live testing
   shows visible freezes.