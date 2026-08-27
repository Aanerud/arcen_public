use std::process::ExitCode;

use arcen_keel::scenario::ScenarioKind;
use arcen_region_patch_benchmark::{ModelKind, ScenarioConfig, ScenarioReport, run_scenario};

fn main() -> ExitCode {
    match print_report() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("region patch report failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_report() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "scenario,model,emit,key,delta,keepalive,carrier_bytes,savings_bp,source_copy_bytes,\
source_copy_ops,compose_copy_ops,patches,fallbacks,growths,mismatches,cadence_i_k_r_s_c"
    );
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Drag,
        ScenarioKind::Scroll,
        ScenarioKind::Video,
        ScenarioKind::Burst,
    ] {
        let config = ScenarioConfig::report(kind);
        let full = run_scenario(config, ModelKind::FullPicture)?;
        print_row(full, full.metrics.carrier_bytes);
        for model in [
            ModelKind::DirtyRows,
            ModelKind::DirtyRects,
            ModelKind::BoundedPatches,
        ] {
            print_row(run_scenario(config, model)?, full.metrics.carrier_bytes);
        }
    }
    Ok(())
}

fn print_row(report: ScenarioReport, full_bytes: u64) {
    let metrics = report.metrics;
    println!(
        "{:?},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}:{}:{}:{}:{}",
        report.config.kind,
        report.model.name(),
        metrics.emitted_frames,
        metrics.keyframes,
        metrics.delta_frames,
        metrics.keepalives,
        metrics.carrier_bytes,
        savings_basis_points(full_bytes, metrics.carrier_bytes),
        metrics.source_copy_bytes,
        metrics.source_copy_operations,
        metrics.compositor_copy_operations,
        metrics.patches,
        metrics.full_frame_fallbacks,
        metrics.allocation_growths,
        report.reconstruction_mismatches,
        metrics.cadence.immediate,
        metrics.cadence.keepalive,
        metrics.cadence.responsive,
        metrics.cadence.smooth,
        metrics.cadence.continuous,
    );
}

fn savings_basis_points(full: u64, candidate: u64) -> i128 {
    if full == 0 {
        return 0;
    }
    let difference = i128::from(full) - i128::from(candidate);
    difference.saturating_mul(10_000) / i128::from(full)
}
