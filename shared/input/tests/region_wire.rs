//! Paired encode/decode parity for every input-v4 `Region*` message.

use arcen_input::{
    PenTool, RegionInputDecodeError, RegionInputEvent, RegionInputWireError,
    RegionInputWireMessage, RegionInputWireRef, RegionLogicalPosition, RegionPenSample,
    domain_generation, domain_pen_tool, domain_position, wire_pen_tool, wire_position,
};
use arcen_media::{LogicalPoint, RegionContractError, RegionGeneration, RegionId};
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

fn generation() -> RegionGeneration {
    RegionGeneration::new(7).unwrap()
}

fn position(x: i64, y: i64) -> RegionLogicalPosition {
    RegionLogicalPosition {
        region_id: RegionId::new(1).unwrap(),
        point: LogicalPoint::new(x, y),
    }
}

fn pen_sample() -> RegionPenSample {
    RegionPenSample {
        position: position(600, 900),
        pressure: 0.75,
        tilt_x_degrees: -12.0,
        tilt_y_degrees: 10.0,
        rotation_degrees: 180.0,
        tool: PenTool::Tip,
        in_proximity: true,
        touching: true,
        buttons: 1,
    }
}

fn every_event() -> Vec<(RegionInputEvent, u64, bool)> {
    vec![
        (
            RegionInputEvent::PointerEnter {
                generation: generation(),
                position: position(0, 0),
                sequence: 40,
            },
            9_000,
            false,
        ),
        (
            RegionInputEvent::PointerMotion {
                generation: generation(),
                position: position(60, 1_440),
                sequence: 41,
            },
            9_001,
            true,
        ),
        (
            RegionInputEvent::PointerButton {
                generation: generation(),
                position: position(60, 1_440),
                button: 1,
                pressed: true,
                sequence: 42,
            },
            9_002,
            false,
        ),
        (
            RegionInputEvent::PointerScroll {
                generation: generation(),
                position: position(60, 1_440),
                delta_x: 120,
                delta_y: -240,
                sequence: 43,
            },
            9_003,
            false,
        ),
        (
            RegionInputEvent::Pen {
                generation: generation(),
                sample: pen_sample(),
                sequence: 44,
            },
            9_004,
            true,
        ),
        (
            RegionInputEvent::PointerLeave {
                generation: generation(),
                position: position(60, 1_440),
                sequence: 45,
            },
            9_005,
            false,
        ),
    ]
}

fn valid_position_msg() -> RegionInputPositionMsg {
    RegionInputPositionMsg {
        region_generation: 7,
        region_id: 1,
        logical_x: 60,
        logical_y: 1_440,
    }
}

fn valid_metadata_msg() -> RegionInputMetadataMsg {
    RegionInputMetadataMsg {
        sequence: 41,
        timestamp_ns: 9_001,
        coalescable: true,
    }
}

fn motion_msg() -> RegionPointerMotionMsg {
    RegionPointerMotionMsg {
        msg_type: REGION_POINTER_MOTION.to_owned(),
        position: valid_position_msg(),
        metadata: valid_metadata_msg(),
    }
}

fn pen_msg() -> RegionPenEventMsg {
    RegionPenEventMsg {
        msg_type: REGION_PEN_EVENT.to_owned(),
        position: valid_position_msg(),
        pressure: 0.5,
        tilt_x_degrees: 0.0,
        tilt_y_degrees: 0.0,
        rotation_degrees: 0.0,
        tool: PenToolMsg::Eraser,
        in_proximity: true,
        touching: false,
        buttons: 0,
        metadata: valid_metadata_msg(),
    }
}

#[test]
fn every_region_event_survives_encode_json_decode_unchanged() {
    for (event, timestamp_ns, coalescable) in every_event() {
        let encoded = RegionInputWireMessage::encode(event, timestamp_ns, coalescable);
        encoded.validate().unwrap();
        assert_eq!(encoded.metadata().sequence, event.sequence());
        assert_eq!(encoded.metadata().timestamp_ns, timestamp_ns);
        assert_eq!(encoded.metadata().coalescable, coalescable);

        let json = encoded.to_json_value().unwrap();
        let reparsed = RegionInputWireMessage::from_json_value(&json).unwrap();
        assert_eq!(reparsed, encoded);
        assert_eq!(reparsed.decode().unwrap(), event);

        let from_str =
            RegionInputWireMessage::from_json_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(from_str.decode().unwrap(), event);
        assert_eq!(from_str.as_ref().decode().unwrap(), event);
        assert_eq!(from_str.as_ref().to_owned_message(), encoded);
    }
}

#[test]
fn every_encoded_message_carries_its_canonical_wire_type_and_position() {
    let encoded = every_event()
        .into_iter()
        .map(|(event, timestamp_ns, coalescable)| {
            RegionInputWireMessage::encode(event, timestamp_ns, coalescable)
        })
        .collect::<Vec<_>>();
    let types = encoded
        .iter()
        .map(RegionInputWireMessage::input_type)
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            REGION_POINTER_ENTER,
            REGION_POINTER_MOTION,
            REGION_POINTER_BUTTON,
            REGION_POINTER_SCROLL,
            REGION_PEN_EVENT,
            REGION_POINTER_LEAVE,
        ]
    );
    for message in &encoded {
        assert_eq!(message.position().region_generation, 7);
        assert_eq!(message.position().region_id, 1);
        assert_eq!(message.as_ref().position(), message.position());
        assert_eq!(message.as_ref().input_type(), message.input_type());
        assert_eq!(message.as_ref().metadata(), message.metadata());
        assert_eq!(message.as_ref().button_state(), message.button_state());
    }
    assert_eq!(encoded[2].button_state(), Some((1, true)));
    assert_eq!(encoded[0].button_state(), None);
}

#[test]
fn shared_encoder_reproduces_the_cross_component_baseline_fixture_byte_for_byte() {
    let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
    let fixture_events = baseline["events"].as_array().unwrap();
    let encoded = every_event()
        .into_iter()
        .map(|(event, timestamp_ns, coalescable)| {
            RegionInputWireMessage::encode(event, timestamp_ns, coalescable)
        })
        .collect::<Vec<_>>();
    assert_eq!(fixture_events.len(), encoded.len());

    for (fixture, message) in fixture_events.iter().zip(&encoded) {
        assert_eq!(&message.to_json_value().unwrap(), fixture);
        let parsed = RegionInputWireMessage::from_json_value(fixture).unwrap();
        assert_eq!(&parsed, message);
    }
}

#[test]
fn baseline_fixture_events_decode_into_the_canonical_domain_stream() {
    let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
    let decoded = baseline["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| {
            RegionInputWireMessage::from_json_value(value)
                .unwrap()
                .decode()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let expected = every_event()
        .into_iter()
        .map(|(event, _, _)| event)
        .collect::<Vec<_>>();
    assert_eq!(decoded, expected);
}

#[test]
fn unknown_or_malformed_json_is_rejected_without_a_message() {
    let unknown = serde_json::json!({ "type": "region_pointer_teleport", "sequence": 1 });
    assert!(matches!(
        RegionInputWireMessage::from_json_value(&unknown),
        Err(RegionInputWireError::UnknownMessageType(ref found))
            if found == "region_pointer_teleport"
    ));
    assert!(matches!(
        RegionInputWireMessage::from_json_value(&serde_json::json!({ "sequence": 1 })),
        Err(RegionInputWireError::UnknownMessageType(_))
    ));
    assert!(matches!(
        RegionInputWireMessage::from_json_str("{not json"),
        Err(RegionInputWireError::Json(_))
    ));
    assert!(matches!(
        RegionInputWireMessage::from_json_value(&serde_json::json!({
            "type": REGION_POINTER_MOTION,
            "region_generation": 7,
        })),
        Err(RegionInputWireError::Json(_))
    ));
}

#[test]
fn a_parsed_but_invalid_message_fails_at_decode_not_at_parse() {
    let invalid = serde_json::json!({
        "type": REGION_POINTER_MOTION,
        "region_generation": 0,
        "region_id": 1,
        "logical_x": 0,
        "logical_y": 0,
        "sequence": 1,
    });
    let parsed = RegionInputWireMessage::from_json_value(&invalid).unwrap();
    assert_eq!(
        parsed.decode(),
        Err(RegionInputDecodeError::Wire(
            RegionInputValidationError::ZeroRegionGeneration
        ))
    );
    assert!(matches!(
        RegionInputWireMessage::decode_json_value(&invalid),
        Err(RegionInputWireError::Decode(RegionInputDecodeError::Wire(
            RegionInputValidationError::ZeroRegionGeneration
        )))
    ));
}

#[test]
fn zero_generation_zero_region_and_zero_sequence_are_rejected_for_every_message() {
    let mut zero_generation = motion_msg();
    zero_generation.position.region_generation = 0;
    let mut zero_region = motion_msg();
    zero_region.position.region_id = 0;
    let mut zero_sequence = motion_msg();
    zero_sequence.metadata.sequence = 0;

    for (message, expected) in [
        (
            zero_generation,
            RegionInputValidationError::ZeroRegionGeneration,
        ),
        (zero_region, RegionInputValidationError::ZeroRegionId),
        (zero_sequence, RegionInputValidationError::ZeroSequence),
    ] {
        assert_eq!(
            RegionInputWireRef::PointerMotion(&message).decode(),
            Err(RegionInputDecodeError::Wire(expected))
        );
        assert_eq!(
            RegionInputWireRef::PointerMotion(&message).validate(),
            Err(expected)
        );
    }
}

#[test]
fn every_message_kind_rejects_the_same_zero_region_identity() {
    let position = RegionInputPositionMsg {
        region_id: 0,
        ..valid_position_msg()
    };
    let metadata = valid_metadata_msg();
    let enter = RegionPointerEnterMsg {
        msg_type: REGION_POINTER_ENTER.to_owned(),
        position,
        metadata,
    };
    let leave = RegionPointerLeaveMsg {
        msg_type: REGION_POINTER_LEAVE.to_owned(),
        position,
        metadata,
    };
    let motion = RegionPointerMotionMsg {
        msg_type: REGION_POINTER_MOTION.to_owned(),
        position,
        metadata,
    };
    let button = RegionPointerButtonMsg {
        msg_type: REGION_POINTER_BUTTON.to_owned(),
        position,
        button: 1,
        pressed: true,
        metadata,
    };
    let scroll = RegionPointerScrollMsg {
        msg_type: REGION_POINTER_SCROLL.to_owned(),
        position,
        delta_x: 0,
        delta_y: 0,
        metadata,
    };
    let pen = RegionPenEventMsg {
        position,
        ..pen_msg()
    };

    for message in [
        RegionInputWireRef::PointerEnter(&enter),
        RegionInputWireRef::PointerLeave(&leave),
        RegionInputWireRef::PointerMotion(&motion),
        RegionInputWireRef::PointerButton(&button),
        RegionInputWireRef::PointerScroll(&scroll),
        RegionInputWireRef::Pen(&pen),
    ] {
        assert_eq!(
            message.decode(),
            Err(RegionInputDecodeError::Wire(
                RegionInputValidationError::ZeroRegionId
            ))
        );
    }
}

#[test]
fn zero_button_is_rejected_before_any_domain_conversion() {
    let message = RegionPointerButtonMsg {
        msg_type: REGION_POINTER_BUTTON.to_owned(),
        position: valid_position_msg(),
        button: 0,
        pressed: true,
        metadata: valid_metadata_msg(),
    };
    assert_eq!(
        RegionInputWireRef::PointerButton(&message).decode(),
        Err(RegionInputDecodeError::Wire(
            RegionInputValidationError::ZeroButton
        ))
    );
}

#[test]
fn out_of_range_pen_fields_are_rejected_field_by_field() {
    for (mutate, field) in [
        (
            (|message: &mut RegionPenEventMsg| message.pressure = 1.5)
                as fn(&mut RegionPenEventMsg),
            "pressure",
        ),
        (|message| message.pressure = f32::NAN, "pressure"),
        (|message| message.tilt_x_degrees = -91.0, "tilt_x_degrees"),
        (|message| message.tilt_y_degrees = 90.5, "tilt_y_degrees"),
        (
            |message| message.rotation_degrees = 361.0,
            "rotation_degrees",
        ),
        (
            |message| message.rotation_degrees = f32::INFINITY,
            "rotation_degrees",
        ),
    ] {
        let mut message = pen_msg();
        mutate(&mut message);
        assert_eq!(
            RegionInputWireRef::Pen(&message).decode(),
            Err(RegionInputDecodeError::Wire(
                RegionInputValidationError::PenFieldOutOfRange(field)
            ))
        );
    }
}

#[test]
fn pen_tool_maps_in_both_directions_for_every_tool() {
    for (domain, wire) in [
        (PenTool::Tip, PenToolMsg::Tip),
        (PenTool::Eraser, PenToolMsg::Eraser),
    ] {
        assert_eq!(domain_pen_tool(wire), domain);
        assert_eq!(wire_pen_tool(domain), wire);
        assert_eq!(domain_pen_tool(wire_pen_tool(domain)), domain);
    }

    let eraser = pen_msg();
    let RegionInputEvent::Pen { sample, .. } = RegionInputWireRef::Pen(&eraser).decode().unwrap()
    else {
        panic!("a pen DTO decodes to a pen event");
    };
    assert_eq!(sample.tool, PenTool::Eraser);
}

#[test]
fn domain_conversion_helpers_reject_zero_identities_and_round_trip() {
    assert_eq!(
        domain_generation(0),
        Err(RegionContractError::ZeroGeneration)
    );
    assert_eq!(domain_generation(7), Ok(generation()));

    let zero_region = RegionInputPositionMsg {
        region_id: 0,
        ..valid_position_msg()
    };
    assert_eq!(
        domain_position(zero_region),
        Err(RegionContractError::ZeroRegionId)
    );

    let decoded = domain_position(valid_position_msg()).unwrap();
    assert_eq!(decoded, position(60, 1_440));
    assert_eq!(wire_position(generation(), decoded), valid_position_msg());
}
