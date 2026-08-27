//! Shared region input pipeline: one validation, decode, ordered-state, and
//! transform path for every host, with only point mapping left to the platform.

use arcen_input::{
    CoordinateTransformError, MappedRegionButton, MappedRegionInput, MappedRegionPen,
    MappedRegionScroll, PenTool, RegionAggregateParityError, RegionInputPipeline,
    RegionInputPipelineError, RegionInputStateError, RegionInputWireMessage, RegionInputWireRef,
    RegionLogicalPosition, RegionPenSample, RegionPointMapper, validate_aggregate_parity,
};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionDescriptor, AppliedRegionSet, AppliedSize,
    LogicalPoint, LogicalRect, LogicalSize, OutputIdentity, OutputTransform, PhysicalSize,
    RegionContractError, RegionDescriptor, RegionGeneration, RegionId, RegionSet, Scale120,
};
use arcen_protocol::messages::{
    PenToolMsg, REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER,
    REGION_POINTER_LEAVE, REGION_POINTER_MOTION, REGION_POINTER_SCROLL, RegionInputMetadataMsg,
    RegionInputPositionMsg, RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg,
    RegionPointerEnterMsg, RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};

const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/runtime/region_input.json"
));

/// Records the applied points a platform would inject without adding any
/// platform-specific arithmetic of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct IdentityMapper;

impl RegionPointMapper for IdentityMapper {
    type Point = AppliedPoint;
    type Error = TestMapError;

    fn map_applied(&self, point: AppliedPoint) -> Result<AppliedPoint, TestMapError> {
        Ok(point)
    }
}

/// Proves the shared pipeline never advances state on a platform failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FailingMapper;

impl RegionPointMapper for FailingMapper {
    type Point = AppliedPoint;
    type Error = TestMapError;

    fn map_applied(&self, _point: AppliedPoint) -> Result<AppliedPoint, TestMapError> {
        Err(TestMapError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TestMapError;

impl std::fmt::Display for TestMapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the platform cannot inject this point")
    }
}

impl std::error::Error for TestMapError {}

fn descriptor(
    id: u32,
    output: &str,
    origin_px: (i64, i64),
    size_px: (u64, u64),
) -> RegionDescriptor {
    descriptor_with_primary(id, output, origin_px, size_px, id == 1)
}

fn descriptor_with_primary(
    id: u32,
    output: &str,
    origin_px: (i64, i64),
    size_px: (u64, u64),
    primary: bool,
) -> RegionDescriptor {
    RegionDescriptor::new(
        RegionId::new(id).unwrap(),
        OutputIdentity::new(output).unwrap(),
        LogicalRect::new(
            LogicalPoint::from_pixels(origin_px.0, origin_px.1).unwrap(),
            LogicalSize::from_pixels(size_px.0, size_px.1).unwrap(),
        )
        .unwrap(),
        PhysicalSize::new(
            u32::try_from(size_px.0).unwrap(),
            u32::try_from(size_px.1).unwrap(),
        )
        .unwrap(),
        Scale120::new(120).unwrap(),
        OutputTransform::Normal,
        primary,
    )
}

fn applied(descriptor: RegionDescriptor, origin: (i64, i64)) -> AppliedRegionDescriptor {
    let size = AppliedSize::new(
        u32::try_from(descriptor.logical_rect().size().width() / 120).unwrap(),
        u32::try_from(descriptor.logical_rect().size().height() / 120).unwrap(),
    )
    .unwrap();
    AppliedRegionDescriptor::new(
        descriptor,
        AppliedRect::new(AppliedPoint::new(origin.0, origin.1), size).unwrap(),
    )
    .unwrap()
}

fn baseline_regions() -> AppliedRegionSet {
    AppliedRegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![applied(
            descriptor(1, "baseline-display", (0, 0), (1_920, 1_080)),
            (0, 0),
        )],
    )
    .unwrap()
}

fn two_regions() -> AppliedRegionSet {
    AppliedRegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![
            applied(descriptor(1, "left", (0, 0), (1_920, 1_080)), (0, 0)),
            applied(descriptor(2, "right", (1_920, 0), (1_280, 720)), (1_920, 0)),
        ],
    )
    .unwrap()
}

fn requested_baseline() -> RegionSet {
    RegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![descriptor(1, "baseline-display", (0, 0), (1_920, 1_080))],
    )
    .unwrap()
}

fn position(region: u32, x: i64, y: i64) -> RegionLogicalPosition {
    RegionLogicalPosition {
        region_id: RegionId::new(region).unwrap(),
        point: LogicalPoint::new(x, y),
    }
}

fn position_msg(region_generation: u64, region_id: u32, x: i64, y: i64) -> RegionInputPositionMsg {
    RegionInputPositionMsg {
        region_generation,
        region_id,
        logical_x: x,
        logical_y: y,
    }
}

fn metadata_msg(sequence: u64) -> RegionInputMetadataMsg {
    RegionInputMetadataMsg {
        sequence,
        timestamp_ns: sequence,
        coalescable: false,
    }
}

fn enter(region: u32, x: i64, y: i64, sequence: u64) -> RegionPointerEnterMsg {
    RegionPointerEnterMsg {
        msg_type: REGION_POINTER_ENTER.to_owned(),
        position: position_msg(7, region, x, y),
        metadata: metadata_msg(sequence),
    }
}

fn leave(region: u32, x: i64, y: i64, sequence: u64) -> RegionPointerLeaveMsg {
    RegionPointerLeaveMsg {
        msg_type: REGION_POINTER_LEAVE.to_owned(),
        position: position_msg(7, region, x, y),
        metadata: metadata_msg(sequence),
    }
}

fn motion(region: u32, x: i64, y: i64, sequence: u64) -> RegionPointerMotionMsg {
    RegionPointerMotionMsg {
        msg_type: REGION_POINTER_MOTION.to_owned(),
        position: position_msg(7, region, x, y),
        metadata: metadata_msg(sequence),
    }
}

fn button(
    region: u32,
    x: i64,
    y: i64,
    button: u8,
    pressed: bool,
    sequence: u64,
) -> RegionPointerButtonMsg {
    RegionPointerButtonMsg {
        msg_type: REGION_POINTER_BUTTON.to_owned(),
        position: position_msg(7, region, x, y),
        button,
        pressed,
        metadata: metadata_msg(sequence),
    }
}

fn scroll(
    region: u32,
    x: i64,
    y: i64,
    delta_x: i64,
    delta_y: i64,
    sequence: u64,
) -> RegionPointerScrollMsg {
    RegionPointerScrollMsg {
        msg_type: REGION_POINTER_SCROLL.to_owned(),
        position: position_msg(7, region, x, y),
        delta_x,
        delta_y,
        metadata: metadata_msg(sequence),
    }
}

fn pen(region: u32, x: i64, y: i64, sequence: u64) -> RegionPenEventMsg {
    RegionPenEventMsg {
        msg_type: REGION_PEN_EVENT.to_owned(),
        position: position_msg(7, region, x, y),
        pressure: 0.75,
        tilt_x_degrees: -12.0,
        tilt_y_degrees: 10.0,
        rotation_degrees: 180.0,
        tool: PenToolMsg::Tip,
        in_proximity: true,
        touching: true,
        buttons: 1,
        metadata: metadata_msg(sequence),
    }
}

fn pipeline() -> RegionInputPipeline<IdentityMapper> {
    RegionInputPipeline::new(baseline_regions(), IdentityMapper)
}

#[test]
fn every_event_maps_through_one_shared_pipeline_and_advances_shared_state() {
    let mut pipeline = pipeline();
    assert_eq!(
        pipeline.pointer_enter(&enter(1, 0, 0, 40)).unwrap(),
        AppliedPoint::new(0, 0)
    );
    assert_eq!(
        pipeline.pointer_motion(&motion(1, 60, 1_440, 41)).unwrap(),
        AppliedPoint::new(0, 12)
    );
    assert_eq!(
        pipeline
            .pointer_button(&button(1, 60, 1_440, 1, true, 42))
            .unwrap(),
        MappedRegionButton {
            position: AppliedPoint::new(0, 12),
            button: 1,
            pressed: true,
        }
    );
    assert_eq!(
        pipeline
            .pointer_scroll(&scroll(1, 60, 1_440, 120, -240, 43))
            .unwrap(),
        MappedRegionScroll {
            position: AppliedPoint::new(0, 12),
            delta_x: 120,
            delta_y: -240,
        }
    );
    let mapped_pen = pipeline.pen(&pen(1, 600, 900, 44)).unwrap();
    assert_eq!(mapped_pen.position, AppliedPoint::new(5, 7));
    assert_eq!(
        mapped_pen.sample,
        RegionPenSample {
            position: position(1, 600, 900),
            pressure: 0.75,
            tilt_x_degrees: -12.0,
            tilt_y_degrees: 10.0,
            rotation_degrees: 180.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 1,
        }
    );
    assert_eq!(
        pipeline.pointer_leave(&leave(1, 60, 1_440, 45)).unwrap(),
        AppliedPoint::new(0, 12)
    );

    let state = pipeline.state();
    assert_eq!(state.last_sequence(), 45);
    assert_eq!(state.held_buttons().collect::<Vec<_>>(), vec![1]);
    assert_eq!(
        state.latest_pointer_position(),
        Some(position(1, 60, 1_440))
    );
    assert_eq!(state.pen().unwrap().sample, mapped_pen.sample);
    assert!(!state.is_focused());
}

#[test]
fn the_cross_component_baseline_fixture_drives_the_shared_pipeline_to_its_frozen_endpoints() {
    let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
    let mut pipeline = pipeline();
    let mut pointer_endpoint = None;
    let mut pen_endpoint = None;

    for value in baseline["events"].as_array().unwrap() {
        let message = RegionInputWireMessage::from_json_value(value).unwrap();
        match pipeline.apply(message.as_ref()).unwrap() {
            MappedRegionInput::PointerMotion(point) => pointer_endpoint = Some(point),
            MappedRegionInput::Pen(MappedRegionPen { position, .. }) => {
                pen_endpoint = Some(position);
            }
            _ => {}
        }
    }

    assert_eq!(
        pointer_endpoint.unwrap(),
        AppliedPoint::new(
            baseline["pointer_endpoint"]["x"].as_i64().unwrap(),
            baseline["pointer_endpoint"]["y"].as_i64().unwrap(),
        )
    );
    assert_eq!(
        pen_endpoint.unwrap(),
        AppliedPoint::new(
            baseline["pen_endpoint"]["x"].as_i64().unwrap(),
            baseline["pen_endpoint"]["y"].as_i64().unwrap(),
        )
    );
    assert_eq!(pipeline.state().last_sequence(), 45);
}

#[test]
fn generic_apply_dispatch_matches_the_typed_entry_points() {
    let mut typed = pipeline();
    let mut generic = pipeline();

    let enter = enter(1, 0, 0, 40);
    let motion = motion(1, 60, 1_440, 41);
    let button = button(1, 60, 1_440, 1, true, 42);
    let scroll = scroll(1, 60, 1_440, 120, -240, 43);
    let pen = pen(1, 600, 900, 44);
    let leave = leave(1, 60, 1_440, 45);

    assert_eq!(
        generic
            .apply(RegionInputWireRef::PointerEnter(&enter))
            .unwrap(),
        MappedRegionInput::PointerEnter(typed.pointer_enter(&enter).unwrap())
    );
    assert_eq!(
        generic
            .apply(RegionInputWireRef::PointerMotion(&motion))
            .unwrap(),
        MappedRegionInput::PointerMotion(typed.pointer_motion(&motion).unwrap())
    );
    assert_eq!(
        generic
            .apply(RegionInputWireRef::PointerButton(&button))
            .unwrap(),
        MappedRegionInput::PointerButton(typed.pointer_button(&button).unwrap())
    );
    assert_eq!(
        generic
            .apply(RegionInputWireRef::PointerScroll(&scroll))
            .unwrap(),
        MappedRegionInput::PointerScroll(typed.pointer_scroll(&scroll).unwrap())
    );
    assert_eq!(
        generic.apply(RegionInputWireRef::Pen(&pen)).unwrap(),
        MappedRegionInput::Pen(typed.pen(&pen).unwrap())
    );
    assert_eq!(
        generic
            .apply(RegionInputWireRef::PointerLeave(&leave))
            .unwrap(),
        MappedRegionInput::PointerLeave(typed.pointer_leave(&leave).unwrap())
    );
    assert_eq!(generic.state(), typed.state());
}

#[test]
fn a_platform_mapping_failure_never_advances_the_shared_state() {
    let mut pipeline = RegionInputPipeline::new(baseline_regions(), FailingMapper);
    assert_eq!(
        pipeline.pointer_enter(&enter(1, 0, 0, 40)),
        Err(RegionInputPipelineError::Mapping(TestMapError))
    );
    assert_eq!(pipeline.state().last_sequence(), 0);
    assert!(!pipeline.state().is_focused());
    assert_eq!(pipeline.state().latest_pointer_position(), None);
}

#[test]
fn zero_wire_identities_are_rejected_as_wire_errors_without_advancing_state() {
    for (message, expected) in [
        (
            enter(1, 0, 0, 40),
            RegionInputValidationError::ZeroRegionGeneration,
        ),
        (enter(0, 0, 0, 40), RegionInputValidationError::ZeroRegionId),
        (enter(1, 0, 0, 0), RegionInputValidationError::ZeroSequence),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (mut message, expected))| {
        if index == 0 {
            message.position.region_generation = 0;
        }
        (message, expected)
    }) {
        let mut pipeline = pipeline();
        assert_eq!(
            pipeline.pointer_enter(&message),
            Err(RegionInputPipelineError::Wire(expected))
        );
        assert_eq!(pipeline.state().last_sequence(), 0);
    }

    let mut pipeline = pipeline();
    assert_eq!(
        pipeline.pointer_button(&button(1, 0, 0, 0, true, 40)),
        Err(RegionInputPipelineError::Wire(
            RegionInputValidationError::ZeroButton
        ))
    );
    assert_eq!(pipeline.state().last_sequence(), 0);
}

#[test]
fn a_stale_generation_is_rejected_against_the_negotiated_aggregate() {
    let mut pipeline = pipeline();
    let mut stale = enter(1, 0, 0, 40);
    stale.position.region_generation = 6;
    assert_eq!(
        pipeline.pointer_enter(&stale),
        Err(RegionInputPipelineError::State(
            RegionInputStateError::StaleGeneration {
                expected: RegionGeneration::new(7).unwrap(),
                received: RegionGeneration::new(6).unwrap(),
            }
        ))
    );
    assert_eq!(pipeline.state().last_sequence(), 0);
    assert!(!pipeline.state().is_focused());
}

#[test]
fn an_unknown_region_is_rejected_by_the_shared_transform() {
    let mut pipeline = pipeline();
    assert_eq!(
        pipeline.pointer_enter(&enter(9, 0, 0, 40)),
        Err(RegionInputPipelineError::Transform(
            CoordinateTransformError::UnknownRegion(RegionId::new(9).unwrap())
        ))
    );
    assert_eq!(pipeline.state().last_sequence(), 0);
}

#[test]
fn a_position_outside_its_region_is_rejected_before_injection() {
    let mut pipeline = pipeline();
    assert_eq!(
        pipeline.pointer_enter(&enter(1, 1_920 * 120, 0, 40)),
        Err(RegionInputPipelineError::Transform(
            CoordinateTransformError::LogicalPointOutsideRegion
        ))
    );
    assert_eq!(pipeline.state().last_sequence(), 0);
    assert!(!pipeline.state().is_focused());
}

#[test]
fn a_non_increasing_sequence_is_rejected_after_a_successful_event() {
    let mut pipeline = pipeline();
    pipeline.pointer_enter(&enter(1, 0, 0, 40)).unwrap();
    assert_eq!(
        pipeline.pointer_motion(&motion(1, 60, 60, 40)),
        Err(RegionInputPipelineError::State(
            RegionInputStateError::InvalidSequence {
                last: 40,
                received: 40,
            }
        ))
    );
    assert_eq!(pipeline.state().last_sequence(), 40);
    assert_eq!(
        pipeline.state().latest_pointer_position(),
        Some(position(1, 0, 0))
    );
}

#[test]
fn out_of_range_pen_fields_are_rejected_as_wire_errors() {
    let mut pipeline = pipeline();
    let mut sample = pen(1, 600, 900, 40);
    sample.pressure = 2.0;
    assert_eq!(
        pipeline.pen(&sample),
        Err(RegionInputPipelineError::Wire(
            RegionInputValidationError::PenFieldOutOfRange("pressure")
        ))
    );
    assert_eq!(pipeline.state().pen(), None);
    assert_eq!(pipeline.state().last_sequence(), 0);
}

#[test]
fn a_button_edge_requires_the_exact_latest_authoritative_position() {
    let mut pipeline = pipeline();
    pipeline.pointer_enter(&enter(1, 0, 0, 40)).unwrap();
    pipeline.pointer_motion(&motion(1, 60, 1_440, 41)).unwrap();
    assert_eq!(
        pipeline.pointer_button(&button(1, 61, 1_440, 1, true, 42)),
        Err(RegionInputPipelineError::State(
            RegionInputStateError::ButtonPositionMismatch {
                expected: Some(position(1, 60, 1_440)),
                received: position(1, 61, 1_440),
            }
        ))
    );
    assert_eq!(pipeline.state().held_buttons().count(), 0);
}

#[test]
fn explicit_leave_then_enter_moves_focus_between_regions() {
    let mut pipeline = RegionInputPipeline::new(two_regions(), IdentityMapper);
    assert_eq!(
        pipeline.pointer_enter(&enter(1, 60, 60, 1)).unwrap(),
        AppliedPoint::new(0, 0)
    );
    assert_eq!(
        pipeline.pointer_leave(&leave(1, 60, 60, 2)).unwrap(),
        AppliedPoint::new(0, 0)
    );
    assert_eq!(
        pipeline.pointer_enter(&enter(2, 240, 240, 3)).unwrap(),
        AppliedPoint::new(1_922, 2)
    );
    assert_eq!(
        pipeline.state().active_pointer_region(),
        Some(RegionId::new(2).unwrap())
    );

    assert_eq!(
        pipeline.pointer_enter(&enter(1, 60, 60, 4)),
        Err(RegionInputPipelineError::State(
            RegionInputStateError::PointerAlreadyFocused
        ))
    );
}

#[test]
fn release_all_clears_shared_state_and_reports_the_native_releases() {
    let mut pipeline = pipeline();
    pipeline.pointer_enter(&enter(1, 0, 0, 40)).unwrap();
    pipeline.pointer_motion(&motion(1, 60, 1_440, 41)).unwrap();
    pipeline
        .pointer_button(&button(1, 60, 1_440, 1, true, 42))
        .unwrap();
    pipeline.pen(&pen(1, 600, 900, 43)).unwrap();

    let released = pipeline.release_all();
    assert_eq!(released.pointer_position, Some(position(1, 60, 1_440)));
    assert_eq!(released.buttons, vec![1]);
    assert!(released.pen.is_some());
    assert!(released.had_focus);

    assert_eq!(pipeline.state().held_buttons().count(), 0);
    assert_eq!(pipeline.state().pen(), None);
    assert!(!pipeline.state().is_focused());
    assert_eq!(pipeline.state().last_sequence(), 43);
}

#[test]
fn mapping_one_position_never_touches_the_ordered_state() {
    let pipeline = pipeline();
    assert_eq!(
        pipeline.map(position(1, 60, 1_440)).unwrap(),
        AppliedPoint::new(0, 12)
    );
    assert_eq!(pipeline.state().last_sequence(), 0);
    assert_eq!(
        pipeline.map(position(9, 0, 0)),
        Err(RegionInputPipelineError::Transform(
            CoordinateTransformError::UnknownRegion(RegionId::new(9).unwrap())
        ))
    );
}

#[test]
fn aggregate_parity_accepts_only_an_identical_requested_and_applied_roster() {
    let applied_regions = baseline_regions();
    assert_eq!(
        validate_aggregate_parity(&requested_baseline(), &applied_regions),
        Ok(())
    );
    assert!(
        RegionInputPipeline::with_aggregate_parity(
            &requested_baseline(),
            applied_regions.clone(),
            IdentityMapper,
        )
        .is_ok()
    );

    let other_generation = RegionSet::new(
        RegionGeneration::new(8).unwrap(),
        vec![descriptor(1, "baseline-display", (0, 0), (1_920, 1_080))],
    )
    .unwrap();
    assert_eq!(
        validate_aggregate_parity(&other_generation, &applied_regions),
        Err(RegionAggregateParityError)
    );

    let extra_region = RegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![
            descriptor(1, "baseline-display", (0, 0), (1_920, 1_080)),
            descriptor(2, "right", (1_920, 0), (1_280, 720)),
        ],
    )
    .unwrap();
    assert_eq!(
        validate_aggregate_parity(&extra_region, &applied_regions),
        Err(RegionAggregateParityError)
    );

    let other_descriptor = RegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![descriptor(1, "baseline-display", (0, 0), (1_280, 720))],
    )
    .unwrap();
    assert_eq!(
        validate_aggregate_parity(&other_descriptor, &applied_regions),
        Err(RegionAggregateParityError)
    );

    let renamed_region = RegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![descriptor_with_primary(
            2,
            "baseline-display",
            (0, 0),
            (1_920, 1_080),
            true,
        )],
    )
    .unwrap();
    assert_eq!(
        validate_aggregate_parity(&renamed_region, &applied_regions),
        Err(RegionAggregateParityError)
    );

    assert!(
        RegionInputPipeline::with_aggregate_parity(
            &renamed_region,
            applied_regions,
            IdentityMapper,
        )
        .is_err()
    );
}

#[test]
fn the_pipeline_exposes_the_negotiated_aggregate_and_platform_mapper() {
    let pipeline = pipeline();
    assert_eq!(
        pipeline.applied_regions().generation(),
        RegionGeneration::new(7).unwrap()
    );
    assert_eq!(*pipeline.mapper(), IdentityMapper);
}

#[test]
fn a_region_contract_failure_surfaces_as_a_contract_error() {
    assert_eq!(
        RegionId::new(0),
        Err(RegionContractError::ZeroRegionId),
        "the pipeline Contract variant exists for identities the wire layer cannot represent",
    );
}
