// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The device state a host driver set, replayed to a re-captured device.
//!
//! A bridged device can outlive one client attachment (a parked bridge): the
//! host kernel keeps the device it enumerated, while the client side
//! re-captures the physical device for each attachment, which may reset it
//! (configuration, alternate settings, a vendor mode switch such as Wacom's
//! `SET_REPORT(Feature 2)`). This records every state-setting control OUT
//! request that completed successfully (standard `SET_CONFIGURATION` and
//! `SET_INTERFACE`, every class and vendor OUT request), keeping the latest
//! payload per request in first-seen order, so the host adapter can replay
//! them to the next attachment before the device serves anything else.

use crate::{SetupPacket, UrbId};
use std::collections::BTreeMap;

/// Most distinct requests kept.
pub const MAX_REPLAYED_REQUESTS: usize = 32;
/// Largest payload kept for one request.
pub const MAX_REPLAYED_PAYLOAD: usize = 256;

const REQUEST_SET_CONFIGURATION: u8 = 0x09;
const REQUEST_SET_INTERFACE: u8 = 0x0B;

/// Whether `setup` sets device state worth replaying.
#[must_use]
pub const fn is_state_setting(setup: SetupPacket) -> bool {
    let out = setup.request_type & 0x80 == 0;
    let kind = (setup.request_type >> 5) & 0x03;
    out && match kind {
        0 => {
            matches!(
                setup.request,
                REQUEST_SET_CONFIGURATION | REQUEST_SET_INTERFACE
            )
        }
        1 | 2 => true,
        _ => false,
    }
}

/// The recorded state-setting requests of one bridged device.
#[derive(Debug, Default, Clone)]
pub struct DeviceStateReplay {
    pending: BTreeMap<UrbId, (SetupPacket, Vec<u8>)>,
    log: Vec<(SetupPacket, Vec<u8>)>,
}

impl DeviceStateReplay {
    /// A control submit on its way to the device.
    pub fn observe_submit(&mut self, urb_id: UrbId, setup: SetupPacket, payload: &[u8]) {
        if is_state_setting(setup)
            && payload.len() <= MAX_REPLAYED_PAYLOAD
            && self.pending.len() < crate::MAX_IN_FLIGHT_URBS
        {
            self.pending.insert(urb_id, (setup, payload.to_vec()));
        }
    }

    /// A completion; a successful state-setting request is recorded.
    pub fn observe_completion(&mut self, urb_id: UrbId, success: bool) {
        let Some((setup, payload)) = self.pending.remove(&urb_id) else {
            return;
        };
        if !success {
            return;
        }
        let key = |candidate: &SetupPacket| {
            (
                candidate.request_type,
                candidate.request,
                if candidate.request == REQUEST_SET_CONFIGURATION && candidate.request_type == 0 {
                    0
                } else {
                    candidate.value
                },
                candidate.index,
            )
        };
        if let Some(entry) = self
            .log
            .iter_mut()
            .find(|(known, _)| key(known) == key(&setup))
        {
            *entry = (setup, payload);
        } else if self.log.len() < MAX_REPLAYED_REQUESTS {
            self.log.push((setup, payload));
        }
    }

    /// A request that ended without a completion (cancelled, attachment
    /// gone) is forgotten.
    pub fn forget(&mut self, urb_id: UrbId) {
        self.pending.remove(&urb_id);
    }

    /// The requests to replay, in first-seen order.
    #[must_use]
    pub fn requests(&self) -> &[(SetupPacket, Vec<u8>)] {
        &self.log
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn urb(id: u32) -> UrbId {
        UrbId::new(NonZeroU32::new(id).unwrap())
    }

    const fn setup(
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> SetupPacket {
        SetupPacket {
            request_type,
            request,
            value,
            index,
            length,
        }
    }

    #[test]
    fn only_successful_state_setting_out_requests_are_kept_latest_first_seen_order() {
        let mut replay = DeviceStateReplay::default();
        // SET_CONFIGURATION 1, then Wacom's mode switch, SET_IDLE, an LED
        // report, then the mode switch again with another value.
        replay.observe_submit(urb(1), setup(0x00, 0x09, 1, 0, 0), &[]);
        replay.observe_submit(urb(2), setup(0x21, 0x09, 0x0302, 0, 2), &[2, 2]);
        replay.observe_submit(urb(3), setup(0x21, 0x0A, 0, 0, 0), &[]);
        replay.observe_submit(urb(4), setup(0x21, 0x09, 0x0320, 0, 9), &[0x20; 9]);
        // Reads and standard requests other than configuration are ignored.
        replay.observe_submit(urb(5), setup(0xA1, 0x01, 0x0302, 0, 2), &[]);
        replay.observe_submit(urb(6), setup(0x80, 0x06, 0x0100, 0, 18), &[]);
        replay.observe_submit(urb(7), setup(0x00, 0x05, 3, 0, 0), &[]);
        for id in 1..=7 {
            replay.observe_completion(urb(id), id != 3);
        }
        replay.observe_submit(urb(8), setup(0x21, 0x09, 0x0302, 0, 2), &[2, 4]);
        replay.observe_completion(urb(8), true);
        let requests = replay.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert_eq!(requests[0].0.request, 0x09);
        assert_eq!(requests[0].0.request_type, 0x00);
        assert_eq!(
            requests[1].1,
            vec![2, 4],
            "the latest mode switch, first-seen place"
        );
        assert_eq!(requests[2].0.value, 0x0320);
        // A cancelled request is never recorded.
        replay.observe_submit(urb(9), setup(0x41, 0x01, 0, 0, 0), &[]);
        replay.forget(urb(9));
        replay.observe_completion(urb(9), true);
        assert_eq!(replay.requests().len(), 3);
    }

    #[test]
    fn the_log_and_payloads_are_bounded() {
        let mut replay = DeviceStateReplay::default();
        replay.observe_submit(
            urb(1),
            setup(0x21, 0x09, 1, 0, 0),
            &[0; MAX_REPLAYED_PAYLOAD + 1],
        );
        replay.observe_completion(urb(1), true);
        assert!(replay.requests().is_empty());
        for value in 0..40_u16 {
            let id = 10 + u32::from(value);
            replay.observe_submit(urb(id), setup(0x41, 0x01, value, 0, 0), &[]);
            replay.observe_completion(urb(id), true);
        }
        assert_eq!(replay.requests().len(), MAX_REPLAYED_REQUESTS);
    }
}
