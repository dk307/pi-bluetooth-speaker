# Material 3 App Bars — Three-Kind Architecture (rev 5, implementation-ready)

## Executive Summary

Three distinct app bar kinds govern the UI:

1. **Now Playing (kind: home)** — *unchanged*. Existing `TopStatusBar` stays exactly as it is; no M3 app bar on this screen.
2. **Screens (kind: screen)** — tabs 1–5 (VU Meter, Bluetooth, CD, DSP, Settings/Themes) take the **full 800×480 area** (TopStatusBar hidden via `visible`), each topped by a stock M3 `SmallAppBar` (60px) with title + **back** leading button. Back is the only way out.
3. **Overlay (kind: overlay)** — Volume HUD renders full-viewport with its own modal app bar (title "Volume" + **close**). The underlying screen and its app bar remain visible but **dimmed** (simulated blur via opacity + scrim).

---

## Kind Matrix

| Context | App bar kind | Layout | Title | Leading | Trailing |
|---------|-------------|--------|-------|---------|----------|
| Now Playing (tab 0) | *(none — TopStatusBar unchanged)* | status bar 60px + viewport 420px | — | — | — |
| VU Meter (tab 1) | Screen | full 800×480, own app bar | "VU Meter" | back | none |
| Bluetooth (tab 2) | Screen | full screen | "Bluetooth" | back | none |
| CD Player (tab 3) | Screen | full screen | "CD Player" | back | none |
| DSP & Tone (tab 4) | Screen | full screen | "DSP & Tone" | back | none |
| Settings / Themes (tab 5) | Screen | full screen | "Settings" / "Themes" | back | none |
| Volume HUD (overlay) | Overlay (modal) | full screen, above dimmed background | "Volume" | close (X) | none |

Notes:
- Status chips (device/codec/volume/DIM) live **only** on the Now Playing status bar. Entry to Bluetooth details is via status bar taps (existing) or Settings grid — no chip duplication.
- `show_themes` is lifted to `MainWindow` via `<=>` bindings through `MainViewport` → title switches "Settings" ↔ "Themes".

---

## Verified Library Facts (drives all decisions below)

- `build.rs` maps `@material` → `material-1.1.0/` (entry `material.slint`). Sub-path imports (`@material/ui/components/app_bar.slint`) work if ever needed.
- `material.slint` **exports** `AppBar, SmallAppBar, MediumAppBar, LargeAppBar`. It does **NOT** re-export `BaseAppBar` (app_bar.slint) or `IconButtonItem` (bottom_app_bar.slint).
- **Patched metrics** (`material_style_metrics.slint`): `size_64: 56px` → `SmallAppBar.min_height = max(56px, base.min_height)` = **56px**. A 60px wrapper does **not** clip → **`SmallAppBar` is used directly; no custom CompactAppBar needed.**
- No shadows in the library; `show_background` toggles `surface_container`/`surface`.
- Icons are SVG `image` sources only (no icon fonts).
- Slint scoping rule: properties of elements inside an `if` conditional are **not addressable from outside** — two-way bindings (`<=>`) to conditional elements fail to compile. Therefore `TopStatusBar` and `MainViewport` stay **permanently mounted** and are gated with `visible:` only.

---

## Overlay Dim Behavior (Volume HUD)

**Trigger:** volume icon tap on Now Playing status bar → HUD opens above Now Playing.

**Visual contract:**
- **Whole background column** (status bar + active tab content, including its app bar) animates `opacity` 1.0 → 0.35 with a 55% dark scrim — reads as dimmed/defocused while staying visible.
- HUD content (overlay app bar "Volume", giant % readout, slider, mute button) renders opaque above it.
- Backdrop-tap-to-dismiss **kept** alongside the X button.

**Slint constraint (honest):** Slint 1.9 has **no backdrop/gaussian blur**. Simulated:
1. `bg_dim_opacity` animated 1.0 → 0.35 (one-shot 200ms `animate opacity` — allowed per M-NO-PER-FRAME-ITEM-TREE-CHURN: state transition, not per-frame restart).
2. Scrim `Rectangle { background: #000000; opacity: bg_dim_opacity * 0.55 }` between background and HUD. (`MaterialPalette.scrim` does not exist — plain `#000000` or `MaterialPalette.shadow`.)
3. All layers permanently mounted, `visible`/`opacity` gated — zero item-tree churn.

---

## MainWindow Layout (rev 5 — no conditionals on bound elements)

```slint
export component MainWindow inherits Window {
    // ... existing properties ...
    in-out property <bool> show_themes: false;                    // NEW (lifted)
    out property <float> bg_dim_opacity: root.volume_hud_visible ? 0.35 : 1.0;
    animate bg_dim_opacity { duration: 200ms; easing: ease-out; } // one-shot

    public function app_bar_back() {
        if (root.active_tab == 2) { root.toggle_bluetooth_details(); }
        else if (root.active_tab == 1) { root.active_tab = root.previous_tab == 1 ? 0 : root.previous_tab; }
        else if (root.active_tab == 3) { root.active_tab = root.previous_tab == 3 ? 0 : root.previous_tab; }
        else if (root.active_tab == 4) { root.active_tab = root.previous_tab == 4 ? 0 : root.previous_tab; }
        else if (root.active_tab == 5) {
            if (root.show_themes) { root.show_themes = false; }
            else { root.active_tab = root.previous_tab == 5 ? 0 : root.previous_tab; }
        }
    }

    // ONE background column carries the dim (covers home AND screens)
    bg_column := VerticalLayout {
        opacity: root.bg_dim_opacity;

        // TopStatusBar: ALWAYS MOUNTED (two-way bindings preserved), visible-gated
        status_bar := TopStatusBar {
            visible: root.active_tab == 0 && !root.volume_hud_visible;
            // ... all existing property forwards & callbacks unchanged ...
        }

        // Screen app bar: ALWAYS MOUNTED, visible-gated (no `if`!)
        screen_app_bar := ScreenAppBar {
            visible: root.active_tab != 0;
            title: root.active_tab == 1 ? "VU Meter" :
                   root.active_tab == 2 ? "Bluetooth" :
                   root.active_tab == 3 ? "CD Player" :
                   root.active_tab == 4 ? "DSP & Tone" :
                   root.active_tab == 5 ? (root.show_themes ? "Themes" : "Settings") : "";
            leading_icon: @image-url("../../../assets/icons/back.svg");
            leading_clicked => { root.app_bar_back(); }
        }

        // MainViewport: ALWAYS MOUNTED (all `<=>` bindings intact)
        viewport := MainViewport {
            active_tab <=> root.active_tab;
            // ... all existing forwards unchanged ...
            show_themes <=> root.show_themes;   // NEW lift
            visible: !root.volume_hud_visible;  // optional; dim already covers it
        }
    }

    // Scrim + HUD as siblings ABOVE the background column
    hud_scrim := Rectangle {
        background: #000000;
        opacity: root.volume_hud_opacity * 0.55;
        visible: root.volume_hud_opacity > 0.01;
    }
    volume_hud := VolumeHudOverlay {
        visible: root.volume_hud_opacity > 0.01;
        opacity: root.volume_hud_opacity;
        // ... existing binds ...
    }
}
```

> Note: when the HUD is open over Now Playing, the status bar hides (`visible: ... && !volume_hud_visible` in the current design) — the dim still applies to the visible tab content. If the HUD should later open over screen tabs too, the dim already covers them uniformly.

**Height budget:** tab 0 → 60 (status) + 420 (viewport) = 480. Tabs 1–5 → 60 (app bar) + 420 = 480. No regression; Now Playing untouched.

---

## `ScreenAppBar` (`ui/screen_app_bar.slint`, NEW — only new file)

Wraps the **stock** `SmallAppBar` (min 56px ≤ 60px wrapper — no clipping). Plain `image` leading (no `IconButtonItem` import needed; the struct literal below resolves against `SmallAppBar`'s `leading_button` property type):

```slint
import { SmallAppBar } from "@material/material.slint";

export component ScreenAppBar inherits Rectangle {
    height: 60px;
    in property <string> title;
    in property <image> leading_icon;        // back.svg
    in property <string> leading_tooltip: "Back";
    callback leading_clicked();

    SmallAppBar {
        title: root.title;
        show_background: true;               // surface_container
        leading_button: { icon: root.leading_icon, tooltip: root.leading_tooltip, enabled: true };
        leading_button_clicked => { root.leading_clicked(); }
    }
}
```

## `show_themes` Lift Path

```
SettingsView (in-out show_themes)
    ⇄  MainViewport (in-out show_themes: false, forwards <=> to SettingsView)
        ⇄  MainWindow (in-out show_themes: false, binds <=> viewport.show_themes)
```

## Volume HUD Overlay App Bar

Inside `VolumeHudOverlay`, add a header row **above** the existing content — same stock `SmallAppBar` at 60px (title "Volume", leading `close.svg` → `dismiss_requested()`), then the existing body (readout / slider / mute row). Backdrop-tap dismiss kept.

---

## New Assets (DONE)

- `assets/icons/back.svg` — official Material Symbols outlined `arrow_back` (24px, Apache 2.0).
- `assets/icons/close.svg` — official Material Symbols outlined `close` (24px, Apache 2.0).
- `left.svg` inspected and rejected — gradient chevron, not a nav arrow.

Library zip audit: stock `material-1.1.0` from `C:\Users\Deepak\Downloads\material-1.1.0.zip` matches vendored copy except 3 styling files — those are **intentional local patches** for the 800×480 panel (compact scale, `in-out` overrides, `border_radius_10`, color-scheme binding). Stock files would regress the panel; **vendored patched library retained.**

---

## Migration Checklist (implementation-ready)

| Task | File(s) |
|------|---------|
| Create `ScreenAppBar` (wraps stock `SmallAppBar`, 60px, plain `image` leading) | `ui/screen_app_bar.slint` (NEW — only new file) |
| Add `in-out property <bool> show_themes: false` to `MainViewport`, forward `<=>` to `SettingsView` | `viewport.slint` |
| Add `in-out property <bool> show_themes: false` + `bg_dim_opacity` animated + `app_bar_back()` router to `MainWindow` | `appwindow.slint` |
| Restructure `MainWindow`: one always-mounted `bg_column` (status bar + `ScreenAppBar` + `MainViewport`, all `visible`-gated — **no `if` conditionals on bound elements**), scrim + HUD as siblings above | `appwindow.slint` |
| Remove Tab 1–4 in-body headers | `viewport.slint` |
| Remove SettingsView header + `BackButton` component | `settings_view.slint` |
| Volume HUD: add stock `SmallAppBar` header row (title "Volume", leading close.svg → dismiss); keep backdrop dismiss | `volume_hud.slint` |
| Purge slint-build caches after `.slint` edits (mandatory, AGENTS.md §2.12) | `rm -rf target/<triple>/{debug,release}/build/screen-manager-* target/<triple>/{debug,release}/.fingerprint/screen-manager-*` |
| Gates: fmt → clippy `-D warnings` → llvm-cov ≥90/90 → aarch64 release build | CI sequence |

## Out of Scope

- Real gaussian backdrop blur (Slint 1.9 unsupported; simulated dim+scrim).
- Elevation shadows, app bar transition animations.
- Marquee/compose internals — untouched.