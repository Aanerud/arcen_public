#![allow(clippy::expect_used)]

use arcen_keel::{
    ACTIVITY_ROLLING_WINDOW, ActivityClass, ActivityGrid, ActivityHint, BgraFrame,
    CadenceRecommendation, DirtyRatio, KernelPreference,
};

fn frame(pixels: &[u8], width: usize, height: usize) -> BgraFrame<'_> {
    BgraFrame::new(pixels, width, height, width * 4).expect("frame")
}

#[test]
fn classifies_idle_sparse_scroll_and_full_motion_with_matching_cadence() {
    const WIDTH: usize = 64;
    const HEIGHT: usize = 64;
    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    let mut activity =
        ActivityGrid::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("activity grid");

    let baseline = activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("baseline");
    assert!(baseline.baseline_refresh);
    assert_eq!(baseline.class, ActivityClass::FullMotion);
    assert_eq!(baseline.cadence, CadenceRecommendation::Immediate);
    assert_eq!(baseline.rolling_samples, 0);

    let idle = activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("idle");
    assert_eq!(idle.class, ActivityClass::Idle);
    assert_eq!(idle.cadence, CadenceRecommendation::Keepalive);

    pixels[0] = 1;
    let sparse = activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("sparse");
    assert_eq!(sparse.summary.dirty_blocks, 1);
    assert_eq!(sparse.class, ActivityClass::Sparse);
    assert_eq!(sparse.cadence, CadenceRecommendation::Responsive);

    for y in 0..(HEIGHT / 2) {
        pixels[y * WIDTH * 4..(y + 1) * WIDTH * 4].fill(2);
    }
    let scroll = activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("scroll");
    assert_eq!(scroll.summary.dirty_blocks, 8);
    assert_eq!(scroll.class, ActivityClass::Scroll);
    assert_eq!(scroll.cadence, CadenceRecommendation::Smooth);

    pixels.fill(3);
    let full_motion = activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("full motion");
    assert!(full_motion.summary.is_full_damage());
    assert_eq!(full_motion.class, ActivityClass::FullMotion);
    assert_eq!(full_motion.cadence, CadenceRecommendation::Continuous);
}

#[test]
fn source_scroll_hint_overrides_a_full_hash_delta() {
    const WIDTH: usize = 32;
    const HEIGHT: usize = 32;
    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    let mut activity =
        ActivityGrid::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("activity grid");
    activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("baseline");

    pixels.fill(9);
    let diagnostics = activity
        .update_with_hint(frame(&pixels, WIDTH, HEIGHT), ActivityHint::Scroll)
        .expect("scroll");
    assert_eq!(diagnostics.class, ActivityClass::Scroll);
    assert_eq!(diagnostics.cadence, CadenceRecommendation::Smooth);
}

#[test]
fn rolling_ratio_is_fixed_size_and_excludes_the_baseline() {
    const WIDTH: usize = 32;
    const HEIGHT: usize = 32;
    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    let mut activity =
        ActivityGrid::new(WIDTH, HEIGHT, KernelPreference::Xxh3).expect("activity grid");
    activity
        .update(frame(&pixels, WIDTH, HEIGHT))
        .expect("baseline");

    for _ in 0..ACTIVITY_ROLLING_WINDOW {
        pixels[0] = pixels[0].wrapping_add(1);
        let diagnostics = activity
            .update(frame(&pixels, WIDTH, HEIGHT))
            .expect("dirty update");
        assert_eq!(diagnostics.current_dirty_ratio.basis_points(), 2_500);
        assert_eq!(diagnostics.rolling_dirty_ratio.basis_points(), 2_500);
    }
    assert_eq!(
        activity.diagnostics().rolling_samples,
        u8::try_from(ACTIVITY_ROLLING_WINDOW).expect("window fits u8")
    );

    for _ in 0..ACTIVITY_ROLLING_WINDOW {
        activity
            .update(frame(&pixels, WIDTH, HEIGHT))
            .expect("clean update");
    }
    assert_eq!(activity.diagnostics().rolling_dirty_ratio, DirtyRatio::ZERO);
    assert_eq!(activity.diagnostics().class, ActivityClass::Idle);
}
