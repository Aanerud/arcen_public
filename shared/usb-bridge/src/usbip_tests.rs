// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use super::*;

fn submit_header(
    seqnum: u32,
    direction: u32,
    endpoint: u32,
    length: u32,
    packets: u32,
    setup: [u8; 8],
) -> [u8; USBIP_HEADER_BYTES] {
    let mut header = [0_u8; USBIP_HEADER_BYTES];
    put_be(&mut header, 0, CMD_SUBMIT);
    put_be(&mut header, 4, seqnum);
    put_be(&mut header, 8, 0x0001_0002);
    put_be(&mut header, 12, direction);
    put_be(&mut header, 16, endpoint);
    put_be(&mut header, 24, length);
    put_be(&mut header, 32, packets);
    put_be(&mut header, 36, 4);
    header[40..48].copy_from_slice(&setup);
    header
}

#[test]
fn a_get_descriptor_submit_decodes_with_its_setup_packet() {
    let header = submit_header(
        7,
        DIRECTION_IN,
        0,
        18,
        0,
        [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00],
    );
    let UsbipCommand::Submit(submit) = decode_command(&header).unwrap() else {
        panic!("not a submit");
    };
    assert_eq!(submit.seqnum, 7);
    assert_eq!(submit.endpoint_address, 0x80);
    assert_eq!(submit.direction, TransferDirection::In);
    assert_eq!(submit.transfer_kind(), TransferKind::Control);
    assert_eq!(submit.length, 18);
    assert_eq!(submit.out_payload_len(), 0);
    assert_eq!(submit.interval, 4);
    assert_eq!(
        submit.setup,
        SetupPacket {
            request_type: 0x80,
            request: 0x06,
            value: 0x0100,
            index: 0,
            length: 18,
        }
    );
}

#[test]
fn an_interrupt_out_submit_announces_its_payload() {
    let header = submit_header(9, DIRECTION_OUT, 2, 64, u32::MAX, [0; 8]);
    let UsbipCommand::Submit(submit) = decode_command(&header).unwrap() else {
        panic!("not a submit");
    };
    assert_eq!(submit.endpoint_address, 0x02);
    assert_eq!(submit.transfer_kind(), TransferKind::Interrupt);
    assert_eq!(submit.out_payload_len(), 64);
}

#[test]
fn malformed_headers_are_refused_before_any_payload_is_read() {
    let mut unknown = submit_header(1, DIRECTION_IN, 1, 8, 0, [0; 8]);
    put_be(&mut unknown, 0, RET_SUBMIT);
    assert_eq!(
        decode_command(&unknown),
        Err(UsbipError::UnknownCommand(RET_SUBMIT))
    );
    assert_eq!(
        decode_command(&submit_header(1, 2, 1, 8, 0, [0; 8])),
        Err(UsbipError::InvalidDirection(2))
    );
    assert_eq!(
        decode_command(&submit_header(1, DIRECTION_IN, 16, 8, 0, [0; 8])),
        Err(UsbipError::InvalidEndpoint(16))
    );
    let too_large = u32::try_from(MAX_TRANSFER_BYTES + 1).unwrap();
    assert_eq!(
        decode_command(&submit_header(1, DIRECTION_OUT, 1, too_large, 0, [0; 8])),
        Err(UsbipError::TransferTooLarge(too_large))
    );
    assert_eq!(
        decode_command(&submit_header(1, DIRECTION_OUT, 1, u32::MAX, 0, [0; 8])),
        Err(UsbipError::TransferTooLarge(u32::MAX))
    );
    assert_eq!(
        decode_command(&submit_header(1, DIRECTION_IN, 1, 8, 3, [0; 8])),
        Err(UsbipError::Isochronous(3))
    );
}

#[test]
fn an_unlink_names_its_target() {
    let mut header = [0_u8; USBIP_HEADER_BYTES];
    put_be(&mut header, 0, CMD_UNLINK);
    put_be(&mut header, 4, 12);
    put_be(&mut header, 20, 11);
    assert_eq!(
        decode_command(&header),
        Ok(UsbipCommand::Unlink {
            seqnum: 12,
            target: 11
        })
    );
}

#[test]
fn replies_are_big_endian_and_carry_the_payload() {
    let pdu = encode_ret_submit(7, -32, &[1, 2, 3]).unwrap();
    assert_eq!(pdu.len(), USBIP_HEADER_BYTES + 3);
    assert_eq!(&pdu[0..4], &[0, 0, 0, 3]);
    assert_eq!(&pdu[4..8], &[0, 0, 0, 7]);
    assert_eq!(&pdu[8..20], &[0; 12]);
    assert_eq!(&pdu[20..24], &(-32_i32).to_be_bytes());
    assert_eq!(&pdu[24..28], &[0, 0, 0, 3]);
    assert_eq!(&pdu[48..], &[1, 2, 3]);
    assert!(encode_ret_submit(1, 0, &vec![0; MAX_TRANSFER_BYTES + 1]).is_err());

    let unlink = encode_ret_unlink(12, -104);
    assert_eq!(&unlink[0..4], &[0, 0, 0, 4]);
    assert_eq!(&unlink[4..8], &[0, 0, 0, 12]);
    assert_eq!(&unlink[20..24], &(-104_i32).to_be_bytes());
    assert_eq!(&unlink[24..], &[0; 24]);
}

fn submit(seqnum: u32) -> UsbipSubmit {
    UsbipSubmit {
        seqnum,
        endpoint_address: 0x81,
        direction: TransferDirection::In,
        length: 8,
        interval: 1,
        setup: SetupPacket {
            request_type: 0,
            request: 0,
            value: 0,
            index: 0,
            length: 0,
        },
    }
}

#[test]
fn a_reused_in_flight_seqnum_is_refused() {
    let mut state = UsbipServerState::default();
    state.submit(&submit(1)).unwrap();
    assert_eq!(
        state.submit(&submit(1)),
        Err(UsbipError::DuplicateSeqnum(1))
    );
    assert!(state.reply(1, 0, 0, &[1]).unwrap().is_some());
    assert!(state.reply(1, 0, 0, &[1]).unwrap().is_none());
    state.submit(&submit(1)).unwrap();
}

#[test]
fn an_unlinked_urb_is_never_answered_and_a_late_unlink_is_acknowledged() {
    let mut state = UsbipServerState::default();
    state.submit(&submit(5)).unwrap();
    assert_eq!(state.unlink(5), UnlinkAction::Cancel);
    // The Deck's completion races the unlink: it must be dropped.
    assert_eq!(state.reply(5, 0, 0, &[1, 2]), Ok(None));
    state.submit(&submit(6)).unwrap();
    assert!(state.reply(6, 0, 0, &[1, 2]).unwrap().is_some());
    // Unlink after completion: status 0, nothing else.
    assert_eq!(state.unlink(6), UnlinkAction::AlreadyDone);
    assert_eq!(state.unlink(99), UnlinkAction::AlreadyDone);
    assert_eq!(state.in_flight(), 0);
}

#[test]
fn a_reply_never_exceeds_the_kernels_buffer() {
    let mut state = UsbipServerState::default();
    state.submit(&submit(3)).unwrap();
    assert_eq!(
        state.reply(3, 0, 0, &[0; 9]),
        Err(UsbipError::ReplyTooLong {
            requested: 8,
            actual: 9
        })
    );
    // Still in flight: the caller fails it instead.
    let pdu = state.reply(3, -71, 0, &[]).unwrap().unwrap();
    assert_eq!(&pdu[24..28], &[0; 4]);

    let mut out = submit(4);
    out.direction = TransferDirection::Out;
    out.endpoint_address = 0x02;
    state.submit(&out).unwrap();
    // OUT: the actual length is reported, no payload follows.
    let pdu = state.reply(4, 0, 8, &[0xAA; 8]).unwrap().unwrap();
    assert_eq!(pdu.len(), USBIP_HEADER_BYTES);
    assert_eq!(&pdu[24..28], &[0, 0, 0, 8]);
    state
        .submit(&{
            let mut again = out;
            again.seqnum = 5;
            again
        })
        .unwrap();
    assert!(state.reply(5, 0, 9, &[]).is_err());
}

#[test]
fn in_flight_urbs_are_bounded() {
    let mut state = UsbipServerState::default();
    for seqnum in 0..u32::try_from(MAX_IN_FLIGHT_URBS).unwrap() {
        state.submit(&submit(seqnum)).unwrap();
    }
    assert_eq!(
        state.submit(&submit(1_000)),
        Err(UsbipError::TooManyInFlight)
    );
}

const EL10_STATUS: &str = "hub port sta spd dev      sockfd local_busid
hs  0000 006 002 00010002 000005 1-1
hs  0001 004 000 00000000 000000 0-0
hs  0002 004 000 00000000 000000 0-0
hs  0003 004 000 00000000 000000 0-0
hs  0004 004 000 00000000 000000 0-0
hs  0005 004 000 00000000 000000 0-0
hs  0006 004 000 00000000 000000 0-0
hs  0007 004 000 00000000 000000 0-0
ss  0008 004 000 00000000 000000 0-0
ss  0009 004 000 00000000 000000 0-0
ss  0010 004 000 00000000 000000 0-0
ss  0011 004 000 00000000 000000 0-0
ss  0012 004 000 00000000 000000 0-0
ss  0013 004 000 00000000 000000 0-0
ss  0014 004 000 00000000 000000 0-0
ss  0015 004 000 00000000 000000 0-0
";

#[test]
fn the_status_table_yields_free_usb2_ports_and_their_root_index() {
    let status = VhciStatus::parse(EL10_STATUS);
    assert_eq!(status.rows().len(), 16);
    assert!(!status.row(0).unwrap().is_free());
    assert_eq!(
        status.free_high_speed_ports().collect::<Vec<_>>(),
        (1..8).collect::<Vec<_>>()
    );
    assert_eq!(status.high_speed_root_index(0), Some(1));
    assert_eq!(status.high_speed_root_index(3), Some(4));
    assert_eq!(status.high_speed_root_index(8), None);
    assert_eq!(status.high_speed_root_index(42), None);
}

#[test]
fn a_full_controller_has_no_free_port_and_garbage_is_skipped() {
    let busy = EL10_STATUS.replace(" 004 ", " 006 ");
    assert_eq!(VhciStatus::parse(&busy).free_high_speed_ports().count(), 0);
    let garbage = "hub port\nxx 0001 004\nhs zz 004\nhs 0001\n";
    assert!(VhciStatus::parse(garbage).rows().is_empty());
    let huge = "hs 0001 004 000 0 0 0-0\n".repeat(1_000);
    assert_eq!(VhciStatus::parse(&huge).rows().len(), MAX_VHCI_STATUS_ROWS);
}

#[test]
fn a_port_prefix_matches_its_own_subtree_only() {
    let prefix = port_devpath_prefix("vhci_hcd.0", 3, 1);
    assert_eq!(prefix, "/devices/platform/vhci_hcd.0/usb3/3-1");
    assert!(devpath_under(
        "/devices/platform/vhci_hcd.0/usb3/3-1",
        &prefix
    ));
    assert!(devpath_under(
        "/devices/platform/vhci_hcd.0/usb3/3-1/3-1:1.0/0003:056A:0317.0001/input/input9/event9",
        &prefix
    ));
    assert!(!devpath_under(
        "/devices/platform/vhci_hcd.0/usb3/3-10/3-10:1.0",
        &prefix
    ));
    assert!(!devpath_under(
        "/devices/platform/vhci_hcd.0/usb3/3-2",
        &prefix
    ));
    assert!(!devpath_under("/devices/pci0000:00/usb1/1-1", &prefix));
}
