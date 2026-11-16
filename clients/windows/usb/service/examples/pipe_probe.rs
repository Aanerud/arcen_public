// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lab probe: connects to the component's pipe as a Deck would, reads the
//! hello and info, asks for the device descriptor, and waits for one
//! interrupt report from the pen endpoint. With `--hold`, it then keeps
//! the session open, re-arming the interrupt IN, and prints every frame
//! until the component ends the session (the lease-revocation proof).

#[cfg(windows)]
#[expect(clippy::too_many_lines, reason = "one linear lab script")]
fn main() -> Result<(), String> {
    use arcen_protocol::{UsbUrbSubmitHeader, decode_usb_urb_complete, encode_usb_urb_submit};
    use arcen_usb_bridge::component_ipc::{
        TAG_COMPLETE, TAG_HELLO, TAG_INFO, TAG_SUBMIT, decode_hello, info_backend, read_frame,
        write_frame,
    };
    use arcen_usb_bridge::{
        AttachmentGeneration, EndpointAddress, SetupPacket, TransferKind, UrbId,
    };
    use std::num::{NonZeroU32, NonZeroU64};
    use std::os::windows::fs::OpenOptionsExt;

    let started = std::time::Instant::now();
    // FILE_READ_DATA | FILE_WRITE_DATA | attributes | READ_CONTROL |
    // SYNCHRONIZE: what the pipe's DACL grants (not GENERIC_WRITE).
    let mut pipe = std::fs::OpenOptions::new()
        .access_mode(0x0012_018b)
        .open(arcen_usb_bridge::winusb_package::PIPE_NAME)
        .map_err(|error| format!("open pipe: {error}"))?;
    let (tag, hello) = read_frame(&mut pipe).map_err(|error| error.to_string())?;
    if tag != TAG_HELLO {
        return Err(format!(
            "expected hello, got tag {tag:#04x}: {}",
            String::from_utf8_lossy(&hello)
        ));
    }
    let hello = decode_hello(&hello).ok_or("malformed hello")?;
    println!(
        "hello: {:04x}:{:04x} bcd {:04x} speed {:?}, {} report descriptor(s)",
        hello.vendor_id,
        hello.product_id,
        hello.bcd_device,
        hello.speed,
        hello.report_descriptors.len()
    );
    let (tag, info) = read_frame(&mut pipe).map_err(|error| error.to_string())?;
    if tag == TAG_INFO {
        println!("info: backend={:?}", info_backend(&info));
    }
    let generation = AttachmentGeneration::new(NonZeroU64::new(1).ok_or("zero")?);
    let submit = |urb: u32, endpoint: u8, kind, length: u32, setup| {
        encode_usb_urb_submit(
            UsbUrbSubmitHeader {
                generation,
                urb_id: UrbId::new(NonZeroU32::new(urb).ok_or("zero")?),
                endpoint: EndpointAddress(endpoint),
                transfer_kind: kind,
                timeout_ms: 1000,
                declared_length: length,
                setup,
            },
            &[],
        )
        .map_err(|error| format!("{error:?}"))
    };
    let frame = submit(
        1,
        0x80,
        TransferKind::Control,
        18,
        Some(SetupPacket {
            request_type: 0x80,
            request: 6,
            value: 0x0100,
            index: 0,
            length: 18,
        }),
    )?;
    write_frame(&mut pipe, TAG_SUBMIT, &frame).map_err(|error| error.to_string())?;
    let frame = submit(2, 0x81, TransferKind::Interrupt, 64, None)?;
    write_frame(&mut pipe, TAG_SUBMIT, &frame).map_err(|error| error.to_string())?;
    let mut seen = 0;
    while seen < 2 {
        let (tag, body) = read_frame(&mut pipe).map_err(|error| error.to_string())?;
        if tag != TAG_COMPLETE {
            println!("frame tag {tag:#04x}: {}", String::from_utf8_lossy(&body));
            continue;
        }
        let (header, data) =
            decode_usb_urb_complete(&body).map_err(|error| format!("{error:?}"))?;
        println!(
            "+{} ms completion urb {} status {:?} {} byte(s): {:02x?}",
            started.elapsed().as_millis(),
            header.urb_id.get(),
            header.status,
            data.len(),
            &data[..data.len().min(18)]
        );
        seen += 1;
    }
    if std::env::args().any(|arg| arg == "--hold") {
        let mut urb = 3;
        write_frame(
            &mut pipe,
            TAG_SUBMIT,
            &submit(urb, 0x81, TransferKind::Interrupt, 64, None)?,
        )
        .map_err(|error| error.to_string())?;
        loop {
            match read_frame(&mut pipe) {
                Ok((TAG_COMPLETE, _)) => {
                    urb += 1;
                    if write_frame(
                        &mut pipe,
                        TAG_SUBMIT,
                        &submit(urb, 0x81, TransferKind::Interrupt, 64, None)?,
                    )
                    .is_err()
                    {
                        println!(
                            "+{} ms submit failed: the pipe is closed",
                            started.elapsed().as_millis()
                        );
                        break;
                    }
                }
                Ok((tag, body)) => println!(
                    "+{} ms frame tag {tag:#04x}: {}",
                    started.elapsed().as_millis(),
                    String::from_utf8_lossy(&body)
                ),
                Err(error) => {
                    println!(
                        "+{} ms session ended: {error}",
                        started.elapsed().as_millis()
                    );
                    break;
                }
            }
        }
        println!("held {} interrupt report(s)", urb - 3);
    }
    println!("probe done");
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
