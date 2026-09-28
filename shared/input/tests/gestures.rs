use arcen_input::{
    GestureEvent, GestureState, GestureStateError, GestureWireRef, ScrollPhase, coalesce_gestures,
};
use arcen_protocol::messages::{
    GestureMagnifyMsg, GestureSwipeMsg, GestureValidationError, ScrollPhaseMsg, SwipeDirectionMsg,
};

#[test]
fn gesture_wire_validates_and_decodes() {
    let magnify = GestureMagnifyMsg {
        scale_delta: 0.125,
        phase: ScrollPhaseMsg::Changed,
        sequence: 7,
        ..GestureMagnifyMsg::default()
    };
    assert_eq!(
        GestureWireRef::Magnify(&magnify).decode().unwrap(),
        GestureEvent::Magnify {
            scale_delta: 0.125,
            phase: ScrollPhase::Changed,
            sequence: 7,
        }
    );

    let bad = GestureMagnifyMsg {
        scale_delta: f64::NAN,
        sequence: 8,
        ..GestureMagnifyMsg::default()
    };
    assert!(matches!(
        GestureWireRef::Magnify(&bad).decode(),
        Err(GestureValidationError::FieldOutOfRange("scale_delta"))
    ));
}

#[test]
fn gesture_state_orders_globally_and_coalesces_motion_changes() {
    let mut state = GestureState::new();
    let first = GestureEvent::Magnify {
        scale_delta: 0.1,
        phase: ScrollPhase::Changed,
        sequence: 1,
    };
    state.apply(first).unwrap();
    assert_eq!(
        state.apply(first),
        Err(GestureStateError::InvalidSequence {
            last: 1,
            received: 1
        })
    );

    let changed = GestureEvent::Swipe {
        direction: SwipeDirectionMsg::Left,
        fingers: 3,
        phase: ScrollPhase::Changed,
        sequence: 2,
    };
    let newer = GestureEvent::Swipe {
        direction: SwipeDirectionMsg::Right,
        fingers: 3,
        phase: ScrollPhase::Changed,
        sequence: 3,
    };
    let ended = GestureEvent::Swipe {
        direction: SwipeDirectionMsg::Right,
        fingers: 3,
        phase: ScrollPhase::Ended,
        sequence: 4,
    };
    assert_eq!(
        coalesce_gestures(&[changed, newer, ended]),
        vec![newer, ended]
    );
}

#[test]
fn swipe_rejects_impossible_finger_counts() {
    let swipe = GestureSwipeMsg {
        direction: SwipeDirectionMsg::Left,
        fingers: 0,
        sequence: 1,
        ..GestureSwipeMsg::default()
    };
    assert_eq!(
        GestureWireRef::Swipe(&swipe).validate(),
        Err(GestureValidationError::FingerCountOutOfRange)
    );
}
