// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A pen's barrel buttons, by what they do.
//!
//! The wire carries what a barrel button does, not where it sits on the pen
//! (`PenEventMsg::buttons`: bit 0 the right click, bit 1 the middle click),
//! so every host clicks the same button for the same press. A Deck decides
//! the meaning from two sources:
//!
//! - **The tablet driver's own decision.** A Windows Ink barrel button is
//!   the button the driver set to Right Click (Windows' definition of the
//!   barrel), and a click the driver performs as a mouse click while the pen
//!   is in range (a Wacom side switch set to Middle Click or Right Click)
//!   is that click. These are honoured as they arrive.
//! - **The Deck's pen settings**, for buttons the operating system reports
//!   by position only (`-[NSEvent buttonMask]`'s lower and upper side
//!   switch on a Mac), and to remap or switch off the Windows Ink barrel.
//!
//! Hosts that predate the meaning ([`HostPenButtons::LinuxLegacy`]) are
//! handled here too, so a new Deck clicks the right button on an old Pier.

use arcen_protocol::messages::{
    HostPlatform, InputCapabilitiesMsg, InputCapabilityAvailability, PEN_BUTTON_MIDDLE_CLICK,
    PEN_BUTTON_RIGHT_CLICK,
};

/// What a barrel button does on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PenButtonAction {
    /// The desktop's right click (context menus).
    RightClick,
    /// The desktop's middle click.
    MiddleClick,
    /// Nothing reaches the host.
    Off,
}

impl PenButtonAction {
    /// Every action, in the order a settings list shows them.
    pub const ALL: [Self; 3] = [Self::RightClick, Self::MiddleClick, Self::Off];

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::RightClick => "right_click",
            Self::MiddleClick => "middle_click",
            Self::Off => "off",
        }
    }

    /// The action a settings token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "right_click" => Some(Self::RightClick),
            "middle_click" => Some(Self::MiddleClick),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The label a settings list shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RightClick => "Right click",
            Self::MiddleClick => "Middle click",
            Self::Off => "Off",
        }
    }

    /// The wire bit this action sets (`0` for [`Self::Off`]).
    #[must_use]
    pub const fn wire_bit(self) -> u16 {
        match self {
            Self::RightClick => PEN_BUTTON_RIGHT_CLICK,
            Self::MiddleClick => PEN_BUTTON_MIDDLE_CLICK,
            Self::Off => 0,
        }
    }
}

/// A click a tablet driver performs as a mouse click while its pen is in
/// range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverClick {
    /// A right click.
    Right,
    /// A middle click.
    Middle,
}

/// The Deck's pen-button settings.
///
/// Defaults: the Windows Ink barrel button and a pen's lower side switch
/// right-click, the upper side switch middle-clicks (lower = right, as the
/// macOS Pier always injected a pen's first button). A tablet maker's own
/// defaults differ by model (a Wacom pen's upper switch right-clicks and its
/// lower one pans); where its driver performs the click itself, that click
/// is sent as it is, and these settings only name buttons the operating
/// system reports by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PenButtonSettings {
    /// The Windows Ink barrel button (Windows Deck).
    pub barrel: PenButtonAction,
    /// The side switch nearer the tip (macOS Deck).
    pub lower: PenButtonAction,
    /// The side switch further from the tip (macOS Deck).
    pub upper: PenButtonAction,
}

impl Default for PenButtonSettings {
    fn default() -> Self {
        Self {
            barrel: PenButtonAction::RightClick,
            lower: PenButtonAction::RightClick,
            upper: PenButtonAction::MiddleClick,
        }
    }
}

/// The barrel-button state the Deck's operating system reports for one
/// sample, by source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent button sources")]
pub struct PenButtonsHeld {
    /// The Windows Ink barrel button (`PEN_FLAG_BARREL`, or the second
    /// button in contact).
    pub barrel: bool,
    /// The lower side switch, by position.
    pub lower: bool,
    /// The upper side switch, by position.
    pub upper: bool,
    /// A right click the tablet driver performs while the pen is in range.
    pub driver_right: bool,
    /// A middle click the tablet driver performs while the pen is in range.
    pub driver_middle: bool,
}

/// What the host does with [`arcen_protocol::messages::PenEventMsg::buttons`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostPenButtons {
    /// Bit 0 right click, bit 1 middle click (every host that says
    /// `pen_button_actions`, and macOS and Windows Piers before it).
    #[default]
    Actions,
    /// A Linux Pier before `pen_button_actions`: bit 0 is `BTN_STYLUS`
    /// (middle click on every Linux desktop) and bit 1 `BTN_STYLUS2` (right).
    LinuxLegacy,
}

impl HostPenButtons {
    /// The host's layout from its hello.
    #[must_use]
    pub fn for_host(platform: HostPlatform, capabilities: InputCapabilitiesMsg) -> Self {
        if platform == HostPlatform::Linux
            && capabilities.pen_button_actions != InputCapabilityAvailability::Available
        {
            Self::LinuxLegacy
        } else {
            Self::Actions
        }
    }

    /// The wire bits for `actions` (bit 0 right, bit 1 middle) on this host.
    #[must_use]
    pub const fn wire(self, actions: u16) -> u16 {
        match self {
            Self::Actions => actions,
            Self::LinuxLegacy => {
                let both = PEN_BUTTON_RIGHT_CLICK | PEN_BUTTON_MIDDLE_CLICK;
                let mut wire = actions & !both;
                if actions & PEN_BUTTON_RIGHT_CLICK != 0 {
                    wire |= PEN_BUTTON_MIDDLE_CLICK;
                }
                if actions & PEN_BUTTON_MIDDLE_CLICK != 0 {
                    wire |= PEN_BUTTON_RIGHT_CLICK;
                }
                wire
            }
        }
    }
}

/// A Deck's pen-button mapping for one session: its settings and its host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PenButtonMap {
    /// The Deck's settings.
    pub settings: PenButtonSettings,
    /// The host's layout.
    pub host: HostPenButtons,
}

impl PenButtonMap {
    /// The wire `buttons` for what is held. While the driver performs a
    /// click, the side switches' positions are not read: the driver has
    /// already decided what that switch does (a Mac's driver may report the
    /// switch's position bit alongside the right-mouse event it posts).
    #[must_use]
    pub const fn wire_buttons(self, held: PenButtonsHeld) -> u16 {
        let mut actions = 0;
        if held.barrel {
            actions |= self.settings.barrel.wire_bit();
        }
        let driver = held.driver_right || held.driver_middle;
        if held.lower && !driver {
            actions |= self.settings.lower.wire_bit();
        }
        if held.upper && !driver {
            actions |= self.settings.upper.wire_bit();
        }
        if held.driver_right {
            actions |= PEN_BUTTON_RIGHT_CLICK;
        }
        if held.driver_middle {
            actions |= PEN_BUTTON_MIDDLE_CLICK;
        }
        self.host.wire(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux(actions: InputCapabilityAvailability) -> HostPenButtons {
        HostPenButtons::for_host(
            HostPlatform::Linux,
            InputCapabilitiesMsg {
                pen_button_actions: actions,
                ..InputCapabilitiesMsg::default()
            },
        )
    }

    #[test]
    fn the_windows_ink_barrel_right_clicks_by_default_on_every_host() {
        let held = PenButtonsHeld {
            barrel: true,
            ..PenButtonsHeld::default()
        };
        let current = PenButtonMap::default();
        assert_eq!(current.wire_buttons(held), PEN_BUTTON_RIGHT_CLICK);
        // A Linux Pier from before the meaning clicks the right button for
        // BTN_STYLUS2, bit 1: the barrel must arrive there, not on BTN_STYLUS
        // (the middle click a field user's barrel became).
        let old_linux = PenButtonMap {
            host: linux(InputCapabilityAvailability::Unknown),
            ..current
        };
        assert_eq!(old_linux.wire_buttons(held), PEN_BUTTON_MIDDLE_CLICK);
        let new_linux = PenButtonMap {
            host: linux(InputCapabilityAvailability::Available),
            ..current
        };
        assert_eq!(new_linux.wire_buttons(held), PEN_BUTTON_RIGHT_CLICK);
    }

    #[test]
    fn only_a_linux_host_without_the_capability_is_legacy() {
        let unknown = InputCapabilitiesMsg::default();
        for platform in [
            HostPlatform::MacOs,
            HostPlatform::Windows,
            HostPlatform::Unknown,
        ] {
            assert_eq!(
                HostPenButtons::for_host(platform, unknown),
                HostPenButtons::Actions
            );
        }
        assert_eq!(
            linux(InputCapabilityAvailability::Unavailable),
            HostPenButtons::LinuxLegacy
        );
    }

    #[test]
    fn the_legacy_layout_swaps_only_the_two_clicks() {
        let legacy = HostPenButtons::LinuxLegacy;
        assert_eq!(legacy.wire(0), 0);
        assert_eq!(legacy.wire(0b11), 0b11);
        assert_eq!(legacy.wire(0b101), 0b110);
        assert_eq!(HostPenButtons::Actions.wire(0b101), 0b101);
    }

    #[test]
    fn mac_side_switches_follow_the_settings() {
        let mut map = PenButtonMap::default();
        let lower = PenButtonsHeld {
            lower: true,
            ..PenButtonsHeld::default()
        };
        let upper = PenButtonsHeld {
            upper: true,
            ..PenButtonsHeld::default()
        };
        assert_eq!(map.wire_buttons(lower), PEN_BUTTON_RIGHT_CLICK);
        assert_eq!(map.wire_buttons(upper), PEN_BUTTON_MIDDLE_CLICK);
        map.settings.lower = PenButtonAction::MiddleClick;
        map.settings.upper = PenButtonAction::RightClick;
        assert_eq!(map.wire_buttons(lower), PEN_BUTTON_MIDDLE_CLICK);
        assert_eq!(map.wire_buttons(upper), PEN_BUTTON_RIGHT_CLICK);
        map.settings.upper = PenButtonAction::Off;
        assert_eq!(map.wire_buttons(upper), 0);
    }

    #[test]
    fn a_click_the_driver_performs_is_honoured_whatever_the_settings() {
        let map = PenButtonMap {
            settings: PenButtonSettings {
                barrel: PenButtonAction::Off,
                lower: PenButtonAction::Off,
                upper: PenButtonAction::Off,
            },
            host: HostPenButtons::Actions,
        };
        let held = PenButtonsHeld {
            driver_right: true,
            driver_middle: true,
            ..PenButtonsHeld::default()
        };
        assert_eq!(
            map.wire_buttons(held),
            PEN_BUTTON_RIGHT_CLICK | PEN_BUTTON_MIDDLE_CLICK
        );
    }

    #[test]
    fn tokens_round_trip() {
        for action in PenButtonAction::ALL {
            assert_eq!(PenButtonAction::from_token(action.token()), Some(action));
        }
        assert_eq!(PenButtonAction::from_token("left_click"), None);
    }
}
