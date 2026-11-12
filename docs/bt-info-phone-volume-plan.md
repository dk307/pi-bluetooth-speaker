# bt-info Phone Volume Enhancement Plan

## Overview
Enhance the `bt-info` diagnostic utility to display **phone/remote volume** (AVRCP absolute volume from BlueZ `MediaPlayer1.Volume`) alongside the existing **hardware/BlueALSA volume** (local Pi volume from BlueALSA PCM).

## Current State
- **Hardware Volume**: Displayed in `print_human_audio_pipeline()` as "Hardware Volume: XX% (L: 127 R: 127)" from `BlueAlsaPcm.volume_percent` and `BlueAlsaPcm.volume`
- **Phone Volume**: NOT displayed. Available via BlueZ `MediaPlayer1.Volume` property (uint16, 0-127)

## Changes Required

### 1. `crates/bluetooth/src/transport.rs` - `BtPlaybackState` struct
Add phone volume field:
```rust
/// Phone/remote device volume (AVRCP absolute volume, 0-127), if reported by MediaPlayer1.
pub phone_volume: Option<u16>,
```

### 2. `crates/bluetooth/src/connection.rs` - `parse_media_player_props()`
Extract Volume property from MediaPlayer1 props:
```rust
let phone_volume = get_u16(props, "Volume").filter(|&v| v <= 127);
```

Add to `BtPlaybackState` construction.

### 3. `tools/diagnostics/src/cli/bt.rs` - Display functions
Update `print_human_audio_pipeline()` to show both volumes:
- "Hardware Volume: XX% (L: 127 R: 127) (Muted: NO)" — existing
- "Phone Volume   : XX% (AVRCP absolute)" — NEW

Update `output_json_status()` to include phone_volume in JSON output.

### 4. Tests
- Update integration tests to include phone_volume in mock data
- Verify both volumes display correctly in human and JSON formats

## Volume Semantics
| Volume Type | Source | Range | Meaning |
|-------------|--------|-------|---------|
| Hardware (BlueALSA) | `BlueALSA PCM.Volume` (ay) | 0-127 per channel, displayed as 0-100% | Local Pi output volume |
| Phone (AVRCP) | `BlueZ MediaPlayer1.Volume` (uint16) | 0-127 | Remote device (phone) volume |

## Implementation Notes
- AVRCP absolute volume is 0-127; convert to percentage for display: `(volume * 100 + 63) / 127`
- When absolute volume is disabled on phone, MediaPlayer1.Volume may not be present or may not reflect actual phone volume
- Both volumes are independent; phone volume controls source gain, hardware volume controls sink gain