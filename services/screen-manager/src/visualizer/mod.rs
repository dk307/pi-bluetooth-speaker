//! Real-time audio visualizer subsystem (`CamillaDSP` spectrum streaming, ERB mapping, ballistics).

pub mod dsp_math;
pub mod worker;

pub use dsp_math::{ERB_BAND_COUNT, ErbBandMapper, SpectrumBallistics};
pub use worker::{VisualizerController, VisualizerMode, VisualizerOutput};
