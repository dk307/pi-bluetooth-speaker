//! Boot-time service probes that feed the splash screen check log.
//!
//! Each check emits a [`BootCheckUpdate`] twice (running marker, then outcome)
//! through a caller-provided sink so the UI render loop stays decoupled from
//! blocking hardware/D-Bus I/O.

use crate::hardware::{AmpDevice, BluetoothDevice, DspDevice, LightSensor, TempSensor};

/// Identifies one splash-screen boot check; declaration order equals row order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootCheckId {
    /// Display panel / framebuffer scanout contract.
    Display,
    /// MA12070P I2S amplifier.
    Amp,
    /// `CamillaDSP` processing pipeline.
    Dsp,
    /// `BlueZ` Bluetooth stack and audio sink.
    Bluetooth,
    /// 1-Wire temperature and BH1750 ambient-light sensors.
    Sensors,
}

impl BootCheckId {
    /// All checks in bottom-of-splash row order.
    pub const ALL: [Self; 5] = [
        Self::Display,
        Self::Amp,
        Self::Dsp,
        Self::Bluetooth,
        Self::Sensors,
    ];

    /// Human-readable check label shown at the start of the row text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Display => "Display",
            Self::Amp => "I2S Amplifier",
            Self::Dsp => "CamillaDSP",
            Self::Bluetooth => "Bluetooth Stack",
            Self::Sensors => "Sensors & Bus",
        }
    }
}

/// Outcome state of a single boot check row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootCheckStatus {
    /// Check is currently executing.
    Running,
    /// Check completed successfully.
    Passed,
    /// Check failed; row text carries the failure detail.
    Failed,
}

/// One incremental splash check-log row update.
#[derive(Debug, Clone)]
pub struct BootCheckUpdate {
    /// Which check the update belongs to.
    pub id: BootCheckId,
    /// New outcome state of the check.
    pub status: BootCheckStatus,
    /// Single-line row text (label plus result detail).
    pub text: Box<str>,
}

pub type ProbeFn = Box<dyn FnOnce() -> Result<Box<str>, Box<str>> + Send>;

/// Ordered set of boot probes executed by the splash screen.
pub struct BootProbe {
    probes: [ProbeFn; 5],
}

impl std::fmt::Debug for BootProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootProbe")
            .field("probe_count", &self.probes.len())
            .finish()
    }
}

impl BootProbe {
    /// Builds a boot probe set from exactly five injectable probe closures.
    #[must_use]
    pub fn from_probes(probes: [ProbeFn; 5]) -> Self {
        Self { probes }
    }

    /// Builds the production probe set wired to real appliance hardware.
    #[must_use]
    pub fn real() -> Self {
        Self::from_probes([
            Box::new(|| Ok(probe_display())),
            Box::new(probe_amp),
            Box::new(probe_dsp),
            Box::new(probe_bluetooth),
            Box::new(probe_sensors),
        ])
    }

    /// Runs every probe sequentially, emitting running/failed/passed updates.
    pub fn run(self, sink: &mut dyn FnMut(BootCheckUpdate)) {
        for (id, probe) in BootCheckId::ALL.into_iter().zip(self.probes) {
            sink(BootCheckUpdate {
                id,
                status: BootCheckStatus::Running,
                text: format!("{} …", id.label()).into(),
            });
            let (status, detail) = match probe() {
                Ok(detail) => (BootCheckStatus::Passed, detail),
                Err(detail) => (BootCheckStatus::Failed, detail),
            };
            sink(BootCheckUpdate {
                id,
                status,
                text: format!("{}: {detail}", id.label()).into(),
            });
        }
    }
}

#[must_use]
pub fn probe_display() -> Box<str> {
    Box::from("Freenove 4.3\" DSI 800x480 (Active)")
}

fn probe_amp() -> Result<Box<str>, Box<str>> {
    let device = AmpDevice::new().map_err(|e| e.summary().into_boxed_str())?;
    let status = device
        .read_status()
        .map_err(|e| e.summary().into_boxed_str())?;
    let live = status.live_errors.faults();
    if !live.is_empty() {
        return Err(format!("live faults: {}", status.live_errors.names_joined()).into());
    }
    Ok(format!(
        "mode: {} | latched faults: {}",
        status.power_mode,
        status.latched_errors.faults().len()
    )
    .into())
}

fn probe_dsp() -> Result<Box<str>, Box<str>> {
    let device = DspDevice::new();
    let status = device
        .read_status()
        .map_err(|e| e.summary().into_boxed_str())?;
    if !status.is_operational() {
        return Err(format!("state: {}", status.state.as_str()).into());
    }
    Ok(format!(
        "v{} | load: {:.1}% | buffer: {:.0}%",
        status.version.as_deref().unwrap_or("?"),
        status.processing_load,
        status.buffer_fill_percent()
    )
    .into())
}

fn probe_bluetooth() -> Result<Box<str>, Box<str>> {
    let device = BluetoothDevice::new().map_err(|e| e.summary().into_boxed_str())?;
    let status = device.status().map_err(|e| e.summary().into_boxed_str())?;
    if !status.is_operational() {
        return Err("adapter not operational".into());
    }
    Ok(format!(
        "{} connected | {}",
        status.connected_count(),
        if status.is_streaming() {
            "streaming"
        } else {
            "idle"
        }
    )
    .into())
}

fn probe_sensors() -> Result<Box<str>, Box<str>> {
    let temp = TempSensor::new();
    let temperature = temp
        .read_primary_temperature()
        .map_err(|e| e.summary().into_boxed_str())?;
    let light = LightSensor::new().map_err(|e| e.summary().into_boxed_str())?;
    let lux = light.read_lux().map_err(|e| e.summary().into_boxed_str())?;
    Ok(format!("{:.1} °C | {} lux", temperature.celsius(), lux).into())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn collect(probes: [ProbeFn; 5]) -> Vec<BootCheckUpdate> {
        let mut updates = Vec::new();
        BootProbe::from_probes(probes).run(&mut |update| updates.push(update));
        updates
    }

    #[test]
    fn probe_display_reports_active_dsi_panel() {
        assert_eq!(&*probe_display(), "Freenove 4.3\" DSI 800x480 (Active)");
    }

    #[test]
    fn run_emits_running_then_passed_per_check_in_order() {
        let probes = BootCheckId::ALL.map(|_| {
            let probe: ProbeFn = Box::new(|| Ok("ok detail".into()));
            probe
        });
        let updates = collect(probes);

        assert_eq!(updates.len(), 10);
        for (index, pair) in updates.chunks(2).enumerate() {
            assert_eq!(pair[0].id as usize, index);
            assert_eq!(pair[0].status, BootCheckStatus::Running);
            assert_eq!(
                pair[0].text,
                format!("{} …", BootCheckId::ALL[index].label()).into()
            );
            assert_eq!(pair[1].id as usize, index);
            assert_eq!(pair[1].status, BootCheckStatus::Passed);
            assert_eq!(
                pair[1].text,
                format!("{}: ok detail", BootCheckId::ALL[index].label()).into()
            );
        }
    }

    #[test]
    fn run_maps_probe_errors_to_failed_rows() {
        let probes: [ProbeFn; 5] =
            std::array::from_fn(|_| Box::new(|| Err("boom: sensor vanished".into())) as ProbeFn);
        let updates = collect(probes);

        assert_eq!(updates.len(), 10);
        for pair in updates.chunks(2) {
            assert_eq!(pair[1].status, BootCheckStatus::Failed);
            assert!(pair[1].text.ends_with(": boom: sensor vanished"));
        }
    }

    #[test]
    fn check_ids_map_to_dense_row_indexes() {
        for (index, id) in BootCheckId::ALL.iter().enumerate() {
            assert_eq!(*id as usize, index);
        }
        assert_eq!(BootCheckId::Display as usize, 0);
        assert_eq!(BootCheckId::Sensors as usize, 4);
    }
}
