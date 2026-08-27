use arcen_protocol::messages::{
    RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg, RegionPointerEnterMsg,
    RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/runtime/region_input.json"
));

fn assert_golden<T>(json: &str)
where
    T: DeserializeOwned + Serialize,
{
    let parsed: T = serde_json::from_str(json).unwrap();
    assert_eq!(
        serde_json::to_value(parsed).unwrap(),
        serde_json::from_str::<serde_json::Value>(json).unwrap()
    );
}

#[test]
fn cross_component_region_input_vectors_round_trip_and_validate() {
    let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
    let events = baseline["events"].as_array().unwrap();

    macro_rules! assert_event {
        ($value:expr, $ty:ty) => {{
            let parsed: $ty = serde_json::from_value($value.clone()).unwrap();
            parsed.validate().unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), *$value);
        }};
    }

    for event in events {
        match event["type"].as_str().unwrap() {
            "region_pointer_enter" => assert_event!(event, RegionPointerEnterMsg),
            "region_pointer_motion" => assert_event!(event, RegionPointerMotionMsg),
            "region_pointer_button" => assert_event!(event, RegionPointerButtonMsg),
            "region_pointer_scroll" => assert_event!(event, RegionPointerScrollMsg),
            "region_pen_event" => assert_event!(event, RegionPenEventMsg),
            "region_pointer_leave" => assert_event!(event, RegionPointerLeaveMsg),
            event_type => panic!("unexpected baseline event {event_type}"),
        }
    }
}

#[test]
fn region_input_golden_json_vectors_round_trip() {
    assert_golden::<RegionPointerEnterMsg>(include_str!("vectors/region_pointer_enter.json"));
    assert_golden::<RegionPointerMotionMsg>(include_str!("vectors/region_pointer_motion.json"));
    assert_golden::<RegionPointerButtonMsg>(include_str!("vectors/region_pointer_button.json"));
    assert_golden::<RegionPointerScrollMsg>(include_str!("vectors/region_pointer_scroll.json"));
    assert_golden::<RegionPenEventMsg>(include_str!("vectors/region_pen_event.json"));
    assert_golden::<RegionPointerLeaveMsg>(include_str!("vectors/region_pointer_leave.json"));
}

#[test]
fn region_input_wire_validation_rejects_zero_identity_sequence_and_button() {
    let mut motion: RegionPointerMotionMsg =
        serde_json::from_str(include_str!("vectors/region_pointer_motion.json")).unwrap();
    motion.position.region_generation = 0;
    assert_eq!(
        motion.validate(),
        Err(RegionInputValidationError::ZeroRegionGeneration)
    );
    motion.position.region_generation = 7;
    motion.position.region_id = 0;
    assert_eq!(
        motion.validate(),
        Err(RegionInputValidationError::ZeroRegionId)
    );
    motion.position.region_id = 2;
    motion.metadata.sequence = 0;
    assert_eq!(
        motion.validate(),
        Err(RegionInputValidationError::ZeroSequence)
    );

    let mut button: RegionPointerButtonMsg =
        serde_json::from_str(include_str!("vectors/region_pointer_button.json")).unwrap();
    button.button = 0;
    assert_eq!(
        button.validate(),
        Err(RegionInputValidationError::ZeroButton)
    );
}
