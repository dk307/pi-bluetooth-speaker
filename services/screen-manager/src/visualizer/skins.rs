//! `PeppyMeter` `800×480` skin parser (`meters.txt`) and embedded asset loader.

use std::collections::HashMap;

/// Embedded `meters.txt` skin definitions from `assets/skins/800x480/meters.txt`.
pub const METERS_TXT: &str = include_str!("../../../../assets/skins/800x480/meters.txt");

/// Default VU skin name when unconfigured.
pub const DEFAULT_VU_SKIN: &str = "vintage";

/// Linear meter sweep direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinearDirection {
    /// Left to right (default).
    #[default]
    LeftRight,
    /// Bottom to top (`vertical-linear`).
    BottomTop,
    /// Top to bottom (`steam-punk`).
    TopBottom,
    /// Center outward to edges (`relax`).
    CenterEdges,
    /// Edges inward to center (`fantasy`).
    EdgesCenter,
}

/// Parsed circular needle geometry (angles in Slint clockwise-positive degrees).
#[derive(Debug, Clone, PartialEq)]
pub struct CircularGeometry {
    /// Distance from needle center to pivot point in pixels.
    pub distance: f32,
    /// Left/mono channel start angle in Slint degrees (`-peppy_start_angle`).
    pub left_start_deg: f32,
    /// Left/mono channel stop angle in Slint degrees (`-peppy_stop_angle`).
    pub left_stop_deg: f32,
    /// Right channel start angle in Slint degrees.
    pub right_start_deg: f32,
    /// Right channel stop angle in Slint degrees.
    pub right_stop_deg: f32,
    /// Left (or mono) pivot X in `800×480` coordinates.
    pub left_origin_x: f32,
    /// Left (or mono) pivot Y in `800×480` coordinates.
    pub left_origin_y: f32,
    /// Right pivot X in `800×480` coordinates.
    pub right_origin_x: f32,
    /// Right pivot Y in `800×480` coordinates.
    pub right_origin_y: f32,
}

/// Parsed linear bar/indicator geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct LinearGeometry {
    /// Sweep direction.
    pub direction: LinearDirection,
    /// `true` when `indicator.type = single` (continuous clipped bar, e.g. `chillout`).
    pub single_indicator: bool,
    /// `true` when left channel is horizontally flipped (`relax`).
    pub flip_left_x: bool,
    /// `true` when right channel is horizontally flipped (`steam-punk`, `fantasy`).
    pub flip_right_x: bool,
    /// Left channel origin X.
    pub left_x: f32,
    /// Left channel origin Y.
    pub left_y: f32,
    /// Right channel origin X.
    pub right_x: f32,
    /// Right channel origin Y.
    pub right_y: f32,
    /// Number of regular segments (or pixels/steps for single indicator).
    pub position_regular: u32,
    /// Number of overload segments.
    pub position_overload: u32,
    /// Step width in pixels for regular segments.
    pub step_width_regular: f32,
    /// Step width in pixels for overload segments.
    pub step_width_overload: f32,
}

/// Skin-type-specific geometry.
#[derive(Debug, Clone, PartialEq)]
pub enum SkinGeometry {
    /// Circular rotating needle meter.
    Circular(CircularGeometry),
    /// Linear segmented or sliding indicator meter.
    Linear(LinearGeometry),
}

/// Complete parsed `PeppyMeter` skin descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct VuSkin {
    /// Section name from `meters.txt` (e.g. `"vintage"`, `"blue"`, `"bar"`).
    pub name: String,
    /// Number of meter channels (`1` for mono, `2` for stereo).
    pub channels: u8,
    /// Background image filename (`*-bgr.png` or `*.jpg`).
    pub bgr_filename: String,
    /// Optional foreground overlay filename (`*-fgr.png`).
    pub fgr_filename: Option<String>,
    /// Needle or indicator image filename.
    pub indicator_filename: String,
    /// Circular or linear geometry.
    pub geometry: SkinGeometry,
}

impl VuSkin {
    /// Returns `true` if this is a circular needle skin.
    #[must_use]
    pub const fn is_circular(&self) -> bool {
        matches!(self.geometry, SkinGeometry::Circular(_))
    }

    /// Computes left and right needle angles in Slint degrees for `0.0..=1.0` ratios.
    #[must_use]
    pub fn circular_angles(&self, left_ratio: f32, right_ratio: f32) -> (f32, f32) {
        let SkinGeometry::Circular(ref c) = self.geometry else {
            return (0.0, 0.0);
        };
        let l = left_ratio.clamp(0.0, 1.0);
        let r = if self.channels <= 1 {
            l
        } else {
            right_ratio.clamp(0.0, 1.0)
        };
        let left_deg = c.left_start_deg + (c.left_stop_deg - c.left_start_deg) * l;
        let right_deg = c.right_start_deg + (c.right_stop_deg - c.right_start_deg) * r;
        (left_deg, right_deg)
    }
}

/// Parses all skins from a `meters.txt` string, preserving section order.
#[must_use]
pub fn parse_meters_txt(input: &str) -> Vec<VuSkin> {
    let mut sections: Vec<(String, HashMap<String, String>)> = Vec::new();
    for raw_line in input.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && line.len() > 2 {
            let name = line[1..line.len() - 1].trim().to_string();
            sections.push((name, HashMap::new()));
            continue;
        }
        if let Some((key, val)) = line.split_once('=')
            && let Some((_, map)) = sections.last_mut()
        {
            map.insert(key.trim().to_string(), val.trim().to_string());
        }
    }

    sections
        .into_iter()
        .filter_map(|(name, props)| build_skin(name, &props))
        .collect()
}

/// Parses the embedded `800×480` `meters.txt` registry.
#[must_use]
pub fn all_skins() -> Vec<VuSkin> {
    parse_meters_txt(METERS_TXT)
}

/// Finds a skin by name, falling back to [`DEFAULT_VU_SKIN`] or the first skin.
#[must_use]
pub fn find_skin<'a>(skins: &'a [VuSkin], name: &str) -> Option<&'a VuSkin> {
    skins
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .or_else(|| skins.iter().find(|s| s.name == DEFAULT_VU_SKIN))
        .or_else(|| skins.first())
}

fn parse_f32(props: &HashMap<String, String>, key: &str, default: f32) -> f32 {
    props
        .get(key)
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
}

fn parse_u32(props: &HashMap<String, String>, key: &str, default: u32) -> u32 {
    props
        .get(key)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

fn parse_bool(props: &HashMap<String, String>, key: &str) -> bool {
    props
        .get(key)
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

fn non_empty(props: &HashMap<String, String>, key: &str) -> Option<String> {
    props.get(key).filter(|v| !v.is_empty()).cloned()
}

fn build_skin(name: String, props: &HashMap<String, String>) -> Option<VuSkin> {
    let meter_type = props.get("meter.type")?.as_str();
    let channels = props
        .get("channels")
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(2)
        .clamp(1, 2);
    let bgr_filename = non_empty(props, "bgr.filename")?;
    let fgr_filename = non_empty(props, "fgr.filename");
    let indicator_filename = non_empty(props, "indicator.filename")?;

    let geometry = match meter_type {
        "circular" => {
            let shared_start = parse_f32(props, "start.angle", 45.0);
            let shared_stop = parse_f32(props, "stop.angle", -45.0);
            // PeppyMeter uses counter-clockwise positive angles; Slint uses clockwise positive.
            let left_start_deg = -parse_f32(props, "left.start.angle", shared_start);
            let left_stop_deg = -parse_f32(props, "left.stop.angle", shared_stop);
            let right_start_deg = -parse_f32(props, "right.start.angle", shared_start);
            let right_stop_deg = -parse_f32(props, "right.stop.angle", shared_stop);

            let mono_x = parse_f32(props, "mono.origin.x", 400.0);
            let mono_y = parse_f32(props, "mono.origin.y", 300.0);
            let left_origin_x = parse_f32(props, "left.origin.x", mono_x);
            let left_origin_y = parse_f32(props, "left.origin.y", mono_y);
            let right_origin_x = parse_f32(props, "right.origin.x", mono_x);
            let right_origin_y = parse_f32(props, "right.origin.y", mono_y);

            SkinGeometry::Circular(CircularGeometry {
                distance: parse_f32(props, "distance", 0.0),
                left_start_deg,
                left_stop_deg,
                right_start_deg,
                right_stop_deg,
                left_origin_x,
                left_origin_y,
                right_origin_x,
                right_origin_y,
            })
        }
        "linear" => {
            let direction = match props.get("direction").map(String::as_str) {
                Some("bottom-top") => LinearDirection::BottomTop,
                Some("top-bottom") => LinearDirection::TopBottom,
                Some("center-edges") => LinearDirection::CenterEdges,
                Some("edges-center") => LinearDirection::EdgesCenter,
                _ => LinearDirection::LeftRight,
            };
            let single_indicator = props
                .get("indicator.type")
                .is_some_and(|v| v.eq_ignore_ascii_case("single"));

            SkinGeometry::Linear(LinearGeometry {
                direction,
                single_indicator,
                flip_left_x: parse_bool(props, "flip.left.x"),
                flip_right_x: parse_bool(props, "flip.right.x"),
                left_x: parse_f32(props, "left.x", 0.0),
                left_y: parse_f32(props, "left.y", 0.0),
                right_x: parse_f32(props, "right.x", 0.0),
                right_y: parse_f32(props, "right.y", 0.0),
                position_regular: parse_u32(props, "position.regular", 10),
                position_overload: parse_u32(props, "position.overload", 0),
                step_width_regular: parse_f32(props, "step.width.regular", 20.0),
                step_width_overload: parse_f32(props, "step.width.overload", 20.0),
            })
        }
        _ => return None,
    };

    Some(VuSkin {
        name,
        channels,
        bgr_filename,
        fgr_filename,
        indicator_filename,
        geometry,
    })
}

const SKIN_ASSETS: &[(&str, &[u8])] = &[
    (
        "bar-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/bar-bgr.png"),
    ),
    (
        "bar-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/bar-indicator.png"),
    ),
    (
        "big-bang-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/big-bang-bgr.png"),
    ),
    (
        "big-bang-needle.png",
        include_bytes!("../../../../assets/skins/800x480/big-bang-needle.png"),
    ),
    (
        "black-white-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/black-white-bgr.png"),
    ),
    (
        "black-white-needle.png",
        include_bytes!("../../../../assets/skins/800x480/black-white-needle.png"),
    ),
    (
        "blue-2-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/blue-2-bgr.png"),
    ),
    (
        "blue-2-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/blue-2-fgr.png"),
    ),
    (
        "blue-2-needle.png",
        include_bytes!("../../../../assets/skins/800x480/blue-2-needle.png"),
    ),
    (
        "blue-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/blue-bgr.png"),
    ),
    (
        "blue-needle.png",
        include_bytes!("../../../../assets/skins/800x480/blue-needle.png"),
    ),
    (
        "chillout-bgr.jpg",
        include_bytes!("../../../../assets/skins/800x480/chillout-bgr.jpg"),
    ),
    (
        "chillout-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/chillout-indicator.png"),
    ),
    (
        "compass-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/compass-bgr.png"),
    ),
    (
        "compass-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/compass-fgr.png"),
    ),
    (
        "compass-needle.png",
        include_bytes!("../../../../assets/skins/800x480/compass-needle.png"),
    ),
    (
        "dash-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/dash-bgr.png"),
    ),
    (
        "dash-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/dash-indicator.png"),
    ),
    (
        "emerald-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/emerald-bgr.png"),
    ),
    (
        "emerald-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/emerald-fgr.png"),
    ),
    (
        "emerald-needle.png",
        include_bytes!("../../../../assets/skins/800x480/emerald-needle.png"),
    ),
    (
        "fantasy-bgr.jpg",
        include_bytes!("../../../../assets/skins/800x480/fantasy-bgr.jpg"),
    ),
    (
        "fantasy-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/fantasy-indicator.png"),
    ),
    (
        "gas-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/gas-bgr.png"),
    ),
    (
        "gas-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/gas-indicator.png"),
    ),
    (
        "gold-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/gold-bgr.png"),
    ),
    (
        "gold-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/gold-fgr.png"),
    ),
    (
        "gold-needle.png",
        include_bytes!("../../../../assets/skins/800x480/gold-needle.png"),
    ),
    (
        "grunge-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/grunge-bgr.png"),
    ),
    (
        "grunge-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/grunge-fgr.png"),
    ),
    (
        "grunge-needle.png",
        include_bytes!("../../../../assets/skins/800x480/grunge-needle.png"),
    ),
    (
        "orange-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/orange-bgr.png"),
    ),
    (
        "orange-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/orange-fgr.png"),
    ),
    (
        "orange-needle.png",
        include_bytes!("../../../../assets/skins/800x480/orange-needle.png"),
    ),
    (
        "rainbow-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/rainbow-bgr.png"),
    ),
    (
        "rainbow-needle.png",
        include_bytes!("../../../../assets/skins/800x480/rainbow-needle.png"),
    ),
    (
        "red-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/red-bgr.png"),
    ),
    (
        "red-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/red-fgr.png"),
    ),
    (
        "red-needle.png",
        include_bytes!("../../../../assets/skins/800x480/red-needle.png"),
    ),
    (
        "relax-bgr.jpg",
        include_bytes!("../../../../assets/skins/800x480/relax-bgr.jpg"),
    ),
    (
        "relax-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/relax-indicator.png"),
    ),
    (
        "ring-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/ring-bgr.png"),
    ),
    (
        "ring-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/ring-fgr.png"),
    ),
    (
        "ring-needle.png",
        include_bytes!("../../../../assets/skins/800x480/ring-needle.png"),
    ),
    (
        "royal-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/royal-bgr.png"),
    ),
    (
        "royal-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/royal-fgr.png"),
    ),
    (
        "royal-needle.png",
        include_bytes!("../../../../assets/skins/800x480/royal-needle.png"),
    ),
    (
        "steam-punk-bgr.jpg",
        include_bytes!("../../../../assets/skins/800x480/steam-punk-bgr.jpg"),
    ),
    (
        "steam-punk-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/steam-punk-indicator.png"),
    ),
    (
        "tube-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/tube-bgr.png"),
    ),
    (
        "tube-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/tube-fgr.png"),
    ),
    (
        "tube-needle.png",
        include_bytes!("../../../../assets/skins/800x480/tube-needle.png"),
    ),
    (
        "vertical-circular-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/vertical-circular-bgr.png"),
    ),
    (
        "vertical-circular-needle.png",
        include_bytes!("../../../../assets/skins/800x480/vertical-circular-needle.png"),
    ),
    (
        "vertical-linear-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/vertical-linear-bgr.png"),
    ),
    (
        "vertical-linear-indicator.png",
        include_bytes!("../../../../assets/skins/800x480/vertical-linear-indicator.png"),
    ),
    (
        "vintage-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/vintage-bgr.png"),
    ),
    (
        "vintage-fgr.png",
        include_bytes!("../../../../assets/skins/800x480/vintage-fgr.png"),
    ),
    (
        "vintage-needle.png",
        include_bytes!("../../../../assets/skins/800x480/vintage-needle.png"),
    ),
    (
        "white-red-bgr.png",
        include_bytes!("../../../../assets/skins/800x480/white-red-bgr.png"),
    ),
    (
        "white-red-needle.png",
        include_bytes!("../../../../assets/skins/800x480/white-red-needle.png"),
    ),
];

/// Returns the embedded bytes for any asset filename referenced in `meters.txt`.
#[must_use]
pub fn embedded_skin_bytes(filename: &str) -> Option<&'static [u8]> {
    SKIN_ASSETS
        .iter()
        .find(|(name, _)| *name == filename)
        .map(|(_, bytes)| *bytes)
}
