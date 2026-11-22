//! Real-time audio visualizer subsystem (`CamillaDSP` streaming, ERB/ballistics, `PeppyMeter` skins).

pub mod dsp_math;
pub mod skins;
pub mod worker;

pub use dsp_math::{
    DEFAULT_ZERO_VU_DBFS, ERB_BAND_COUNT, ErbBandMapper, SpectrumBallistics, dbfs_to_vu_ratio,
};
pub use skins::{
    CircularGeometry, LinearDirection, LinearGeometry, SkinGeometry, VuSkin, all_skins,
    embedded_skin_bytes, find_skin, parse_meters_txt,
};
pub use worker::{VisualizerController, VisualizerMode, VisualizerOutput};
