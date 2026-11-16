// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which reader a host that reads a bridged pen itself uses (Q-16, ADR 0023
//! amendment 2026-11-15): a dedicated reader where one exists, because it
//! adds what the descriptor does not say (the Intuos tool id, its eraser end,
//! its mode switch); otherwise the generic reader built from the pen
//! interface's own report descriptor. Only a pen that speaks a vendor format
//! no reader knows is refused, and the refusal says so.

use crate::{
    GenericPenLayout, GenericPenReader, GenericPenRefusal, HidReportKind, HostPenSample,
    INTUOS_MODE_SWITCH, INTUOS5_PEN_REPORT_ID, INTUOS5_PEN_REPORT_LENGTH,
    InterfaceReportDescriptor, IntuosPenModel, IntuosPenReader, UsbDeviceId,
    declared_report_lengths, generic_pen_layout, host_decoded_intuos_model,
};
use std::fmt::{Display, Formatter};

/// How a host reads one bridged tablet's pen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPenRoute {
    /// A model with a dedicated reader; its pen interface is found by its
    /// packet (`INTUOS5_PEN_REPORT_ID` at its length) once the device is
    /// enumerated.
    Dedicated(IntuosPenModel),
    /// Any other pen: read by its own report descriptor.
    Generic {
        /// The USB interface whose reports carry the pen.
        interface: u8,
        layout: GenericPenLayout,
    },
}

impl HostPenRoute {
    /// A short name for logs and the Deck: `dedicated` or `generic`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Dedicated(_) => "dedicated",
            Self::Generic { .. } => "generic",
        }
    }

    /// The interface a generic reader reads; `None` for a dedicated reader,
    /// whose interface is known only from the enumerated plan.
    #[must_use]
    pub const fn generic_interface(&self) -> Option<u8> {
        match self {
            Self::Dedicated(_) => None,
            Self::Generic { interface, .. } => Some(*interface),
        }
    }

    /// The USB interface a host reads this pen on, from the authorized
    /// report descriptors: the generic reader's own, or for a dedicated
    /// reader the interface that declares the model's pen packet (input
    /// report 2 at its length), found by its packet, not its number. `None`
    /// when no interface does.
    #[must_use]
    pub fn read_interface(&self, descriptors: &[InterfaceReportDescriptor]) -> Option<u8> {
        match self {
            Self::Generic { interface, .. } => Some(*interface),
            Self::Dedicated(_) => descriptors
                .iter()
                .find(|claim| {
                    declared_report_lengths(&claim.descriptor, HidReportKind::Input)
                        .get(&INTUOS5_PEN_REPORT_ID)
                        == Some(&INTUOS5_PEN_REPORT_LENGTH)
                })
                .map(|claim| claim.interface),
        }
    }

    /// A fresh reader for one attachment.
    #[must_use]
    pub fn reader(&self) -> HostPenReader {
        match self {
            Self::Dedicated(model) => HostPenReader::Intuos(IntuosPenReader::new(*model)),
            Self::Generic { layout, .. } => {
                HostPenReader::Generic(GenericPenReader::new(layout.clone()))
            }
        }
    }
}

/// Why no reader can read a tablet's pen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoHostPenRoute {
    pub vendor_id: u16,
    pub product_id: u16,
    /// What the last pen-declaring interface's descriptor gave.
    pub refusal: GenericPenRefusal,
}

impl Display for NoHostPenRoute {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.refusal {
            GenericPenRefusal::NoPenReport => write!(
                f,
                "this tablet ({:04x}:{:04x}) sends its pen in a vendor format this Pier has no \
                 reader for yet",
                self.vendor_id, self.product_id
            ),
            GenericPenRefusal::Malformed => write!(
                f,
                "this tablet's ({:04x}:{:04x}) HID report descriptor could not be read",
                self.vendor_id, self.product_id
            ),
        }
    }
}

impl std::error::Error for NoHostPenRoute {}

/// Chooses how a host reads `identity`'s pen from its authorized report
/// descriptors: the dedicated reader for a known model, else the first
/// interface whose descriptor declares a readable pen report.
///
/// # Errors
///
/// [`NoHostPenRoute`] when neither applies.
pub fn host_pen_route(
    identity: UsbDeviceId,
    descriptors: &[InterfaceReportDescriptor],
) -> Result<HostPenRoute, NoHostPenRoute> {
    if let Some(model) = host_decoded_intuos_model(identity.vendor_id, identity.product_id) {
        return Ok(HostPenRoute::Dedicated(model));
    }
    let mut refusal = GenericPenRefusal::NoPenReport;
    for claim in descriptors {
        match generic_pen_layout(&claim.descriptor) {
            Ok(layout) => {
                return Ok(HostPenRoute::Generic {
                    interface: claim.interface,
                    layout,
                });
            }
            Err(GenericPenRefusal::Malformed) => refusal = GenericPenRefusal::Malformed,
            Err(GenericPenRefusal::NoPenReport) => {}
        }
    }
    Err(NoHostPenRoute {
        vendor_id: identity.vendor_id,
        product_id: identity.product_id,
        refusal,
    })
}

/// One attachment's pen reader, dedicated or generic.
#[derive(Debug, Clone)]
pub enum HostPenReader {
    Intuos(IntuosPenReader),
    Generic(GenericPenReader),
}

impl HostPenReader {
    /// One input report of the pen interface.
    pub fn read(&mut self, report: &[u8]) -> Option<HostPenSample> {
        match self {
            Self::Intuos(reader) => reader.read(report),
            Self::Generic(reader) => reader.read(report),
        }
    }

    /// The exit of a pen that left by going quiet (a generic pen without In
    /// Range); call it periodically. A dedicated reader always sees its
    /// exit packet.
    pub fn expire(&mut self, now: std::time::Instant) -> Option<HostPenSample> {
        match self {
            Self::Intuos(_) => None,
            Self::Generic(reader) => reader.expire(now),
        }
    }

    /// Pen reports read, and reports that produced nothing.
    #[must_use]
    pub const fn counts(&self) -> (u64, u64) {
        match self {
            Self::Intuos(reader) => reader.counts(),
            Self::Generic(reader) => reader.counts(),
        }
    }

    /// The feature report that switches the pen into the mode this reader
    /// reads, when it needs one (the Intuos family's Wacom mode). A generic
    /// pen reports in its power-on mode.
    #[must_use]
    pub const fn mode_switch(&self) -> Option<(u8, [u8; 2])> {
        match self {
            Self::Intuos(_) => Some(INTUOS_MODE_SWITCH),
            Self::Generic(_) => None,
        }
    }

    /// `dedicated` or `generic`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Intuos(_) => "dedicated",
            Self::Generic(_) => "generic",
        }
    }
}
