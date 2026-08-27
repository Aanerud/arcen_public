use crate::{AlternateSetting, InterfaceNumber, ParsedConfiguration, UsbDeviceId};
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

/// Exact expected interface facts for one allowed profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceRule {
    pub number: InterfaceNumber,
    pub alternate_setting: AlternateSetting,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// Exact host-authorized device profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub minimum_bcd_device: u16,
    pub maximum_bcd_device: u16,
    pub interfaces: Vec<InterfaceRule>,
}

/// Device facts supplied to host-side policy after descriptor parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub id: UsbDeviceId,
    pub device_class: u8,
    pub configuration: ParsedConfiguration,
}

/// Successful policy selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    pub profile_name: String,
    pub configuration_value: u8,
}

/// Closed denial reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDenial {
    Disabled,
    IdentityMismatch,
    DeviceRevisionMismatch,
    ProhibitedDeviceClass(u8),
    ProhibitedInterfaceClass(u8),
    InterfaceCountMismatch,
    UnexpectedInterface {
        number: InterfaceNumber,
        alternate_setting: AlternateSetting,
    },
    MissingInterface {
        number: InterfaceNumber,
        alternate_setting: AlternateSetting,
    },
}

impl Display for PolicyDenial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("USB input bridge is disabled"),
            Self::IdentityMismatch => formatter.write_str("device identity does not match profile"),
            Self::DeviceRevisionMismatch => {
                formatter.write_str("device revision is outside profile range")
            }
            Self::ProhibitedDeviceClass(class) => {
                write!(formatter, "device class {class:#04x} is prohibited")
            }
            Self::ProhibitedInterfaceClass(class) => {
                write!(formatter, "interface class {class:#04x} is prohibited")
            }
            Self::InterfaceCountMismatch => {
                formatter.write_str("interface count does not match profile")
            }
            Self::UnexpectedInterface {
                number,
                alternate_setting,
            } => write!(
                formatter,
                "unexpected interface {} alternate {}",
                number.0, alternate_setting.0
            ),
            Self::MissingInterface {
                number,
                alternate_setting,
            } => write!(
                formatter,
                "missing interface {} alternate {}",
                number.0, alternate_setting.0
            ),
        }
    }
}

impl std::error::Error for PolicyDenial {}

/// Returns whether a USB class is permanently outside the input-only product
/// boundary.
#[must_use]
pub const fn is_permanently_prohibited_class(class: u8) -> bool {
    matches!(
        class,
        0x01 | 0x02 | 0x06 | 0x07 | 0x08 | 0x09 | 0x0a | 0x0b | 0x0e | 0xdc | 0xe0 | 0xfe
    )
}

/// Evaluates one exact profile against independently parsed device facts.
///
/// # Errors
///
/// Returns [`PolicyDenial`] when bridging is disabled or an exact identity,
/// revision, class, interface, or alternate-setting invariant fails.
pub fn evaluate_profile(
    enabled: bool,
    profile: &DeviceProfile,
    snapshot: &DeviceSnapshot,
) -> Result<PolicyDecision, PolicyDenial> {
    if !enabled {
        return Err(PolicyDenial::Disabled);
    }
    if snapshot.id.vendor_id != profile.vendor_id || snapshot.id.product_id != profile.product_id {
        return Err(PolicyDenial::IdentityMismatch);
    }
    if !(profile.minimum_bcd_device..=profile.maximum_bcd_device).contains(&snapshot.id.bcd_device)
    {
        return Err(PolicyDenial::DeviceRevisionMismatch);
    }
    if is_permanently_prohibited_class(snapshot.device_class) {
        return Err(PolicyDenial::ProhibitedDeviceClass(snapshot.device_class));
    }
    if snapshot.configuration.interfaces.len() != profile.interfaces.len() {
        return Err(PolicyDenial::InterfaceCountMismatch);
    }
    for interface in &snapshot.configuration.interfaces {
        if is_permanently_prohibited_class(interface.class) {
            return Err(PolicyDenial::ProhibitedInterfaceClass(interface.class));
        }
        let matched = profile.interfaces.iter().any(|rule| {
            rule.number == interface.number
                && rule.alternate_setting == interface.alternate_setting
                && rule.class == interface.class
                && rule.subclass == interface.subclass
                && rule.protocol == interface.protocol
        });
        if !matched {
            return Err(PolicyDenial::UnexpectedInterface {
                number: interface.number,
                alternate_setting: interface.alternate_setting,
            });
        }
    }
    for rule in &profile.interfaces {
        if !snapshot.configuration.interfaces.iter().any(|interface| {
            rule.number == interface.number
                && rule.alternate_setting == interface.alternate_setting
                && rule.class == interface.class
                && rule.subclass == interface.subclass
                && rule.protocol == interface.protocol
        }) {
            return Err(PolicyDenial::MissingInterface {
                number: rule.number,
                alternate_setting: rule.alternate_setting,
            });
        }
    }
    Ok(PolicyDecision {
        profile_name: profile.name.clone(),
        configuration_value: snapshot.configuration.configuration_value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EndpointAddress, EndpointDescriptor, InterfaceDescriptor, TransferKind};

    fn profile() -> DeviceProfile {
        DeviceProfile {
            name: "arcen-lab-tablet".to_owned(),
            vendor_id: 0xffff,
            product_id: 0xa2ce,
            minimum_bcd_device: 0x0100,
            maximum_bcd_device: 0x0100,
            interfaces: vec![InterfaceRule {
                number: InterfaceNumber(0),
                alternate_setting: AlternateSetting(0),
                class: 3,
                subclass: 0,
                protocol: 0,
            }],
        }
    }

    fn snapshot(class: u8) -> DeviceSnapshot {
        DeviceSnapshot {
            id: UsbDeviceId {
                vendor_id: 0xffff,
                product_id: 0xa2ce,
                bcd_device: 0x0100,
            },
            device_class: 0,
            configuration: ParsedConfiguration {
                configuration_value: 1,
                interfaces: vec![InterfaceDescriptor {
                    number: InterfaceNumber(0),
                    alternate_setting: AlternateSetting(0),
                    class,
                    subclass: 0,
                    protocol: 0,
                    endpoints: vec![EndpointDescriptor {
                        address: EndpointAddress(0x81),
                        transfer_kind: TransferKind::Interrupt,
                        max_packet_size: 16,
                        interval: 1,
                    }],
                }],
            },
        }
    }

    #[test]
    fn exact_hid_profile_is_accepted() {
        assert!(evaluate_profile(true, &profile(), &snapshot(3)).is_ok());
    }

    #[test]
    fn storage_composite_is_rejected() {
        assert_eq!(
            evaluate_profile(true, &profile(), &snapshot(8)),
            Err(PolicyDenial::ProhibitedInterfaceClass(8))
        );
    }

    #[test]
    fn vendor_specific_interface_is_not_a_wildcard() {
        assert_eq!(
            evaluate_profile(true, &profile(), &snapshot(0xff)),
            Err(PolicyDenial::UnexpectedInterface {
                number: InterfaceNumber(0),
                alternate_setting: AlternateSetting(0),
            })
        );
    }
}
