use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::num::{NonZeroU32, NonZeroU64};

/// One immutable attachment generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachmentGeneration(NonZeroU64);

impl AttachmentGeneration {
    /// Creates a nonzero attachment generation.
    #[must_use]
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    /// Returns the numeric generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl Display for AttachmentGeneration {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

/// Nonzero request identity unique within one attachment generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UrbId(NonZeroU32);

impl UrbId {
    /// Creates a nonzero URB identifier.
    #[must_use]
    pub const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    /// Returns the numeric identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Exact USB vendor/product identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbDeviceId {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
}

/// USB interface number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InterfaceNumber(pub u8);

/// USB interface alternate setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AlternateSetting(pub u8);

/// USB endpoint address, including the direction bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointAddress(pub u8);

impl EndpointAddress {
    /// Returns the transfer direction encoded in bit 7.
    #[must_use]
    pub const fn direction(self) -> TransferDirection {
        if self.0 & 0x80 == 0 {
            TransferDirection::Out
        } else {
            TransferDirection::In
        }
    }

    /// Returns the endpoint number without the direction bit.
    #[must_use]
    pub const fn number(self) -> u8 {
        self.0 & 0x0f
    }
}

/// Direction of a USB transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    In,
    Out,
}

/// Transfer kind supported by the input-only v1 bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    Control,
    Interrupt,
}

/// Negotiated device speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbSpeed {
    Low,
    Full,
    High,
}

/// Closed bridge completion status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrbStatus {
    Success,
    Cancelled,
    TimedOut,
    Stall,
    Disconnected,
    Protocol,
    Io,
}

/// Standard eight-byte USB setup packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    /// Returns the requested descriptor type for `GET_DESCRIPTOR`.
    #[must_use]
    pub const fn descriptor_type(self) -> u8 {
        (self.value >> 8) as u8
    }

    /// Returns the requested descriptor index for `GET_DESCRIPTOR`.
    #[must_use]
    pub const fn descriptor_index(self) -> u8 {
        self.value.to_le_bytes()[0]
    }
}
