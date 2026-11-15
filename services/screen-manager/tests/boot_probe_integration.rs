//! Integration tests for the boot self-check probe orchestration.
#![cfg(feature = "ui")]

use screen_manager::ui::boot::{
    BootCheckId, BootCheckStatus, BootCheckUpdate, BootProbe, ProbeFn, probe_display,
};

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

    // `BootProbe::real()` constructs the production five-probe set; running it
    // must emit one Running row per check (probe bodies may fail on a host
    // without appliance hardware — only the row structure is asserted).
    let updates = {
        let mut updates = Vec::new();
        BootProbe::real().run(&mut |update| updates.push(update));
        updates
    };
    assert_eq!(updates.len(), BootCheckId::ALL.len() * 2);
    for (index, pair) in updates.chunks(2).enumerate() {
        assert_eq!(pair[0].id as usize, index);
        assert_eq!(pair[0].status, BootCheckStatus::Running);
        assert_eq!(pair[1].id as usize, index);
        assert!(
            matches!(
                pair[1].status,
                BootCheckStatus::Passed | BootCheckStatus::Failed
            ),
            "settled row must be Passed or Failed"
        );
    }
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
