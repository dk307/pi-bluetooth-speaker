//! `CamillaDSP` WebSocket client and dBFS level meter telemetry.

pub mod client;

pub use camilladsp_schema;
pub use client::{
    DEFAULT_DSP_BUFFER_CAPACITY, DEFAULT_DSP_HOST, DEFAULT_DSP_PORT, DspChannelLevels,
    DspConfigInfo, DspDevice, DspDeviceMock, DspError, DspHardwareInfo, DspHost, DspPort,
    DspStatus, ProcessingState, StopReason, format_dbfs_meter, format_load_meter,
};

#[cfg(feature = "test-util")]
pub use client::test_helpers as dsp_test_helpers;
