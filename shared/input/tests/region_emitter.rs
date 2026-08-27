//! Shared client-side region input emitter: ordered state, derived focus
//! transitions, sequence allocation, and encoding in one place.

use arcen_input::{
    PenTool, RegionInputEmitError, RegionInputEmitter, RegionInputEvent, RegionInputPipeline,
    RegionInputStateError, RegionInputWireMessage, RegionLogicalPosition, RegionPenSample,
    RegionPointMapper,
};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionDescriptor, AppliedRegionSet, AppliedSize,
    LogicalPoint, LogicalRect, LogicalSize, OutputIdentity, OutputTransform, PhysicalSize,
    RegionDescriptor, RegionGeneration, RegionId, Scale120,
};
use arcen_protocol::messages::{
    REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER, REGION_POINTER_LEAVE,
    REGION_POINTER_MOTION, REGION_POINTER_SCROLL,
};

#[derive(Debug, Clone, Copy, Default)]
struct IdentityMapper;

impl RegionPointMapper for IdentityMapper {
    type Point = AppliedPoint;
    type Error = std::convert::Infallible;

    fn map_applied(&self, point: AppliedPoint) -> Result<AppliedPoint, Self::Error> {
        Ok(point)
    }
}

fn descriptor(
    id: u32,
    output: &str,
    origin_px: (i64, i64),
    size_px: (u64, u64),
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
        id == 1,
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

fn regions() -> AppliedRegionSet {
    AppliedRegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![
            applied(descriptor(1, "left", (0, 0), (1_920, 1_080)), (0, 0)),
            applied(descriptor(2, "right", (1_920, 0), (1_280, 720)), (1_920, 0)),
        ],
    )
    .unwrap()
}

fn position(region: u32, x: i64, y: i64) -> RegionLogicalPosition {
    RegionLogicalPosition {
        region_id: RegionId::new(region).unwrap(),
        point: LogicalPoint::new(x, y),
    }
}

fn pen_sample(position: RegionLogicalPosition) -> RegionPenSample {
    RegionPenSample {
        position,
        pressure: 0.5,
        tilt_x_degrees: 4.0,
        tilt_y_degrees: -4.0,
        rotation_degrees: 12.0,
        tool: PenTool::Tip,
        in_proximity: true,
        touching: true,
        buttons: 0,
    }
}

fn shape(messages: &[RegionInputWireMessage]) -> Vec<(&'static str, u64, bool)> {
    messages
        .iter()
        .map(|message| {
            (
                message.input_type(),
                message.metadata().sequence,
                message.metadata().coalescable,
            )
        })
        .collect()
}

#[test]
fn the_first_pointer_sample_in_a_region_derives_exactly_one_enter() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let messages = emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_000)
        .unwrap();
    assert_eq!(shape(&messages), vec![(REGION_POINTER_ENTER, 1, false)]);
    assert_eq!(
        emitter.state().active_pointer_region(),
        Some(RegionId::new(1).unwrap())
    );
    assert_eq!(emitter.sequence(), 1);
}

#[test]
fn a_new_position_in_the_same_region_derives_motion_and_an_unchanged_one_derives_nothing() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_000)
        .unwrap();

    let moved = emitter
        .pointer_motion(&regions, position(1, 120, 60), 9_001)
        .unwrap();
    assert_eq!(shape(&moved), vec![(REGION_POINTER_MOTION, 2, true)]);

    let unchanged = emitter
        .pointer_motion(&regions, position(1, 120, 60), 9_002)
        .unwrap();
    assert!(unchanged.is_empty());
    assert_eq!(emitter.sequence(), 2);
    assert_eq!(emitter.state().last_sequence(), 2);
}

#[test]
fn crossing_into_another_region_derives_leave_then_enter_in_order() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_000)
        .unwrap();

    let crossed = emitter
        .pointer_motion(&regions, position(2, 240, 240), 9_001)
        .unwrap();
    assert_eq!(
        shape(&crossed),
        vec![
            (REGION_POINTER_LEAVE, 2, false),
            (REGION_POINTER_ENTER, 3, false),
        ]
    );
    assert_eq!(crossed[0].position().region_id, 1);
    assert_eq!(crossed[1].position().region_id, 2);
    assert_eq!(
        emitter.state().active_pointer_region(),
        Some(RegionId::new(2).unwrap())
    );
}

#[test]
fn a_button_edge_is_emitted_once_and_only_when_the_held_state_changes() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let pressed = emitter
        .pointer_button(&regions, position(1, 60, 60), 1, true, 9_000)
        .unwrap();
    assert_eq!(
        shape(&pressed),
        vec![
            (REGION_POINTER_ENTER, 1, false),
            (REGION_POINTER_BUTTON, 2, false),
        ]
    );
    assert_eq!(pressed[1].button_state(), Some((1, true)));

    let repeated = emitter
        .pointer_button(&regions, position(1, 60, 60), 1, true, 9_001)
        .unwrap();
    assert!(repeated.is_empty());

    let released = emitter
        .pointer_button(&regions, position(1, 60, 60), 1, false, 9_002)
        .unwrap();
    assert_eq!(shape(&released), vec![(REGION_POINTER_BUTTON, 3, false)]);
    assert_eq!(emitter.state().held_buttons().count(), 0);
}

#[test]
fn a_pointer_sample_emits_one_edge_per_changed_button_after_placing_the_pointer() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let messages = emitter
        .pointer_sample(
            &regions,
            position(1, 60, 60),
            &[(1, true), (2, false), (3, true)],
            9_000,
        )
        .unwrap();
    assert_eq!(
        shape(&messages),
        vec![
            (REGION_POINTER_ENTER, 1, false),
            (REGION_POINTER_BUTTON, 2, false),
            (REGION_POINTER_BUTTON, 3, false),
        ]
    );
    assert_eq!(messages[1].button_state(), Some((1, true)));
    assert_eq!(messages[2].button_state(), Some((3, true)));
    assert_eq!(
        emitter.state().held_buttons().collect::<Vec<_>>(),
        vec![1, 3]
    );

    let idle = emitter
        .pointer_sample(
            &regions,
            position(1, 60, 60),
            &[(1, true), (3, true)],
            9_001,
        )
        .unwrap();
    assert!(idle.is_empty());
}

#[test]
fn a_button_at_the_latest_position_never_moves_the_pointer() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    assert_eq!(
        emitter.pointer_button_at_latest(&regions, 1, true, 9_000),
        Err(RegionInputEmitError::MissingPointerPosition)
    );

    emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_000)
        .unwrap();
    let pressed = emitter
        .pointer_button_at_latest(&regions, 1, true, 9_001)
        .unwrap()
        .unwrap();
    assert_eq!(pressed.input_type(), REGION_POINTER_BUTTON);
    assert_eq!(pressed.metadata().sequence, 2);
    assert_eq!(pressed.position().logical_x, 60);
    assert!(
        emitter
            .pointer_button_at_latest(&regions, 1, true, 9_002)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_scroll_is_suppressed_when_both_deltas_are_zero() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let scrolled = emitter
        .pointer_scroll(&regions, position(1, 60, 60), 0, -240, 9_000)
        .unwrap();
    assert_eq!(
        shape(&scrolled),
        vec![
            (REGION_POINTER_ENTER, 1, false),
            (REGION_POINTER_SCROLL, 2, false),
        ]
    );

    let idle = emitter
        .pointer_scroll(&regions, position(1, 60, 60), 0, 0, 9_001)
        .unwrap();
    assert!(idle.is_empty());
    assert_eq!(emitter.sequence(), 2);
}

#[test]
fn pen_samples_allocate_or_adopt_a_sequence_and_stay_monotonic() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let sample = pen_sample(position(1, 600, 900));
    let allocated = emitter.pen(&regions, sample, 9_000, true).unwrap();
    assert_eq!(allocated.input_type(), REGION_PEN_EVENT);
    assert_eq!(allocated.metadata().sequence, 1);
    assert!(allocated.metadata().coalescable);

    let adopted = emitter
        .pen_with_sequence(&regions, sample, 40, 9_001, false)
        .unwrap();
    assert_eq!(adopted.metadata().sequence, 40);
    assert_eq!(emitter.sequence(), 40);

    let after = emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_002)
        .unwrap();
    assert_eq!(shape(&after), vec![(REGION_POINTER_ENTER, 41, false)]);
}

#[test]
fn an_invalid_transition_is_rejected_without_consuming_client_state() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    assert_eq!(
        emitter.pointer_motion(&regions, position(1, 1_920 * 120, 0), 9_000),
        Err(RegionInputEmitError::State(
            RegionInputStateError::PositionOutsideRegion(position(1, 1_920 * 120, 0))
        ))
    );
    assert!(!emitter.state().is_focused());
    assert_eq!(emitter.state().last_sequence(), 0);

    let mut invalid_pen = pen_sample(position(1, 600, 900));
    invalid_pen.pressure = 4.0;
    assert_eq!(
        emitter.pen(&regions, invalid_pen, 9_001, false),
        Err(RegionInputEmitError::State(
            RegionInputStateError::PenFieldOutOfRange("pressure")
        ))
    );
    assert_eq!(emitter.state().pen(), None);
}

#[test]
fn release_all_clears_client_state_but_keeps_the_sequence_monotonic() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    emitter
        .pointer_button(&regions, position(1, 60, 60), 1, true, 9_000)
        .unwrap();
    emitter
        .pen(&regions, pen_sample(position(1, 600, 900)), 9_001, false)
        .unwrap();

    let released = emitter.release_all();
    assert_eq!(released.buttons, vec![1]);
    assert!(released.pen.is_some());
    assert!(released.had_focus);
    assert_eq!(emitter.state().last_sequence(), 3);

    let resumed = emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_002)
        .unwrap();
    assert_eq!(shape(&resumed), vec![(REGION_POINTER_ENTER, 4, false)]);
}

#[test]
fn an_emitter_continues_an_existing_session_sequence() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::with_sequence(99);
    let messages = emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_000)
        .unwrap();
    assert_eq!(shape(&messages), vec![(REGION_POINTER_ENTER, 100, false)]);
}

#[test]
fn a_session_global_counter_can_raise_but_never_lower_the_emitter_floor() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();

    // A client whose keyboard/pen paths allocated 40 sequences already
    // continues that one counter instead of restarting at 1.
    emitter.advance_sequence_to(40);
    let entered = emitter
        .pointer_motion(&regions, position(1, 60, 60), 9_000)
        .unwrap();
    assert_eq!(shape(&entered), vec![(REGION_POINTER_ENTER, 41, false)]);
    assert_eq!(emitter.sequence(), 41);

    // A stale floor never rewinds an already accepted region sequence.
    emitter.advance_sequence_to(7);
    assert_eq!(emitter.sequence(), 41);
    let moved = emitter
        .pointer_motion(&regions, position(1, 61, 60), 9_001)
        .unwrap();
    assert_eq!(shape(&moved), vec![(REGION_POINTER_MOTION, 42, true)]);
}

#[test]
fn everything_the_emitter_produces_is_accepted_by_the_host_pipeline_unchanged() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let mut emitted = Vec::new();

    emitted.extend(
        emitter
            .pointer_motion(&regions, position(1, 60, 60), 9_000)
            .unwrap(),
    );
    emitted.extend(
        emitter
            .pointer_button(&regions, position(1, 120, 120), 1, true, 9_001)
            .unwrap(),
    );
    emitted.extend(
        emitter
            .pointer_scroll(&regions, position(1, 120, 120), 120, -240, 9_002)
            .unwrap(),
    );
    emitted.push(
        emitter
            .pen(&regions, pen_sample(position(1, 600, 900)), 9_003, true)
            .unwrap(),
    );
    emitted.extend(
        emitter
            .pointer_button(&regions, position(1, 120, 120), 1, false, 9_004)
            .unwrap(),
    );
    emitted.extend(
        emitter
            .pointer_motion(&regions, position(2, 240, 240), 9_005)
            .unwrap(),
    );

    assert_eq!(
        shape(&emitted),
        vec![
            (REGION_POINTER_ENTER, 1, false),
            (REGION_POINTER_MOTION, 2, true),
            (REGION_POINTER_BUTTON, 3, false),
            (REGION_POINTER_SCROLL, 4, false),
            (REGION_PEN_EVENT, 5, true),
            (REGION_POINTER_BUTTON, 6, false),
            (REGION_POINTER_LEAVE, 7, false),
            (REGION_POINTER_ENTER, 8, false),
        ]
    );

    let mut pipeline = RegionInputPipeline::new(regions, IdentityMapper);
    for message in &emitted {
        pipeline.apply(message.as_ref()).unwrap();
    }
    assert_eq!(pipeline.state(), emitter.state());
}

#[test]
fn every_emitted_message_decodes_back_into_the_event_the_client_applied() {
    let regions = regions();
    let mut emitter = RegionInputEmitter::new();
    let messages = emitter
        .pointer_sample(&regions, position(1, 60, 60), &[(1, true)], 9_000)
        .unwrap();

    let decoded = messages
        .iter()
        .map(|message| message.decode().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        decoded,
        vec![
            RegionInputEvent::PointerEnter {
                generation: RegionGeneration::new(7).unwrap(),
                position: position(1, 60, 60),
                sequence: 1,
            },
            RegionInputEvent::PointerButton {
                generation: RegionGeneration::new(7).unwrap(),
                position: position(1, 60, 60),
                button: 1,
                pressed: true,
                sequence: 2,
            },
        ]
    );

    for (message, event) in messages.iter().zip(decoded) {
        assert_eq!(
            &RegionInputWireMessage::encode(
                event,
                message.metadata().timestamp_ns,
                message.metadata().coalescable
            ),
            message
        );
    }
}
