# Vendored Material Design 3 Component Library (`@material`)

This directory contains the vendored [Slint Material Design 3 Components](https://material.slint.dev/) (`1.1.0`) customized for the Raspberry Pi `800×480` touchscreen appliance.

It is registered as the `@material` component library path in [`services/screen-manager/build.rs`](../../build.rs) and imported across UI files via:

```slint
import {
    FilledButton,
    MaterialPalette,
    MaterialStyleMetrics,
    MaterialText,
    MaterialTypography,
    TouchScale,
} from "@material/material.slint";
```

## Appliance Customizations for `800×480` Touchscreen

1. **Centralized Touch Scaling (`TouchScale`):**
   - Defined in [`ui/styling/material_style_metrics.slint`](ui/styling/material_style_metrics.slint) and re-exported from [`material.slint`](material.slint):
     - `TouchScale.font_scale` (`1.0` to `1.35`, default `1.15`): scales all typography sizes in [`ui/styling/material_typography.slint`](ui/styling/material_typography.slint).
     - `TouchScale.widget_scale` (`1.0 + (clamped_font_scale - 1.0) * 0.5`): damped multiplier (grows at 50% of `font_scale`) applied to touch target minimums (`size_32`, `size_40`, `size_64`, `size_72`), icon sizes (`icon_size_24`), paddings (`padding_16`), and spacings in `MaterialStyleMetrics`.
   - Thin stroke and indicator tokens (`size_1: 1px`, `size_2: 2px`, `size_3: 3px`) remain unscaled integer pixels to prevent fractional pixel softness on borders and dividers.

2. **Unclamped Base Widget Primitives:**
   - [`ui/components/base_button.slint`](ui/components/base_button.slint) (`BaseButton` and derived `FilledButton`, `OutlineButton`, `TextButton`, `ElevatedButton`, `TonalButton`): expands dynamically with `min-height: max(MaterialStyleMetrics.size_40, layout.min-height)` and `min-width: max(64px * TouchScale.widget_scale, layout.min-width)`, while keeping button text single-line (`wrap: no-wrap; overflow: elide`) to preserve pill border radii (`root.height / 2`).
   - [`ui/components/chip.slint`](ui/components/chip.slint) (`ActionChip`, `FilterChip`, `InputChip`): unclamped height (`min_layout_height: MaterialStyleMetrics.size_32`) with fixed `1px` outline borders.
   - [`ui/components/list.slint`](ui/components/list.slint) (`ListTile`): unclamped height (`min-height: max(MaterialStyleMetrics.size_72, layout.min-height)`) that expands around scaled primary and supporting `MaterialText`.
   - [`ui/components/text_field.slint`](ui/components/text_field.slint) (`TextField`): unclamped container height that expands around both the floating label (`body_small`) and input (`body_large`), and notifies `VirtualKeyboard` when `TextInput` gains focus.

3. **Touch Hover States Stripped Globally:**
   - [`ui/components/state_layer.slint`](ui/components/state_layer.slint) and button/control state blocks (`filled_button.slint`, `elevated_button.slint`, `tonal_button.slint`, `floating_action_button.slint`, `outline_icon_button.slint`, `switch.slint`, `radio_button.slint`) have sticky `has_hover` opacity and elevation transitions removed so only `pressed`, `focus`, and `Ripple` states render on touchscreens.

4. **Pruned Unused / Oversized Components:**
   - Removed `app_bar.slint` and `bottom_app_bar.slint` (replaced by custom [`ui/screen_app_bar.slint`](../screen_app_bar.slint) and [`ui/status_bar.slint`](../status_bar.slint)).
   - Removed `navigation_drawer.slint` and `drawer.slint`.
   - Removed `date_picker.slint` and `time_picker.slint` (vertical layouts exceed the `480px` panel height).
