use arcen_input::{
    PenTool, RegionCoordinateTransformer, RegionInputEvent, RegionInputState,
    RegionInputStateError, RegionLogicalPosition, RegionPenSample,
};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionDescriptor, AppliedRegionSet, LogicalPoint,
    LogicalRect, LogicalSize, OutputIdentity, OutputTransform, PhysicalSize, RegionDescriptor,
    RegionGeneration, RegionId, Scale120,
};

const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/runtime/region_input.json"
));

fn regions() -> AppliedRegionSet {
    let descriptor = RegionDescriptor::new(
        RegionId::new(2).unwrap(),
        OutputIdentity::new("display-2").unwrap(),
        LogicalRect::new(
            LogicalPoint::from_pixels(-20, 10).unwrap(),
            LogicalSize::from_pixels(100, 80).unwrap(),
        )
        .unwrap(),
        PhysicalSize::new(100, 80).unwrap(),
        Scale120::new(120).unwrap(),
        OutputTransform::Normal,
        true,
    );
    let applied = AppliedRegionDescriptor::new(
        descriptor,
        AppliedRect::new(
            AppliedPoint::new(-100, 50),
            arcen_media::AppliedSize::new(100, 80).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    AppliedRegionSet::new(RegionGeneration::new(7).unwrap(), vec![applied]).unwrap()
}

fn baseline_regions() -> AppliedRegionSet {
    let descriptor = RegionDescriptor::new(
        RegionId::new(1).unwrap(),
        OutputIdentity::new("baseline-display").unwrap(),
        LogicalRect::new(
            LogicalPoint::new(0, 0),
            LogicalSize::from_pixels(1_920, 1_080).unwrap(),
        )
        .unwrap(),
        PhysicalSize::new(1_920, 1_080).unwrap(),
        Scale120::new(120).unwrap(),
        OutputTransform::Normal,
        true,
    );
    let applied = AppliedRegionDescriptor::new(
        descriptor,
        AppliedRect::new(
            AppliedPoint::new(0, 0),
            arcen_media::AppliedSize::new(1_920, 1_080).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    AppliedRegionSet::new(RegionGeneration::new(7).unwrap(), vec![applied]).unwrap()
}

fn position(x: i64, y: i64) -> RegionLogicalPosition {
    RegionLogicalPosition {
        region_id: RegionId::new(2).unwrap(),
        point: LogicalPoint::new(x, y),
    }
}

fn enter(sequence: u64) -> RegionInputEvent {
    RegionInputEvent::PointerEnter {
        generation: RegionGeneration::new(7).unwrap(),
        position: position(60, 120),
        sequence,
    }
}

#[test]
fn cross_component_fixture_freezes_state_and_physical_endpoints() {
    let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
    let regions = baseline_regions();
    let mut state = RegionInputState::default();

    for event in baseline["events"].as_array().unwrap() {
        let generation =
            RegionGeneration::new(event["region_generation"].as_u64().unwrap()).unwrap();
        let position = RegionLogicalPosition {
            region_id: RegionId::new(
                serde_json::from_value::<u32>(event["region_id"].clone()).unwrap(),
            )
            .unwrap(),
            point: LogicalPoint::new(
                event["logical_x"].as_i64().unwrap(),
                event["logical_y"].as_i64().unwrap(),
            ),
        };
        let sequence = event["sequence"].as_u64().unwrap();
        let event = match event["type"].as_str().unwrap() {
            "region_pointer_enter" => RegionInputEvent::PointerEnter {
                generation,
                position,
                sequence,
            },
            "region_pointer_motion" => RegionInputEvent::PointerMotion {
                generation,
                position,
                sequence,
            },
            "region_pointer_button" => RegionInputEvent::PointerButton {
                generation,
                position,
                button: serde_json::from_value(event["button"].clone()).unwrap(),
                pressed: event["pressed"].as_bool().unwrap(),
                sequence,
            },
            "region_pointer_scroll" => RegionInputEvent::PointerScroll {
                generation,
                position,
                delta_x: event["delta_x"].as_i64().unwrap(),
                delta_y: event["delta_y"].as_i64().unwrap(),
                sequence,
            },
            "region_pen_event" => RegionInputEvent::Pen {
                generation,
                sample: RegionPenSample {
                    position,
                    pressure: serde_json::from_value(event["pressure"].clone()).unwrap(),
                    tilt_x_degrees: serde_json::from_value(event["tilt_x_degrees"].clone())
                        .unwrap(),
                    tilt_y_degrees: serde_json::from_value(event["tilt_y_degrees"].clone())
                        .unwrap(),
                    rotation_degrees: serde_json::from_value(event["rotation_degrees"].clone())
                        .unwrap(),
                    tool: PenTool::Tip,
                    in_proximity: event["in_proximity"].as_bool().unwrap(),
                    touching: event["touching"].as_bool().unwrap(),
                    buttons: serde_json::from_value(event["buttons"].clone()).unwrap(),
                },
                sequence,
            },
            "region_pointer_leave" => RegionInputEvent::PointerLeave {
                generation,
                position,
                sequence,
            },
            event_type => panic!("unexpected baseline event {event_type}"),
        };
        state.apply(&regions, event).unwrap();
    }

    let pointer = RegionLogicalPosition {
        region_id: RegionId::new(1).unwrap(),
        point: LogicalPoint::new(60, 1_440),
    };
    let transformer = RegionCoordinateTransformer::new(&regions);
    let pointer_endpoint = transformer
        .logical_to_applied(pointer.region_id, pointer.point)
        .unwrap();
    assert_eq!(
        pointer_endpoint,
        AppliedPoint::new(
            baseline["pointer_endpoint"]["x"].as_i64().unwrap(),
            baseline["pointer_endpoint"]["y"].as_i64().unwrap(),
        )
    );
    let pen = state.pen().unwrap().sample;
    assert_eq!(
        transformer
            .logical_to_applied(pen.position.region_id, pen.position.point)
            .unwrap(),
        AppliedPoint::new(
            baseline["pen_endpoint"]["x"].as_i64().unwrap(),
            baseline["pen_endpoint"]["y"].as_i64().unwrap(),
        )
    );
    assert_eq!(state.latest_pointer_position(), Some(pointer));
    assert_eq!(state.held_buttons().collect::<Vec<_>>(), vec![1]);
    assert_eq!(state.last_sequence(), 45);
    assert!(!state.is_focused());
    assert_eq!(state.active_pointer_region(), None);
}

#[test]
fn rejects_stale_generation_unknown_region_and_bad_sequence_without_mutation() {
    let regions = regions();
    let mut state = RegionInputState::default();
    let stale = RegionInputEvent::PointerEnter {
        generation: RegionGeneration::new(6).unwrap(),
        position: position(0, 0),
        sequence: 1,
    };
    assert!(matches!(
        state.apply(&regions, stale),
        Err(RegionInputStateError::StaleGeneration { .. })
    ));

    let unknown = RegionInputEvent::PointerEnter {
        generation: regions.generation(),
        position: RegionLogicalPosition {
            region_id: RegionId::new(99).unwrap(),
            point: LogicalPoint::new(0, 0),
        },
        sequence: 1,
    };
    assert_eq!(
        state.apply(&regions, unknown),
        Err(RegionInputStateError::UnknownRegion(
            RegionId::new(99).unwrap()
        ))
    );
    assert!(state.apply(&regions, enter(1)).is_ok());
    assert!(matches!(
        state.apply(
            &regions,
            RegionInputEvent::PointerMotion {
                generation: regions.generation(),
                position: position(120, 120),
                sequence: 1,
            }
        ),
        Err(RegionInputStateError::InvalidSequence { .. })
    ));
    assert_eq!(state.latest_pointer_position(), Some(position(60, 120)));
    assert_eq!(state.last_sequence(), 1);
}

#[test]
fn button_edges_require_the_latest_authoritative_position() {
    let regions = regions();
    let mut state = RegionInputState::default();
    state.apply(&regions, enter(1)).unwrap();

    let mismatched = RegionInputEvent::PointerButton {
        generation: regions.generation(),
        position: position(61, 120),
        button: 1,
        pressed: true,
        sequence: 2,
    };
    assert!(matches!(
        state.apply(&regions, mismatched),
        Err(RegionInputStateError::ButtonPositionMismatch { .. })
    ));
    assert_eq!(state.last_sequence(), 1);

    state
        .apply(
            &regions,
            RegionInputEvent::PointerButton {
                generation: regions.generation(),
                position: position(60, 120),
                button: 1,
                pressed: true,
                sequence: 2,
            },
        )
        .unwrap();
    assert_eq!(state.held_buttons().collect::<Vec<_>>(), vec![1]);
}

#[test]
fn release_all_reports_and_clears_buttons_pen_and_focus() {
    let regions = regions();
    let mut state = RegionInputState::default();
    state.apply(&regions, enter(1)).unwrap();
    state
        .apply(
            &regions,
            RegionInputEvent::PointerButton {
                generation: regions.generation(),
                position: position(60, 120),
                button: 3,
                pressed: true,
                sequence: 2,
            },
        )
        .unwrap();
    let pen = RegionPenSample {
        position: position(600, 900),
        pressure: 0.75,
        tilt_x_degrees: -12.0,
        tilt_y_degrees: 10.0,
        rotation_degrees: 180.0,
        tool: PenTool::Tip,
        in_proximity: true,
        touching: true,
        buttons: 1,
    };
    state
        .apply(
            &regions,
            RegionInputEvent::Pen {
                generation: regions.generation(),
                sample: pen,
                sequence: 3,
            },
        )
        .unwrap();

    let released = state.release_all();
    assert_eq!(released.buttons, vec![3]);
    assert_eq!(released.pen.unwrap().sample, pen);
    assert!(released.had_focus);
    assert!(!state.is_focused());
    assert_eq!(state.active_pointer_region(), None);
    assert_eq!(state.held_buttons().count(), 0);
    assert_eq!(state.pen(), None);
    assert_eq!(state.last_sequence(), 3);
}
