use crate::{
    AlternateSetting, EndpointAddress, InterfaceNumber, MAX_CONFIGURATION_DESCRIPTOR_BYTES,
    MAX_ENDPOINTS, MAX_INTERFACES, TransferKind,
};
use std::fmt::{Display, Formatter};

const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_INTERFACE: u8 = 0x04;
const DESCRIPTOR_ENDPOINT: u8 = 0x05;

/// One parsed endpoint descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDescriptor {
    pub address: EndpointAddress,
    pub transfer_kind: TransferKind,
    pub max_packet_size: u16,
    pub interval: u8,
}

/// One parsed interface/alternate-setting descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub number: InterfaceNumber,
    pub alternate_setting: AlternateSetting,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointDescriptor>,
}

/// Bounded configuration facts derived from descriptor bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfiguration {
    pub configuration_value: u8,
    pub interfaces: Vec<InterfaceDescriptor>,
}

/// Configuration descriptor rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    TooLarge { actual: usize, maximum: usize },
    Truncated,
    InvalidLength { offset: usize, length: usize },
    MissingConfiguration,
    InvalidConfigurationHeader,
    InterfaceLimit,
    EndpointLimit,
    EndpointBeforeInterface,
    UnsupportedTransferType(u8),
    EndpointDirectionMismatch,
}

impl Display for DescriptorError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "descriptor is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::Truncated => formatter.write_str("configuration descriptor is truncated"),
            Self::InvalidLength { offset, length } => {
                write!(
                    formatter,
                    "descriptor at offset {offset} has invalid length {length}"
                )
            }
            Self::MissingConfiguration => {
                formatter.write_str("configuration descriptor is missing")
            }
            Self::InvalidConfigurationHeader => {
                formatter.write_str("configuration descriptor header is invalid")
            }
            Self::InterfaceLimit => formatter.write_str("interface count exceeds bridge limit"),
            Self::EndpointLimit => formatter.write_str("endpoint count exceeds bridge limit"),
            Self::EndpointBeforeInterface => {
                formatter.write_str("endpoint descriptor appears before an interface")
            }
            Self::UnsupportedTransferType(kind) => {
                write!(formatter, "endpoint transfer type {kind} is outside v1")
            }
            Self::EndpointDirectionMismatch => {
                formatter.write_str("control endpoint appeared in interface endpoint list")
            }
        }
    }
}

impl std::error::Error for DescriptorError {}

/// Parses a USB configuration descriptor with strict v1 limits.
///
/// Unknown class-specific descriptors are skipped by their declared length;
/// only the standard configuration, interface, and endpoint facts become
/// authority-bearing policy input.
///
/// # Errors
///
/// Returns [`DescriptorError`] when the bytes are malformed, exceed a bridge
/// limit, or request a transfer type outside input-only v1.
pub fn parse_configuration_descriptor(
    bytes: &[u8],
) -> Result<ParsedConfiguration, DescriptorError> {
    if bytes.len() > MAX_CONFIGURATION_DESCRIPTOR_BYTES {
        return Err(DescriptorError::TooLarge {
            actual: bytes.len(),
            maximum: MAX_CONFIGURATION_DESCRIPTOR_BYTES,
        });
    }
    if bytes.len() < 9 {
        return Err(DescriptorError::Truncated);
    }
    if bytes[0] != 9 || bytes[1] != DESCRIPTOR_CONFIGURATION {
        return Err(DescriptorError::InvalidConfigurationHeader);
    }
    let declared_total = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
    if declared_total < 9 || declared_total > bytes.len() {
        return Err(DescriptorError::Truncated);
    }

    let mut parsed = ParsedConfiguration {
        configuration_value: bytes[5],
        interfaces: Vec::new(),
    };
    let mut endpoint_count = 0_usize;
    let mut offset = 0_usize;
    while offset < declared_total {
        let Some(&length_byte) = bytes.get(offset) else {
            return Err(DescriptorError::Truncated);
        };
        let length = usize::from(length_byte);
        if length < 2 || offset.saturating_add(length) > declared_total {
            return Err(DescriptorError::InvalidLength { offset, length });
        }
        let descriptor_type = bytes[offset + 1];
        match descriptor_type {
            DESCRIPTOR_CONFIGURATION if offset == 0 => {}
            DESCRIPTOR_INTERFACE => {
                if length < 9 {
                    return Err(DescriptorError::InvalidLength { offset, length });
                }
                if parsed.interfaces.len() >= MAX_INTERFACES {
                    return Err(DescriptorError::InterfaceLimit);
                }
                parsed.interfaces.push(InterfaceDescriptor {
                    number: InterfaceNumber(bytes[offset + 2]),
                    alternate_setting: AlternateSetting(bytes[offset + 3]),
                    class: bytes[offset + 5],
                    subclass: bytes[offset + 6],
                    protocol: bytes[offset + 7],
                    endpoints: Vec::new(),
                });
            }
            DESCRIPTOR_ENDPOINT => {
                if length < 7 {
                    return Err(DescriptorError::InvalidLength { offset, length });
                }
                endpoint_count = endpoint_count.saturating_add(1);
                if endpoint_count > MAX_ENDPOINTS {
                    return Err(DescriptorError::EndpointLimit);
                }
                let Some(interface) = parsed.interfaces.last_mut() else {
                    return Err(DescriptorError::EndpointBeforeInterface);
                };
                let address = EndpointAddress(bytes[offset + 2]);
                if address.number() == 0 {
                    return Err(DescriptorError::EndpointDirectionMismatch);
                }
                let attributes = bytes[offset + 3] & 0x03;
                let transfer_kind = match attributes {
                    0x03 => TransferKind::Interrupt,
                    other => return Err(DescriptorError::UnsupportedTransferType(other)),
                };
                interface.endpoints.push(EndpointDescriptor {
                    address,
                    transfer_kind,
                    max_packet_size: u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]),
                    interval: bytes[offset + 6],
                });
            }
            _ => {}
        }
        offset += length;
    }

    if parsed.interfaces.is_empty() {
        return Err(DescriptorError::MissingConfiguration);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_hid_interrupt_interface() {
        let descriptor = [
            9, 2, 25, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 0, 0, 0, // interface
            7, 5, 0x81, 3, 64, 0, 1, // interrupt IN
        ];
        let parsed = parse_configuration_descriptor(&descriptor).unwrap();
        assert_eq!(parsed.configuration_value, 1);
        assert_eq!(parsed.interfaces.len(), 1);
        assert_eq!(parsed.interfaces[0].class, 3);
        assert_eq!(
            parsed.interfaces[0].endpoints[0].address,
            EndpointAddress(0x81)
        );
    }

    #[test]
    fn rejects_bulk_endpoints() {
        let descriptor = [
            9, 2, 25, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 0, 0, 0, 7, 5, 0x81, 2, 64, 0, 1,
        ];
        assert_eq!(
            parse_configuration_descriptor(&descriptor),
            Err(DescriptorError::UnsupportedTransferType(2))
        );
    }
}
