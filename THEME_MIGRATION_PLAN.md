# Theme Migration Plan: P0–P5 (Single Source of Truth via MaterialPalette)

**Goal:** One preset system. `ThemePreset` (Rust, 5 presets) → 50-field `MaterialScheme` (dark-only) → `MaterialPalette.schemes` + marquee rasterizer → all views read `MaterialPalette.*`. `Theme` / `ThemePalette` deleted at the end.

---

## P0 — Spike + force dark mode ✅ DONE (2026-11-08)
**Deliverable:** Settings/CD/DSP recolor to active preset; status bar / Now Playing unchanged.

Verified: host probe + device deploy (SHA `e2b140da…`); all Material tabs render McIntosh cyan;
Now Playing screenshot unchanged. Dark forced via `MaterialPalette.set_color_scheme(Dark)` in `apply()`.

### Vendor patches (material-1.1.0/)
1. `ui/styling/material_palette.slint:67` — `in property <MaterialSchemes> schemes` → `in-out property <MaterialSchemes> schemes`
2. `ui/styling/material_palette.slint:8` — add `in-out property <ColorScheme> color-scheme <=> Palette.color-scheme;`

### App changes
- `appwindow.slint`: `export { MaterialPalette }`
- `theme.rs`: `material_schemes(self) -> MaterialSchemes` for all 5 presets (dark==light)
- `theme.rs`: `apply()` pushes schemes via `MaterialPalette.set_schemes()`
- `theme.rs`: `apply_splash()` keeps inline ThemePalette (splash still uses legacy Theme during transition)

### Gates
- `cargo build --features ui` (host)
- `cargo build --target aarch64-unknown-linux-gnu --release --bins`
- `scripts/deploy_binaries.sh screen-manager`
- `systemctl restart screen-manager`
- Screenshot `/dev/fb0` → verify Settings/CD/DSP use McIntosh cyan; status bar/Now Playing unchanged

---

## P1 — Complete single source of truth (Rust) ✅ DONE (2026-11-08)
**Deliverable:** All 5 presets distinct in Material scheme; marquee strips recolor on theme switch; theme picker callbacks refresh strips.

Verified: unit tests `material_schemes_dark_equals_light`, `material_schemes_presets_are_distinct`,
`material_schemes_core_tokens_match_legacy_palette`; callbacks funnel through `preset.apply()` +
`refresh_marquee_strips_for()`; `material_theme_bridge.slint` deleted; commented pseudo-presets removed.
Host probe: Settings renders all 5 preset primaries (4683 px each).

### Rust scheme builder (theme.rs)
- `material_scheme_dark()` for all 5 presets with unit tests asserting every field non-default and preset-distinct
- Derivation rules (legacy → Material):
  | Legacy | Material tokens | Formula |
  |---|---|---|
  | `primary_accent` | `primary`, `surfaceTint`, `primaryFixed`, `primaryFixedDim` | = `primary_accent` |
  | `secondary_accent` | `secondary`, `secondaryFixed`, `secondaryFixedDim` | = `secondary_accent` |
  | `chassis` | `background`, `surfaceDim`, `surfaceContainerLowest` | = `chassis` |
  | `card_surface` | `surface`, `surfaceBright`, `surfaceContainerLow` | = `card_surface` |
  | `card_border` | `outline`, `outlineVariant`, `surfaceVariant`, `surfaceContainerHigh`, `surfaceContainerHighest` | = `card_border` |
  | `text_primary` | `onBackground`, `onSurface` | = `text_primary` |
  | `text_muted` | `onSurfaceVariant` | = `text_muted` |
  | `needle_peak` | `error`, `tertiary` | = `needle_peak` |
  | `dock_active_text` | `onPrimary`, `onSecondary`, `onTertiary`, `onError` | = `dock_active_text` |
  | `inverse_surface` / `inverse_on_surface` / `inversePrimary` | fixed cross-preset | `#F0F6FC` / `#090D12` / `#006E99` (McIntosh-derived) |
- Containers/fixed variants via `darken(0.12)` / `lighten(0.12)` helpers in Rust

### Marquee rasterizer
- `refresh_marquee_strips()` reads color from `material_schemes().dark.onBackground` (≡ old `text_primary`)

### Single funnel
- New `apply_material_theme(preset)` that (a) pushes `MaterialPalette.schemes`, (b) sets `active_preset`, (c) calls `refresh_marquee_strips()`
- Replace `on_select_theme` / `on_cycle_theme` callbacks in `shell.rs` to delegate to `apply_material_theme`

### Cleanup
- Delete `material_theme_bridge.slint`
- Delete commented pseudo-presets in `material_schemes_custom.slint` (file stays for P5)

### Gates
- fmt, clippy `-D warnings`
- Unit tests for `material_scheme()` + full test suite
- Screenshot per preset

---

## P2 — Low-risk views (token substitution) (CURRENT)
**Deliverable:** Status bar / volume HUD / splash / viewport / settings visually identical before/after under all 5 presets.

### Token map (Theme.active.* → MaterialPalette.*)
| Legacy | → Material |
|---|---|
| `chassis` | `background` |
| `card_surface` | `surface` / `surface_container_high` |
| `card_border` | `outline_variant` / `surface_variant` |
| `primary_accent` | `primary` |
| `secondary_accent` | `secondary` |
| `text_primary` | `on_surface` / `on_background` |
| `text_muted` | `on_surface_variant` |
| `needle_peak` | `error` |
| `dock_active_text` | `on_primary` |

### Volume HUD hardcoded colors
- `#EF4444` (muted red, 3×) → `error` (derives from `needle_peak`, now preset-aware)
- `#1c2430` (seek track) → `surface_container_highest`
- `#FFFFFF` (muted icon/text) → `on_error`

### Settings view
- 10 swatch hexes (lines 224–270) stay literal (must show each preset's own colors)
- Add Rust constant table `PRESET_SWATCHES` used by both `theme.rs` and view to prevent drift

### Viewport skin selection
- Add `global AppTheme { in-out property <int> active_preset }` (own file, no vendor)
- Both Slint skin switch and Rust `current_theme()`/`cycle_theme()` read/write this

### Gates
- Per-preset screenshot verification

---

## P3 — Now Playing (folded into P2)
Same token map as P2 plus:
- Hero play circle: `background_color: inverse_surface` (≈ white), `icon_color: background` (≈ black) — **preserves M3 white circle across all presets**
- Seek track `#1c2430` → `surface_container_highest`
- Wave bars: `primary` / `secondary` (follows preset hue — intended)

---

## P4 — Typography & metrics tokens
**Deliverable:** Layout stable; golden input test passes; no overflow/clip regressions.

### Vendor patches (only tokens we use)
| File | Tokens patched `out` → `in-out` |
|---|---|
| `material_typography.slint` | `regular`/`medium`/`semibold`, all `display_*`/`headline_*`/`title_*`/`label_*`/`body_*` |
| `material_style_metrics.slint` | `icon_size_18/24/36/90`, `size_48/56`, `spacing_2–52`, `padding_4–56`, `border_radius_*`, `size_14–572` |

- Set compact defaults (mirror current `material_*_override.slint`)
- Delete `material_typography_override.slint` and `material_metrics_override.slint` (vendor globals now carry overrides)

### Gates
- Full test suite
- Screenshot all 5 presets
- Layout sanity at 800×480
- Update golden input test coordinates (`ui_input_integration.rs`)

---

## P5 — Cleanup & final gates
**Deliverable:** Zero `Theme`/`ThemePalette` references; all gates pass; 5 preset screenshots archived.

### Deletions
- `theme.slint`, `ThemePalette`, `Theme` global
- `CustomMaterialSchemes`, `material_schemes_custom.slint`
- `material_theme_bridge.slint`
- `material_typography_override.slint`, `material_metrics_override.slint`

### New global
- `AppTheme` (active_preset + future flags) exported from `appwindow.slint`

### Rust public API
- Remove `Theme`, `ThemePalette` from `mod.rs` re-exports
- Keep `ThemePreset` (config slug, `material_schemes()`, `meter_skin_id()`, `font_family()`)
- Rewrite `ThemePreset::palette()` → `material_schemes()` or delete
- Update tests: `ui_theme_integration.rs`, `ui_shell_integration.rs`, `daemon_integration.rs`, `config_integration.rs`

### Splash screen
- `init_splash_screen()` creates `SplashScreen` **after** `MaterialPalette.schemes` pushed
- Splash imports `@material` and reads `MaterialPalette.*` like other views
- Remove `apply_splash()`

### Repo hygiene
- Add `lcov.info` to `.gitignore`
- Delete `material-1.1.0.zip` (extracted dir stays committed)
- Update `docs/slint-material-migration.md` (phases, token map, vendor patch list, derivation formulas)

### Gates (all must pass)
1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. Workspace tests
4. `cargo llvm-cov --target x86_64-unknown-linux-gnu --fail-under-lines 90 --fail-under-functions 90`
5. `cargo build --target aarch64-unknown-linux-gnu --release --bins`
6. `scripts/deploy_binaries.sh screen-manager`
7. `systemctl restart screen-manager`
8. **Screenshot all 5 presets** via `/dev/fb0`

---

## Vendor patch log (for future upgrades)
| File | Change |
|---|---|
| `material_palette.slint:67` | `in property` → `in-out property` for `schemes` |
| `material_palette.slint:8` | add `in-out property <ColorScheme> color-scheme <=> Palette.color-scheme` |
| `material_typography.slint:*` | `out property` → `in-out property` for used weights + all `TextStyle` tokens |
| `material_style_metrics.slint:*` | `out property` → `in-out property` for used sizes/spacings/paddings/radii |

---

## Risk register
| Risk | Phase | Mitigation |
|---|---|---|
| Vendor patch drift on 1.1.0 updates | P0/P4 | Patch list in docs + tests; vendor dir committed |
| `Palette.color_scheme` not forced dark before first paint | P0 | Set on `MainWindow` root via `MaterialPalette.color-scheme <=> Palette.color-scheme` |
| Marquee strip color stale after P1 if rasterizer not updated | P1 | Unit test: push new scheme, assert `refresh_marquee_strips` reads new color |
| Layout break at 800×480 from compact metrics | P4 | Per-token gate + golden input test + per-preset screenshot |
| Public API break `ThemePreset::palette()` | P5 | Deprecate with `#[deprecated]` first if needed; rewrite tests to `material_schemes()` |