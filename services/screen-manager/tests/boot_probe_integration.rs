//! Integration tests for the boot self-check probe orchestration.
#![cfg(feature = "ui")]

use screen_manager::ui::boot::{
    BootCheckId, BootCheckStatus, BootCheckUpdate, BootProbe, ProbeFn, probe_amp_with,
    probe_bluetooth_with, probe_display, probe_dsp_with, probe_sensors_with,
};
use speaker_bluetooth::BluetoothDevice;
use speaker_dsp::DspDevice;
use speaker_hardware::{AmpDevice, LightSensor, SensorId, TempSensor, Temperature};

fn collect(probes: [ProbeFn; 5]) -> Vec<BootCheckUpdate> {
    let mut updates = Vec::new();
    BootProbe::from_probes(probes).run(&mut |update| updates.push(update));
    updates
}

#[test]
fn check_id_labels_and_probe_debug_are_stable() {
    // Every label arm of `BootCheckId` (regression: UI boot rows must keep
    // their user-facing strings) plus the manual `Debug for BootProbe` impl.
    assert_eq!(BootCheckId::Display.label(), "Display");
    assert_eq!(BootCheckId::Amp.label(), "I2S Amplifier");
    assert_eq!(BootCheckId::Dsp.label(), "CamillaDSP");
    assert_eq!(BootCheckId::Bluetooth.label(), "Bluetooth Stack");
    assert_eq!(BootCheckId::Sensors.label(), "Sensors & Bus");

    let probe = BootProbe::from_probes(std::array::from_fn(|_| {
        let probe: ProbeFn = Box::new(|| Ok("ok".into()));
        probe
    }));
    let debug = format!("{probe:?}");
    assert!(debug.contains("BootProbe"), "Debug output: {debug}");
    assert!(debug.contains("probe_count"), "Debug output: {debug}");
}

#[test]
fn real_probe_bodies_run_hermetically_against_mocked_devices() {
    // The production probe bodies (status formatting, fault/operational
    // checks) run against mocked devices so this suite performs no live
    // I2C, sysfs or D-Bus I/O ("Ensure Hermetic Tests").
    let (amp, _amp_ctrl) = AmpDevice::new_mocked();
    let amp_detail = probe_amp_with(&amp).expect("mocked amp reports no live faults");
    assert!(amp_detail.contains("mode:"), "amp detail: {amp_detail}");

    let (dsp, _dsp_ctrl) = DspDevice::new_mocked();
    let dsp_detail = probe_dsp_with(&dsp).expect("mocked dsp operational");
    assert!(dsp_detail.contains("load:"), "dsp detail: {dsp_detail}");

    let (bt, _bt_ctrl) = BluetoothDevice::new_mocked();
    let bt_detail = probe_bluetooth_with(&bt).expect("mocked adapter operational");
    assert!(bt_detail.contains("connected"), "bt detail: {bt_detail}");

    let (temp, temp_ctrl) = TempSensor::new_mocked();
    temp_ctrl.set_temperature(
        SensorId::parse("28-00000feedbee").expect("valid 1-wire id"),
        Temperature::from_celsius(25.0).expect("valid temperature"),
    );
    let (light, _light_ctrl) = LightSensor::new_mocked();
    let sensor_detail = probe_sensors_with(&temp, &light).expect("mocked sensors");
    assert!(
        sensor_detail.contains("°C"),
        "sensor detail: {sensor_detail}"
    );
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
