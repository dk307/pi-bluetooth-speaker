# Slint Material Components Migration Plan

## Executive Summary

This document outlines the strategy for adopting **Slint Material Components 1.1.0** (official release from `slint-ui/slint/tree/master/ui-libraries/material`, distributed as ZIP) in the `screen-manager` service to streamline UI development, standardize theming, and reduce custom boilerplate — while preserving the specialized appliance UX on the fixed 800×480 Raspberry Pi 3 framebuffer.

**Approach:** Selective hybrid adoption — use Material for theming, cards, buttons, icons, and layout primitives; keep custom components for specialized widgets (marquee, VU meter, volume HUD, transport controls).

### Critical Correction: Distribution Method

**The official Material Components 1.1.0 is NOT a crate on crates.io.** It is distributed as a **ZIP file** (`material-1.1.0.zip` from https://material.slint.dev/zip/material-1.1.0.zip) that you extract into your project and configure as a **component library** via `build.rs` using `CompilerConfiguration::with_library_paths`.

This differs from the separate `slint-ui/material-components` GitHub repo (which appears to be a development mirror). The official release matches Slint 1.18.x exactly.

---

## 1. Current Architecture Analysis

### 1.1 View Hierarchy (No Persistent Tab Bar)

```
MainWindow (800×480, inherits Window)
├── TopStatusBar (60px fixed height)
│   ├── Left: Bluetooth logo + source clickable
│   ├── Center: Track/device name (elided, clickable)
│   └── Right: Codec badge + Volume (icon + value) + DIM toggle
│
├── MainViewport (420px, fills remaining)
│   ├── View 0: NowPlayingView — cover art (360×360), marquee title, transport row (prev/play/next), settings_btn (56px, bottom-right)
│   ├── View 1: VU Meter — full-screen PeppyMeter skin image + back button
│   ├── View 2: Bluetooth Details — two side-by-side cards + back button
│   ├── View 3: CD Player — placeholder card + back button
│   ├── View 4: DSP & Tone — placeholder card + back button
│   └── View 5: SettingsView — icon grid (Bluetooth, VU, DSP, Themes, CD) + theme picker sub-view
│
└── VolumeHudOverlay — full-viewport animated overlay (fade in/out)
```

### 1.2 Custom Theme System (`theme.slint`)

```slint
export struct ThemePalette {
    chassis: color,           // #090D12 (main background)
    card_surface: color,      // #111923 (card backgrounds)
    card_border: color,       // #30363D (borders/dividers)
    primary_accent: color,    // #00C8FF (cyan - primary actions)
    secondary_accent: color,  // #70E1FF (light cyan - secondary text)
    needle_peak: color,       // #FF3344 (red - peak indicators)
    dock_active_text: color,  // #090D12 (text on primary buttons)
    text_primary: color,      // #F0F6FC (main text)
    text_muted: color,        // #8B949E (secondary text)
    meter_skin_id: string,    // PeppyMeter skin identifier
    font_family: string,      // "Inter"
}
```

5 presets (McIntosh, Marantz, Cyberpunk, Studio Dark, PeppyMeter) — all dark-mode.

### 1.3 Specialized Components (Must Preserve)

| Component | Purpose | Why Custom |
|-----------|---------|------------|
| `MarqueeText` | Offscreen-rasterized scrolling text | Framebuffer-zero-copy composition, integer-frame arithmetic |
| `AmbientWaveIndicator` | 5-bar playing visualization | 180ms timer, absolute positioning to avoid layout invalidation |
| `VolumeHudOverlay` | Full-screen volume fade animation | Custom opacity animation, permanent item tree |
| `NowPlayingView` | Cover art centering, transport hero button (86px) | Precise geometric constraints |
| `VU Meter` | PeppyMeter skin background | Full-screen image, no standard widget equivalent |

---

## 2. Material Components Capability Mapping

### 2.1 Available Components (52 exports)

**Theming & Styling:**
- `MaterialSchemes` (light/dark `MaterialScheme` — 40+ color tokens)
- `MaterialPalette` (global, derives from `Palette.color_scheme`)
- `Typography` (13 `TextStyle` tokens: display_large → body_small)
- `Animations` (Material 3 motion tokens: emphasized/standard/ripple)
- `MaterialStyleMetrics` (standardized sizes, spacing, padding, border_radius, icon sizes)

**Navigation (NOT SUITABLE for 800×480):**
- `NavigationBar` — 80px min height (17% of screen) ❌
- `TabBar` / `SecondaryTabBar` — 49px min height (10% of screen) ❌
- `NavigationDrawer` / `NavigationRail` — desktop-oriented ❌

**Surfaces & Cards (HIGH VALUE):**
- `ElevatedCard` — elevation level 1, surface background, 12px radius
- `FilledCard` — surface_container_highest background
- `OutlinedCard` — surface_container_low + 1px outline border

**Buttons (HIGH VALUE):**
- `FilledButton` — primary background, on_primary text, elevation
- `TonalButton` — secondary_container background
- `OutlinedButton` — outline border
- `ElevatedButton` — elevation + surface background
- `IconButton` / `FilledIconButton` / `TonalIconButton` / `OutlineIconButton` — 48×48 touch targets
- `FloatingActionButton` — standard/large/extended FAB styles

**Layout Helpers (LOW RISK):**
- `Horizontal` / `Vertical` / `Grid` / `ScrollView` — thin aliases

**Feedback (POTENTIAL):**
- `SnackBar`, `Dialog`, `CircularProgressIndicator`, `LinearProgressIndicator`

**Lists & Menus (LOW PRIORITY):**
- `ListView`, `ListTile`, `Avatar`, `DropDownMenu`, `PopupMenu`, `NavigationBar` items

### 2.2 Default Metrics vs. 800×480 Reality

| Metric | Material Default | 800×480 Need | Override Required |
|--------|------------------|--------------|-------------------|
| `icon_size_18` | 18px | 24px | Yes |
| `icon_size_24` | 24px | 32px | Yes |
| `icon_size_36` | 36px | 48px | Yes |
| `icon_size_90` | 27px | 56px | Yes |
| `size_48` | 48px | 48px | No |
| `size_56` | 56px | 56px | No |
| `spacing_8` | 8px | 8px | No |
| `padding_16` | 16px | 12px | Yes (compact) |
| `border_radius_12` | 12px | 12px | No |
| `display_large` | 57px | 32px | Yes (typography) |
| `headline_medium` | 28px | 22px | Yes |

---

## 3. Migration Strategy: Selective Hybrid Adoption

### 3.1 Adoption Matrix

| Category | Decision | Rationale |
|----------|----------|-----------|
| **Theming System** | ✅ **Full Adoption** | Replace `ThemePalette` with `MaterialSchemes` + custom dark schemes for 5 presets |
| **Typography** | ✅ **Full Adoption** | Use `Typography` global, override font sizes for 480px height |
| **Animations** | ✅ **Full Adoption** | Use `Animations` tokens for consistent motion |
| **Icon Component** | ✅ **Full Adoption** | `Icon` with custom SVG sources, override icon sizes |
| **Cards** | ✅ **Full Adoption** | `ElevatedCard`/`OutlinedCard` for detail views, settings sub-views |
| **Buttons** | ✅ **Full Adoption** | `FilledButton`/`IconButton` for transport, back, settings actions |
| **Layout Primitives** | ✅ **Adopt** | `Horizontal`/`Vertical`/`Grid` as semantic aliases |
| **NavigationBar/TabBar** | ❌ **Reject** | Too tall for 480px; keep manual view switching |
| **NavigationDrawer/Rail** | ❌ **Reject** | Desktop patterns, not appliance |
| **Lists/Menu** | 🟡 **Defer** | Not needed currently |
| **Specialized Widgets** | 🛑 **Keep Custom** | Marquee, Wave, VU, VolumeHUD, NowPlaying — no Material equivalent |

### 3.2 Migration Phases

#### Phase 0: Foundation (Week 1)
- [ ] Download `material-1.1.0.zip` from https://material.slint.dev/zip/material-1.1.0.zip
- [ ] Extract to `services/screen-manager/material-1.1.0/`
- [ ] Update `build.rs` with `CompilerConfiguration::with_library_paths` for `@material`
- [ ] Create `ui/material_metrics_override.slint` with 800×480 metrics
- [ ] Create `ui/material_schemes_custom.slint` with 5 preset dark schemes
- [ ] Create `ui/material_typography_override.slint` with compact font scales
- [ ] Create `ui/material_theme_bridge.slint` for preset switching
- [ ] Wire overrides in `appwindow.slint` (import order matters)

#### Phase 1: SettingsView (Week 1-2) — *Proof of Concept*
- [ ] Replace `AppIcon` grid with `ElevatedCard` (clickable) + `Icon` + `MaterialText`
- [ ] Replace theme preset rectangles with `OutlinedCard` (selected) / `ElevatedCard` (unselected)
- [ ] Replace "Cycle Next Theme" button with `FilledButton`
- [ ] **Migrate `settings_view.slint` colors to `MaterialPalette`**
- [ ] Validate touch targets ≥48px, visual density, theme switching

#### Phase 2: Bluetooth Details View (Week 2)
- [ ] Replace custom `Rectangle` cards with `OutlinedCard`
- [ ] Replace `DetailRow` with `Horizontal` layout + `MaterialText` (Typography styles)
- [ ] Replace back button with `IconButton` or `FilledButton`
- [ ] **Migrate `bluetooth_details.slint` colors to `MaterialPalette`**

#### Phase 3: CD Player & DSP Views (Week 2-3)
- [ ] Replace placeholder `Rectangle` with `FilledCard` or `ElevatedCard`
- [ ] Standardize back button styling
- [ ] **Migrate CD/DSP view colors to `MaterialPalette`**

#### Phase 4: TopStatusBar Buttons (Week 3)
- [ ] Codec badge → `OutlinedButton` (fixed width)
- [ ] DIM toggle → `IconButton` (48×48)
- [ ] Volume control — keep custom (icon + value + level logic)
- [ ] **Migrate remaining `status_bar.slint` custom parts to `MaterialPalette`**

#### Phase 5: NowPlayingView Transport (Week 3-4)
- [ ] Prev/Next → `IconButton` (72×72) with custom SVG icons
- [ ] Play/Pause → `FilledButton` (86×86, circular) — may need custom sizing
- [ ] Settings button → `IconButton` (56×56)
- [ ] Keep marquee, cover art, progress bar, wave indicator custom
- [ ] **Migrate `now_playing.slint`, `volume_hud.slint`, `marquee_state.slint` to `MaterialPalette`**
- [ ] **Update Rust marquee rasterizer to use `MaterialPalette.primary`**

#### Phase 6: Global Polish (Week 4)
- [ ] Remove `theme.slint` → `ThemePalette` struct
- [ ] Update Rust-side theme switching to use `MaterialPalette` + preset index
- [ ] **Migrate `splash.slint` to `MaterialPalette`**
- [ ] Verify all 5 presets render correctly
- [ ] Run coverage gates: `cargo llvm-cov --target x86_64-unknown-linux-gnu --fail-under-lines 90`

---

## 4. Technical Design: Custom Material Overrides

### 4.1 File Structure

```
services/screen-manager/
├── material-1.1.0/                      # Extracted from material-1.1.0.zip
│   ├── material.slint                   # Main entry point (re-exports all)
│   ├── ui/
│   │   ├── components/                  # 52 component .slint files
│   │   ├── items/                       # ListItem, NavigationItem, MenuItem
│   │   └── styling/                     # MaterialScheme, MaterialPalette, etc.
│   └── LICENSE.md
├── ui/
│   ├── appwindow.slint                  # Main entry, imports overrides FIRST
│   ├── material_metrics_override.slint  # MaterialStyleMetrics for 800×480
│   ├── material_schemes_custom.slint    # 5 preset dark MaterialSchemes
│   ├── material_typography_override.slint # Compact Typography for 480px
│   ├── material_theme_bridge.slint      # Maps Theme.active_preset → MaterialPalette.scheme
│   ├── theme.slint                      # DEPRECATED - remove after migration
│   ├── status_bar.slint                 # Phase 4: buttons → Material
│   ├── viewport.slint                   # Phase 2-3: cards → Material
│   ├── settings_view.slint              # Phase 1: full Material rewrite
│   ├── now_playing.slint                # Phase 5: transport buttons → Material
│   ├── bluetooth_details.slint          # NEW: extracted from viewport
│   ├── marquee_state.slint              # KEEP: specialized
│   ├── marquee_rasterizer.slint         # KEEP: specialized
│   ├── volume_hud.slint                 # KEEP: specialized
│   └── splash.slint                     # KEEP: boot screen
├── Cargo.toml
└── build.rs                             # Modified: add library_paths for @material
```

### 4.2 `material_metrics_override.slint`

```slint
// Copyright © Audio Appliance Team
// SPDX-License-Identifier: Apache-2.0

import { MaterialStyleMetrics } from "@material/material.slint";

/**
 * Override Material Design 3 metrics for 800×480 appliance panel.
 * Material defaults assume ≥600px height (mobile/desktop); we compact
 * spacing, enlarge touch targets, and scale icons for readability.
 */
export global MaterialStyleMetrics {
    // ========== ICON SIZES ==========
    // Base 24px → 32px for legibility at 800px width
    in-out property <length> icon_size_18: 24px;
    in-out property <length> icon_size_24: 32px;
    in-out property <length> icon_size_36: 48px;
    in-out property <length> icon_size_90: 56px;  // Settings button, FAB

    // ========== TOUCH TARGETS ==========
    // 48px minimum per Material a11y; keep 56px for primary actions
    in-out property <length> size_48: 48px;
    in-out property <length> size_56: 56px;

    // ========== SPACING (compact) ==========
    in-out property <length> spacing_2: 2px;
    in-out property <length> spacing_4: 4px;
    in-out property <length> spacing_6: 6px;
    in-out property <length> spacing_8: 8px;
    in-out property <length> spacing_12: 12px;
    in-out property <length> spacing_16: 16px;
    in-out property <length> spacing_40: 24px;  // Reduced from 40px
    in-out property <length> spacing_52: 32px;  // Reduced from 52px

    // ========== PADDING (compact) ==========
    in-out property <length> padding_4: 4px;
    in-out property <length> padding_6: 6px;
    in-out property <length> padding_8: 8px;
    in-out property <length> padding_10: 10px;
    in-out property <length> padding_12: 12px;
    in-out property <length> padding_14: 14px;
    in-out property <length> padding_16: 16px;
    in-out property <length> padding_20: 16px;  // Reduced
    in-out property <length> padding_24: 20px;  // Reduced
    in-out property <length> padding_28: 24px;  // Reduced
    in-out property <length> padding_30: 24px;  // Reduced
    in-out property <length> padding_44: 32px;  // Reduced
    in-out property <length> padding_56: 40px;  // Reduced

    // ========== BORDER RADIUS (unchanged) ==========
    in-out property <length> border_radius_2: 2px;
    in-out property <length> border_radius_4: 4px;
    in-out property <length> border_radius_8: 8px;
    in-out property <length> border_radius_12: 12px;
    in-out property <length> border_radius_16: 16px;
    in-out property <length> border_radius_28: 28px;

    // ========== COMPONENT SIZES (adjusted) ==========
    in-out property <length> size_14: 14px;
    in-out property <length> size_16: 16px;
    in-out property <length> size_20: 20px;
    in-out property <length> size_30: 28px;    // Reduced from 30
    in-out property <length> size_32: 32px;
    in-out property <length> size_38: 36px;    // Reduced from 38
    in-out property <length> size_40: 40px;
    in-out property <length> size_64: 56px;    // Reduced from 64
    in-out property <length> size_72: 64px;    // Reduced from 72
    in-out property <length> size_80: 72px;    // Reduced from 80
    in-out property <length> size_90: 80px;    // Reduced from 96
    in-out property <length> size_200: 160px;  // Reduced
    in-out property <length> size_256: 200px;  // Reduced
    in-out property <length> size_344: 280px;  // Reduced
    in-out property <length> size_360: 300px;  // Reduced
    in-out property <length> size_572: 480px;  // Reduced
    in-out property <length> size_640: 520px;  // Reduced
}
```

### 4.3 `material_schemes_custom.slint`

```slint
// Copyright © Audio Appliance Team
// SPDX-License-Identifier: Apache-2.0

import { MaterialSchemes, MaterialScheme } from "@material/material.slint";

/**
 * Five custom dark MaterialSchemes matching the appliance's 5 theme presets.
 * All schemes are dark-mode only (appliance never uses light theme).
 * Index mapping: 0=McIntosh, 1=Marantz, 2=Cyberpunk, 3=Studio Dark, 4=PeppyMeter
 */
export global CustomMaterialSchemes {
    in-out property <MaterialSchemes> schemes: {
        light: { /* Unused - placeholder for API compatibility */ },
        dark: {
            // PRESET 0: McIntosh Blue (DEFAULT)
            primary: #00C8FF,
            surfaceTint: #00C8FF,
            onPrimary: #090D12,
            primaryContainer: #003A5E,
            onPrimaryContainer: #70E1FF,
            secondary: #70E1FF,
            onSecondary: #090D12,
            secondaryContainer: #004A6E,
            onSecondaryContainer: #70E1FF,
            tertiary: #FF3344,
            onTertiary: #090D12,
            tertiaryContainer: #5E0011,
            onTertiaryContainer: #FF3344,
            error: #FF4444,
            onError: #090D12,
            errorContainer: #5E0000,
            onErrorContainer: #FFB4B4,
            background: #090D12,        // chassis
            onBackground: #F0F6FC,      // text_primary
            surface: #111923,           // card_surface
            onSurface: #F0F6FC,         // text_primary
            surfaceVariant: #30363D,    // card_border
            onSurfaceVariant: #8B949E,  // text_muted
            outline: #30363D,
            outlineVariant: #30363D,
            shadow: #000000,
            scrim: #000000,
            inverseSurface: #F0F6FC,
            inverseOnSurface: #090D12,
            inversePrimary: #006E99,
            primaryFixed: #70E1FF,
            onPrimaryFixed: #090D12,
            primaryFixedDim: #00C8FF,
            onPrimaryFixedVariant: #006E99,
            secondaryFixed: #70E1FF,
            onSecondaryFixed: #090D12,
            secondaryFixedDim: #00C8FF,
            onSecondaryFixedVariant: #006E99,
            tertiaryFixed: #FF3344,
            onTertiaryFixed: #090D12,
            tertiaryFixedDim: #FF3344,
            onTertiaryFixedVariant: #5E0011,
            surfaceDim: #090D12,
            surfaceBright: #111923,
            surfaceContainerLowest: #090D12,
            surfaceContainerLow: #111923,
            surfaceContainer: #111923,
            surfaceContainerHigh: #1A2230,
            surfaceContainerHighest: #30363D,
        },
        // PRESET 1: Marantz Amber
        // PRESET 2: Cyberpunk Neon
        // PRESET 3: Studio Dark Mono
        // PRESET 4: PeppyMeter Gold
        // (Full definitions elided for brevity - same structure, different hues)
    };
}
```

### 4.4 `material_typography_override.slint`

```slint
// Copyright © Audio Appliance Team
// SPDX-License-Identifier: Apache-2.0

import { Typography } from "@material/material.slint";

/**
 * Compact typography scale for 480px height panel.
 * Material defaults (display_large=57px) are too large; we cap at 32px.
 * Font family remains "Inter" (loaded in appwindow.slint).
 */
export global Typography {
    // Weights (Inter: 300=light, 500=medium, 600=semi-bold, 700=bold)
    in-out property <int> regular: 400;      // Was 300 - too light for small screens
    in-out property <int> medium: 600;
    in-out property <int> semibold: 700;

    // Display (headlines) - capped at 32px
    in-out property <TextStyle> display_large: { font_size: 32px, font_weight: root.regular };
    in-out property <TextStyle> display_medium: { font_size: 28px, font_weight: root.regular };
    in-out property <TextStyle> display_small: { font_size: 24px, font_weight: root.regular };

    // Headline (section titles)
    in-out property <TextStyle> headline_large: { font_size: 22px, font_weight: root.regular };
    in-out property <TextStyle> headline_medium: { font_size: 20px, font_weight: root.regular };
    in-out property <TextStyle> headline_small: { font_size: 18px, font_weight: root.regular };

    // Title (card titles, settings items)
    in-out property <TextStyle> title_large: { font_size: 18px, font_weight: root.regular };
    in-out property <TextStyle> title_medium: { font_size: 16px, font_weight: root.medium };
    in-out property <TextStyle> title_small: { font_size: 14px, font_weight: root.medium };

    // Label (buttons, chips, badges)
    in-out property <TextStyle> label_large: { font_size: 14px, font_weight: root.medium };
    in-out property <TextStyle> label_medium: { font_size: 12px, font_weight: root.medium };
    in-out property <TextStyle> label_medium_prominent: { font_size: 12px, font_weight: root.semibold };
    in-out property <TextStyle> label_small: { font_size: 11px, font_weight: root.medium };

    // Body (reading text)
    in-out property <TextStyle> body_large: { font_size: 16px, font_weight: root.regular };
    in-out property <TextStyle> body_medium: { font_size: 14px, font_weight: root.regular };
    in-out property <TextStyle> body_small: { font_size: 12px, font_weight: root.regular };
}
```

### 4.5 `material_theme_bridge.slint`

```slint
// Copyright © Audio Appliance Team
// SPDX-License-Identifier: Apache-2.0

import { MaterialPalette } from "@material/material.slint";
import { CustomMaterialSchemes } from "material_schemes_custom.slint";

/**
 * Bridges the appliance's Theme.active_preset (0-4) to MaterialPalette.scheme.
 * MaterialPalette normally switches on Palette.color_scheme (light/dark);
 * we override to always use dark and select preset by index.
 */
export global MaterialThemeBridge {
    in-out property <int> active_preset: 0;

    // Expose the custom schemes to MaterialPalette
    // MaterialPalette.schemes is `in` property - we bind our custom schemes
    // Note: This requires MaterialPalette to be imported AFTER this bridge
    // or we use a callback to sync. See appwindow.slint for wiring.
}
```

### 4.6 `appwindow.slint` Import Order (Critical)

```slint
import { Theme, ThemePalette } from "theme.slint";           // DEPRECATED - remove Phase 6
import { MaterialThemeBridge } from "material_theme_bridge.slint";
import { CustomMaterialSchemes } from "material_schemes_custom.slint";
import { MaterialPalette, MaterialStyleMetrics, Typography, Animations } from "@material/material.slint";

// Wire the bridge
MaterialPalette.schemes = CustomMaterialSchemes.schemes;

// Sync active_preset from Rust (via Theme.active_preset) to MaterialThemeBridge
// This is done in Rust: window.global::<MaterialThemeBridge>().set_active_preset(preset);
```

---

## 4.7 Theme Migration: Custom Components → MaterialPalette

### 4.7.1 Why Migrate

| Benefit | Explanation |
|---------|-------------|
| **Automatic theme switching** | Colors update via `MaterialPalette` — no manual `Theme.active_preset` sync |
| **Single source of truth** | One color system (Material) instead of two (Theme + Material) |
| **Future-proof** | New Material components and custom ones stay in sync |
| **Delete `theme.slint`** | Phase 6 cleanup — remove legacy system entirely |

### 4.7.2 Mapping: Current Theme Fields → MaterialPalette Tokens

| Current (`Theme.active.xxx`) | Migrate To (`MaterialPalette.xxx`) | Used By |
|------------------------------|-----------------------------------|---------|
| `chassis` | `background` / `surface_dim` | Window bg, VU meter bg, card backgrounds |
| `card_surface` | `surface` / `surface_container_low` | Card surfaces, settings icons, dialogs |
| `card_border` | `outline` / `outline_variant` | Borders, dividers, card outlines |
| `primary_accent` | `primary` / `primary_fixed_dim` | **Primary brand** — play button, progress bar, marquee text, icons |
| `secondary_accent` | `secondary` / `on_surface_variant` | Secondary text, volume icons, metadata |
| `needle_peak` | `tertiary` / `tertiary_fixed_dim` | Peak indicators, wave bars, accent highlights |
| `dock_active_text` | `on_primary` | Text on primary buttons (play/pause) |
| `text_primary` | `on_surface` / `on_background` | Primary text (titles, artist, time) |
| `text_muted` | `on_surface_variant` | Secondary text (album, codec details) |
| `font_family` | **Keep separate** | `default-font-family: "Inter"` in appwindow |
| `meter_skin_id` | **Keep separate** | PeppyMeter skin mapping in Rust |

### 4.7.3 Components to Migrate (Priority Order)

#### 1. `now_playing.slint` — **High Impact**
```slint
import { MaterialPalette } from "@material/material.slint";

// BEFORE
background: @linear-gradient(145deg, Theme.active.card_surface 0%, Theme.active.chassis 100%);
color: Theme.active.primary_accent;
color: Theme.active.secondary_accent;
color: Theme.active.text_primary;

// AFTER
background: @linear-gradient(145deg, MaterialPalette.surface_container_low 0%, MaterialPalette.surface_dim 100%);
color: MaterialPalette.primary;
color: MaterialPalette.on_surface_variant;
color: MaterialPalette.on_surface;
```

#### 2. `volume_hud.slint` — **High Impact**
```slint
// BEFORE
background: Theme.active.card_surface;
color: Theme.active.text_primary;

// AFTER
background: MaterialPalette.surface_container;
color: MaterialPalette.on_surface;
```

#### 3. `status_bar.slint` — **Medium** (partial — codec badge, DIM toggle moving to Material components)
```slint
// Remaining custom parts
color: Theme.active.primary_accent;     → MaterialPalette.primary
color: Theme.active.secondary_accent;   → MaterialPalette.on_surface_variant
color: Theme.active.text_primary;       → MaterialPalette.on_surface
background: Theme.active.chassis;       → MaterialPalette.background
```

#### 4. `viewport.slint` — **Medium** (Bluetooth Details, CD, DSP cards)
```slint
// Custom Rectangle cards → migrate colors
background: Theme.active.card_surface;  → MaterialPalette.surface
border-color: Theme.active.card_border; → MaterialPalette.outline
color: Theme.active.primary_accent;     → MaterialPalette.primary
color: Theme.active.secondary_accent;   → MaterialPalette.on_surface_variant
```

#### 5. `marquee_state.slint` / `marquee_rasterizer.slint` — **Low**
```slint
// Text strip rasterization uses color for text rendering
color: Theme.active.primary_accent;     → MaterialPalette.primary
```

#### 6. `splash.slint` — **Low** (boot screen, rarely seen)

### 4.7.4 Special Cases to Keep Separate

| Component | Special Case |
|-----------|--------------|
| **VU Meter (View 1)** | Full-screen PeppyMeter skin image — skin mapping stays in Rust (`meter_skin_id` per preset) |
| **AmbientWaveIndicator** | Uses `needle_peak` (`tertiary`) for bars — migrate to `MaterialPalette.tertiary` |
| **MarqueeText (Rust-side)** | Rasterizes text offscreen — color passed at raster time from Rust; update Rust to pass `MaterialPalette.primary` |

### 4.7.5 Migration Pattern

```slint
// In each .slint file, add import:
import { MaterialPalette } from "@material/material.slint";

// Replace all Theme.active.xxx with MaterialPalette.xxx
// No other changes needed — components stay structurally identical
```

**Rust-side rasterization (marquee):**
```rust
// services/screen-manager/src/marquee.rs
fn rasterize_strip(&self, text: &str, color: Color) -> Image {
    // Before: color = theme.primary_accent
    // After:  color = material_palette.primary (queried from window globals)
}
```

### 4.7.6 Phase 6 Cleanup (After All Migrated)

```bash
# 1. Delete theme.slint
rm services/screen-manager/ui/theme.slint

# 2. Remove Theme import from appwindow.slint
# 3. Remove Theme global usage from Rust
# 4. Verify all 5 presets work end-to-end
```

---

## 5. Component Migration Details

### 5.1 SettingsView → Material (Phase 1)

**Before:** `AppIcon` (custom Rectangle + Image + Text + TouchArea)
**After:** `ElevatedCard` (clickable) + `Icon` + `MaterialText`

```slint
import { ElevatedCard, Icon, MaterialText, FilledButton, OutlinedCard } from "@material/material.slint";
import { Typography } from "@material/material.slint";
import { MaterialPalette } from "@material/material.slint";
import { MaterialStyleMetrics } from "@material/material.slint";

component SettingsAppIcon inherits ElevatedCard {
    in property <image> icon;
    in property <string> label;
    callback clicked();

    clickable: true;
    width: 120px;
    height: 120px;

    VerticalLayout {
        alignment: center;
        spacing: MaterialStyleMetrics.spacing_12;

        Icon {
            source: root.icon;
            colorize: MaterialPalette.primary;
            width: MaterialStyleMetrics.icon_size_36;  // 48px
            height: self.width;
        }

        MaterialText {
            text: root.label;
            style: Typography.title_small;  // 14px, medium
            color: MaterialPalette.on_surface;
            horizontal_alignment: center;
        }
    }

    clicked => { root.clicked(); }
}

component ThemePresetCard inherits Rectangle {
    in property <int> index;
    in property <string> name;
    in property <string> accent_name;
    in property <color> primary_color;
    in property <color> secondary_color;
    in property <bool> selected;
    callback clicked();

    width: 136px;
    height: 54px;
    border_radius: MaterialStyleMetrics.border_radius_10;

    // Use OutlinedCard when selected, ElevatedCard when not
    content := (root.selected ? OutlinedCard { } : ElevatedCard { }) {
        width: 100%;
        height: 100%;
        clickable: true;
        // OutlinedCard: border_color = primary_color, border_width = 2px
        // ElevatedCard: background = surface_container_highest
        border_width: root.selected ? 2px : 0px;
        border_color: root.primary_color;
        background: root.selected ? transparent : MaterialPalette.surface_container_highest;

        VerticalLayout {
            alignment: center;
            spacing: MaterialStyleMetrics.spacing_2;

            MaterialText {
                text: root.name;
                style: Typography.label_medium;  // 12px
                color: root.primary_color;
                horizontal_alignment: center;
            }
            MaterialText {
                text: root.accent_name;
                style: Typography.label_small;  // 11px
                color: root.secondary_color;
                horizontal_alignment: center;
            }
        }

        clicked => { root.clicked(); }
    }
}
```

### 5.2 Bluetooth Details Cards (Phase 2)

**Before:** Custom `Rectangle` with `VerticalLayout` + `DetailRow`
**After:** `OutlinedCard` + `Horizontal`/`Vertical` + `MaterialText`

```slint
import { OutlinedCard, MaterialText, Horizontal, Vertical } from "@material/material.slint";
import { Typography } from "@material/material.slint";
import { MaterialPalette } from "@material/material.slint";
import { MaterialStyleMetrics } from "@material/material.slint";

component DetailCard inherits OutlinedCard {
    in property <string> title;
    in property <string> icon;  // optional
    callback back_clicked();

    clickable: false;
    horizontal_stretch: 1;

    VerticalLayout {
        padding: MaterialStyleMetrics.padding_16;
        spacing: MaterialStyleMetrics.spacing_6;

        MaterialText {
            text: root.title;
            style: Typography.title_medium;  // 16px, medium
            color: MaterialPalette.primary;
        }

        @children  // DetailRow items inserted here
    }
}

component DetailRow inherits Horizontal {
    in property <string> label;
    in property <string> value;

    spacing: MaterialStyleMetrics.spacing_8;
    height: 24px;

    MaterialText {
        text: root.label;
        style: Typography.label_medium;  // 12px, medium
        color: MaterialPalette.on_surface_variant;
        font_weight: 800;
        width: 76px;
        vertical_alignment: center;
    }

    MaterialText {
        text: root.value;
        style: Typography.body_medium;  // 14px, regular
        color: MaterialPalette.on_surface;
        horizontal_stretch: 1;
        vertical_alignment: center;
        overflow: elide;
    }
}
```

### 5.3 Transport Buttons (Phase 5)

**Before:** Custom `Rectangle` (72×72 / 86×86) + `Image` + `TouchArea` + opacity animate
**After:** `IconButton` (prev/next) + `FilledButton` (play/pause, custom size)

```slint
import { IconButton, FilledButton, Icon } from "@material/material.slint";
import { MaterialPalette } from "@material/material.slint";
import { MaterialStyleMetrics } from "@material/material.slint";

// Previous / Next - IconButton at 72px
component TransportIconButton inherits IconButton {
    in property <image> icon;
    in property <bool> enabled: true;
    callback clicked();

    width: 72px;
    height: 72px;
    enabled: root.enabled;

    // IconButton uses Icon component internally; we override icon_size
    // But IconButton doesn't expose icon directly - may need custom
    // Alternative: use FilledButton with icon-only, or custom wrapper

    // For now, keep custom Rectangle for precise 72×72 + SVG icon
    // Material IconButton defaults to 48×48 with 24px icon
}
```

**Note:** `IconButton` is 48×48 fixed. For 72×72 transport buttons, we have two options:
1. **Wrap `FilledButton`** with `icon` property, set `width: 72px; height: 72px; border_radius: 36px`
2. **Keep custom** for transport row (hero button 86×86 is non-standard anyway)

**Recommendation:** Keep custom transport buttons (prev/play/next) — they're highly specialized with hero sizing, SVG icons, and press animations. Use Material buttons for settings, back buttons, dialog actions.

---

## 6. Rust-Side Integration

### 6.1 Project Setup (No Cargo.toml Dependency)

**Do NOT add a crate dependency.** Instead:

1. **Download and extract** `material-1.1.0.zip` from https://material.slint.dev/zip/material-1.1.0.zip into your project (e.g., `services/screen-manager/material-1.1.0/`)

2. **Verify structure:**
   ```
   services/screen-manager/
   ├── material-1.1.0/
   │   ├── material.slint          # Main entry point
   │   ├── ui/
   │   │   ├── components/         # 52 component .slint files
   │   │   ├── items/              # ListItem, NavigationItem, MenuItem
   │   │   └── styling/            # MaterialScheme, MaterialPalette, etc.
   │   └── LICENSE.md
   ├── ui/                         # Your existing .slint files
   ├── Cargo.toml
   └── build.rs
   ```

### 6.2 Build.rs Configuration

```rust
// services/screen-manager/build.rs
use std::path::PathBuf;

fn main() {
    // ... existing slint_build::compile() calls ...

    // Add Material Components as a component library
    let material_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("material-1.1.0/material.slint");

    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(std::collections::HashMap::from([
            ("material".to_string(), material_path),
        ]));

    slint_build::compile_with_config("ui/appwindow.slint", config).unwrap();
}
```

**Key points:**
- The library path key `"material"` enables imports like `import { FilledButton } from "@material/filled_button.slint";`
- All 52 components, items, and styling globals become available under `@material/`
- No Cargo feature flags needed — purely a build-time Slint compiler configuration

### 6.3 Theme Switching (Rust)

```rust
// services/screen-manager/src/theme.rs (new or modified)

use slint::ComponentHandle;

pub fn apply_theme_preset(window: &MainWindow, preset: u8) {
    // Update MaterialThemeBridge global
    let bridge = window.global::<MaterialThemeBridge>();
    bridge.set_active_preset(preset as i32);

    // Also update legacy Theme global for any remaining custom components
    let theme = window.global::<Theme>();
    theme.set_active_preset(preset as i32);
}
```

---

## 7. Risk Assessment & Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Material components don't compile with Slint 1.18.1 | Medium | High | Pin material-components to commit tested with 1.18.1; test in CI |
| Metric overrides not respected (cascade order) | Medium | Medium | Import overrides BEFORE `material.slint` in appwindow.slint |
| Custom SVG icons not colorizing via `Icon` | Low | Medium | `Icon` component uses `colorize: MaterialPalette.on_background` - test with our SVGs |
| Typography overrides break `MaterialText` layout | Low | Medium | Test all 13 text styles in SettingsView first |
| Performance regression on Pi 3 (extra component layers) | Low | High | Profile with `cargo llvm-cov` + framebuffer render loop timing |
| Theme preset mapping incomplete (5 presets × 40 tokens) | Medium | Medium | Automate: write script to generate schemes from current ThemePalette values |
| `NavigationBar`/`TabBar` accidentally used | Low | High | Lint: deny `NavigationBar`, `TabBar`, `NavigationDrawer` in clippy |

### 7.1 CI Gate Additions

```yaml
# .github/workflows/test-rpi3.yml (add to existing)
- name: Check Material component imports
  run: |
    grep -r "NavigationBar\|TabBar\|NavigationDrawer" services/screen-manager/ui/ && exit 1 || true
    grep -r "from \"@material/" services/screen-manager/ui/ | head -20

- name: Verify metric overrides load first
  run: |
    head -30 services/screen-manager/ui/appwindow.slint | grep -A5 -B5 "material_metrics_override"

- name: Verify material-1.1.0 exists
  run: |
    test -f services/screen-manager/material-1.1.0/material.slint || exit 1
```

---

## 8. Testing Strategy

### 8.1 Unit Tests (Host)
- [ ] `material_schemes_custom.slint` — verify all 5 presets parse, colors valid
- [ ] `material_metrics_override.slint` — verify all overridden metrics compile
- [ ] `material_typography_override.slint` — verify all 13 styles compile
- [ ] SettingsView — render with each preset, snapshot test

### 8.2 Integration Tests (Host)
- [ ] Theme switching: cycle through 5 presets, verify no crashes
- [ ] SettingsView → NowPlayingView navigation with Material buttons
- [ ] VolumeHUD overlay still works with Material theming active

### 8.3 Device Tests (Pi 3)
- [ ] Deploy release binary via `scripts/deploy_binaries.sh screen-manager`
- [ ] Visual verification: all 5 themes, all 6 views
- [ ] Touch response: buttons, cards, back navigation
- [ ] Performance: frame time < 16.65ms (60fps) on static UI, < 33ms during marquee
- [ ] Memory: RSS < 150MB

---

## 9. Rollback Plan

If Material adoption causes unrecoverable issues:

1. **Revert Cargo.toml** — remove `slint-material` dependency
2. **Delete override files** — `material_*.slint`
3. **Restore `theme.slint`** — `ThemePalette` + `Theme` global
4. **Revert component changes** — git checkout `settings_view.slint`, `viewport.slint`, etc.
5. **Verify** — `cargo build --target aarch64-unknown-linux-gnu --release --bins`

All changes are additive (new files) or localized (per-view) until Phase 6.

---

## 10. Open Questions

1. **Material-icons font vs. custom SVGs?** — Current: custom SVGs in `assets/icons/`. Material `Icon` component works with `@image-url()`. Decision: Keep custom SVGs.

2. **Version pinning strategy** — Material 1.1.0 ZIP is the official release for Slint 1.18.x. Track Slint releases for Material updates. Consider vendoring the ZIP in the repo for reproducible builds.

3. **Dynamic theme generation?** — Could write Rust build script to generate `material_schemes_custom.slint` from a JSON/YAML palette definition (compatible with Material Theme Builder export format), ensuring parity with legacy `ThemePalette`.

4. **Typography font family?** — Material `Typography` doesn't include font family. We keep `font_family: "Inter"` in a global or per-component.

5. **PeppyMeter skin integration?** — VU Meter tab uses full-screen background images. No Material equivalent. Keep as-is.

6. **JSON theme import?** — The gallery example uses `.json` theme files (Material Theme Builder export). Could we load presets from JSON at runtime instead of hardcoding in `.slint`?

---

## 11. Appendix: File-by-File Change Log (To Be Filled)

| File | Phase | Change Type | Status |
|------|-------|-------------|--------|
| `material-1.1.0/` (extracted ZIP) | 0 | Add vendor directory | ☐ |
| `build.rs` | 0 | Add library_paths config | ☐ |
| `ui/material_metrics_override.slint` | 0 | New file | ☐ |
| `ui/material_schemes_custom.slint` | 0 | New file | ☐ |
| `ui/material_typography_override.slint` | 0 | New file | ☐ |
| `ui/material_theme_bridge.slint` | 0 | New file | ☐ |
| `ui/appwindow.slint` | 0 | Import order + wiring | ☐ |
| `ui/settings_view.slint` | 1 | Full rewrite | ☐ |
| `ui/viewport.slint` | 2-3 | Extract BluetoothDetails, CD, DSP | ☐ |
| `ui/bluetooth_details.slint` | 2 | New file (Material cards) | ☐ |
| `ui/status_bar.slint` | 4 | Buttons → Material | ☐ |
| `ui/now_playing.slint` | 5 | Transport buttons decision | ☐ |
| `ui/theme.slint` | 6 | Delete | ☐ |
| `src/theme.rs` | 6 | Update theme switching | ☐ |

---

*Document version: 1.1*
*Last updated: 2026-11-07*
*Author: AI Agent (OpenCode)*

---

## Appendix: Official Material Components 1.1.0 Reference

### Distribution
- **ZIP URL**: https://material.slint.dev/zip/material-1.1.0.zip
- **Docs**: https://material.slint.dev/getting-started/
- **Source**: https://github.com/slint-ui/slint/tree/master/ui-libraries/material
- **Gallery Example**: https://github.com/slint-ui/slint/tree/master/ui-libraries/material/examples/gallery
- **Requires**: Slint 1.18+ (project uses 1.18.1 ✓)

### Integration Pattern (from official docs)
```rust
// build.rs
let config = slint_build::CompilerConfiguration::new()
    .with_library_paths(HashMap::from([(
        "material".to_string(),
        Path::new(&env!("CARGO_MANIFEST_DIR")).join("material-1.1.0/material.slint"),
    )]));
slint_build::compile_with_config("ui/main.slint", config).unwrap();
```

```slint
// In your .slint files
import { FilledButton, ElevatedCard, Icon } from "@material/material.slint";
import { MaterialPalette, MaterialStyleMetrics, Typography, Animations } from "@material/material.slint";
```

### Theming via JSON (Material Theme Builder)
The gallery example demonstrates loading themes from JSON files exported by [Material Theme Builder](https://m3.material.io/theme-builder):
- `material_slint_theme.json`, `material_green_theme.json`, etc.
- Contains `schemes.light`, `schemes.dark`, `palettes.*`
- Could be used for runtime theme switching instead of hardcoded `.slint` schemes
---

## Completion: Theme Migration P0–P5 (2026-11-08)

Single source of truth: `ThemePreset` (Rust, 5 presets) → `MaterialScheme` (dark-only) → `MaterialPalette.schemes`; all views read `MaterialPalette.*`. `AppTheme.active_preset` is the single preset index (Slint skin switch + Rust). Legacy `Theme`/`ThemePalette`/`theme.slint` deleted.

### Token map (legacy → Material)
| Legacy | Material |
|---|---|
| `chassis` | `background` |
| `card_surface` | `surface` |
| `card_border` | `outline_variant` |
| `primary_accent` | `primary` |
| `secondary_accent` | `secondary` |
| `text_primary` | `on_surface` |
| `text_muted` | `on_surface_variant` |
| `needle_peak` | `error` |
| `dock_active_text` | `on_primary` |
| hero play circle | `inverse_surface` / `background` |

### Derivation (`ThemePreset::material_scheme_dark`)
`primary`/`secondary`/`chassis`/`card_surface`/`card_border`/`text_primary`/`text_muted`/`needle_peak`/`dock_active_text` map directly; containers (`surface_container_*`, `*_container`) and `surface_dim`/`surface_bright` are weighted averages of base colors. `PRESET_SWATCHES` pins Settings swatch hexes to the same source.

### Vendor patches (`material-1.1.0/`, see THEME_MIGRATION_PLAN.md)
- `material_palette.slint`: `schemes` `in`→`in-out`; added `color-scheme <=> Palette.color-scheme` (Rust forces Dark)
- `material_typography.slint` / `material_style_metrics.slint`: `out`→`in-out`, compact 800×480 defaults (from former `material_*_override.slint`, now deleted); added `border_radius_10`

### Coverage gate deviation
Plan P5 gated ≥90% lines/functions; workspace is 89.16%/91.67% (repo gate 85% passes). The shortfall is in unrelated crates (bluetooth/hardware) outside this migration's scope; `theme.rs` itself is 99.78%/100%.

### Verification
Host framebuffer probes: all 5 presets render distinct primaries on Settings; all Material tabs recolor on preset switch. Device: aarch64 release deployed, `screen-manager` active.

### Known pitfall: stale Slint build cache masks `.slint` edits
`slint-build` generated code lives under `target/<triple>/release/build/screen-manager-*/out`; incremental
`cargo build` can ship a binary compiled from the **previous** `.slint` contents when only `.slint` files
change (observed twice: deployed binaries silently kept the old UI while sources were already updated).
Before deploying or debugging UI changes, force regeneration:

```bash
cargo clean -p screen-manager
cargo build -p screen-manager --target aarch64-unknown-linux-gnu --release
```

A quick sanity check: run a clean `-p screen-manager` build and compare the binary sha256 against the
cached one — if the clean rebuild produces a *different* hash, the cache was stale. Also note a failing
`.slint` compile in a cached build dir only surfaces on `cargo clean`, so always build clean after UI edits.
