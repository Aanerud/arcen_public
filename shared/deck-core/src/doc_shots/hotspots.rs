// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which widget each of the manual's numbered hotspots points at: one
//! small explicit table, by the widget's accessible name (its text, or the
//! name an icon button gives itself), never by a guessed position. A
//! hotspot that names a group rather than one widget is a [`Target::Group`]
//! and is reported as unmapped, with the widgets it spans.

use super::DocPlatform;
use super::capture::{Name, Widget, find};

/// What a hotspot points at on one Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The widget `name` names, the `nth` from the top left.
    Widget(Name, usize),
    /// Not one widget: why, and the widgets it spans.
    Group(&'static str, &'static [(Name, usize)]),
    /// No widget in the image is what the hotspot names: why.
    Unmapped(&'static str),
    /// The hotspot is not on this Deck (the manifest gives no position).
    Absent,
}

/// One numbered hotspot of a scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotspot {
    /// The manifest's number.
    pub n: u64,
    /// The manifest's label.
    pub label: &'static str,
    pub mac: Target,
    pub windows: Target,
}

impl Hotspot {
    /// Its target on `platform`'s Deck.
    #[must_use]
    pub const fn target(&self, platform: DocPlatform) -> Target {
        match platform {
            DocPlatform::Mac => self.mac,
            DocPlatform::Windows => self.windows,
        }
    }
}

/// Where a hotspot landed in one image.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// On this widget.
    At { widget: Name, rect: egui::Rect },
    /// A group: reported unmapped, with where each part is.
    Group {
        reason: &'static str,
        parts: Vec<(Name, Option<egui::Rect>)>,
    },
    /// Reported unmapped: no widget is what it names.
    Unmapped { reason: &'static str },
    /// The widget it names is not in the image.
    Missing { widget: Name },
    /// Not on this Deck.
    Absent,
}

/// The Connection form's Typing picker, which the connection-picker scene
/// opens.
pub const TYPING_PICKER: Name = Name::StartsWith("Typing, ");

const fn both(n: u64, label: &'static str, target: Target) -> Hotspot {
    Hotspot {
        n,
        label,
        mac: target,
        windows: target,
    }
}

const fn on(n: u64, label: &'static str, mac: Target, windows: Target) -> Hotspot {
    Hotspot {
        n,
        label,
        mac,
        windows,
    }
}

const fn is(text: &'static str) -> Target {
    Target::Widget(Name::Is(text), 0)
}

const fn control(text: &'static str) -> Target {
    Target::Widget(Name::Control(text), 0)
}

const fn starts(text: &'static str) -> Target {
    Target::Widget(Name::StartsWith(text), 0)
}

const fn under(caption: &'static str) -> Target {
    Target::Widget(Name::Under(caption), 0)
}

const HOME: &[Hotspot] = &[
    both(1, "Help", control("Help")),
    both(2, "Settings", control("Settings")),
    both(3, "Add (\u{ff0b})", control("Add connection")),
    both(4, "Search", is("Search\u{2026}")),
    both(5, "The list", control("Grade 1")),
    both(6, "Row menu (\u{22ee})", control("Connection menu")),
    // The selected machine's name, the column's title (the row is a
    // control of the same name).
    both(7, "Name", is("Grade 1")),
    both(
        8,
        "Username and password",
        Target::Group(
            "the two credential fields together",
            &[(Name::Under("Username"), 0), (Name::Under("Password"), 0)],
        ),
    ),
    both(9, "Connect", control("Connect")),
    both(
        10,
        "Two options",
        Target::Group(
            "the two options under Connect together",
            &[
                (Name::Control("Remember username"), 0),
                (Name::Control("Auto-reconnect"), 0),
            ],
        ),
    ),
    both(11, "Edit", control("Edit")),
];

const HOME_EMPTY: &[Hotspot] = &[
    both(1, "Empty list", is("No saved connections yet.")),
    // The ＋ beside "Connections" is the first control of that name.
    both(
        2,
        "Add connection",
        Target::Widget(Name::Control("Add connection"), 1),
    ),
    both(3, "Import\u{2026}", control("Import\u{2026}")),
];

const ADD_MENU: &[Hotspot] = &[
    both(
        1,
        "New connection\u{2026}",
        control("New connection\u{2026}"),
    ),
    both(
        2,
        "Import from file\u{2026}",
        control("Import from file\u{2026}"),
    ),
];

const ROW_MENU: &[Hotspot] = &[
    // The menu's items lie above the column's Edit.
    both(1, "Edit", control("Edit")),
    both(2, "Test connection", control("Test connection")),
    both(3, "Export\u{2026}", control("Export\u{2026}")),
    both(4, "Delete", control("Delete")),
];

const CONNECTION_NEW: &[Hotspot] = &[
    both(1, "Address", under("Address")),
    both(2, "Name (optional)", under("Name (optional)")),
    both(3, "Username (optional)", under("Username (optional)")),
    both(4, "On this machine", is("On this machine")),
    on(
        5,
        "\u{2318} key",
        under("\u{2318} key"),
        under("Command key on a Mac host"),
    ),
    on(6, "Option key", under("Option key"), Target::Absent),
    both(7, "Typing", Target::Widget(TYPING_PICKER, 0)),
    both(8, "Displays", under("Displays")),
    both(9, "Test connection", control("Test connection")),
    both(
        10,
        "Cancel and Add connection",
        Target::Group(
            "the form's two buttons together",
            // The ＋ beside "Connections" is the first "Add connection".
            &[
                (Name::Control("Cancel"), 0),
                (Name::Control("Add connection"), 1),
            ],
        ),
    ),
];

const CONNECTION_PICKER: &[Hotspot] = &[
    both(1, "The value in force", Target::Widget(TYPING_PICKER, 0)),
    both(
        2,
        "The choices",
        Target::Group(
            "the open list's rows together (the rows have no accessible name)",
            &[],
        ),
    ),
    both(3, "One line of help", starts("Sends the letters you type")),
];

const CONNECTION_TEST: &[Hotspot] = &[
    both(1, "What was tested", starts("Connection test \u{2014} ")),
    both(2, "What to do", starts("What to do: ")),
    both(3, "Copy report", control("Copy report")),
];

const SETTINGS_STREAMING: &[Hotspot] = &[
    both(
        1,
        "Pages",
        Target::Group(
            "the page list (its section headings; the pages have no accessible name)",
            &[
                (Name::Is("General"), 0),
                (Name::Is("Input and devices"), 0),
                (Name::Is("System"), 0),
            ],
        ),
    ),
    both(2, "Done", control("Done")),
    both(3, "Presets", under("Preset")),
    both(4, "Exact still image", control("Exact still image")),
    both(5, "Colour check\u{2026}", control("Colour check\u{2026}")),
];

const SETTINGS_DISPLAYS: &[Hotspot] = &[
    both(1, "How many displays", starts("Displays, ")),
    on(
        2,
        "Notch area",
        control("Extend fullscreen under the notch"),
        Target::Absent,
    ),
    both(3, "Remote UI scale", under("Remote UI scale")),
    both(4, "HiDPI streaming", starts("HiDPI streaming")),
    both(
        5,
        "Per-display colour priority",
        is("Per-display color priority"),
    ),
];

const SETTINGS_INPUT: &[Hotspot] = &[
    both(1, "The keyboard switch", starts("Press ")),
    both(2, "System shortcuts", starts("System shortcuts go to ")),
    both(3, "Typing", under("Default for all connections")),
    both(4, "Clipboard", starts("Share the clipboard")),
];

const NOT_SHOWN_CHOICE: &str =
    "a choice inside the closed Tablet picker: no widget of its own on screen";

const SETTINGS_TABLET: &[Hotspot] = &[
    both(1, "Tablet support", starts(", Tablet support")),
    both(
        2,
        "Pen buttons",
        Target::Group(
            "the two pen-button pickers together",
            &[
                (Name::Under("Lower pen button"), 0),
                (Name::Under("Upper pen button"), 0),
            ],
        ),
    ),
    both(3, "Native tablet", Target::Unmapped(NOT_SHOWN_CHOICE)),
    both(4, "Mouse only", Target::Unmapped(NOT_SHOWN_CHOICE)),
];

const SETTINGS_LOGS: &[Hotspot] = &[
    both(1, "Log level", under("Log level")),
    both(2, "Show in folder", control("Show in folder")),
    both(
        3,
        "Create support bundle\u{2026}",
        control("Create support bundle\u{2026}"),
    ),
];

const SETTINGS_ADVANCED: &[Hotspot] = &[
    both(
        1,
        "Certificate checking",
        under("How strictly the Deck checks a host's certificate before it connects."),
    ),
    both(2, "Keyboard switch", is("Keyboard switch")),
    both(3, "Paste as Keystrokes", is("Paste as Keystrokes")),
];

const SETTINGS_PRIVACY: &[Hotspot] = &[
    both(
        1,
        "Keep the host\u{2019}s desk private",
        is("Keep the host\u{2019}s desk private"),
    ),
    both(2, "What it does", starts("While you are connected")),
];

const SIGN_IN_ERROR: &[Hotspot] = &[
    both(1, "The field to fix", under("Password")),
    both(2, "What happened", is("Wrong username or password.")),
    both(3, "Details", control("Details")),
];

const VERIFY_HOST: &[Hotspot] = &[
    both(1, "SPKI SHA-256", starts("SPKI SHA-256 ")),
    both(2, "Trust and Remember", control("Trust and Remember")),
    both(3, "Trust Once", control("Trust Once")),
    both(4, "Cancel", control("Cancel")),
];

const IDENTITY_CHANGED: &[Hotspot] = &[
    both(
        1,
        "What happened",
        starts("Grade 1 no longer has the identity"),
    ),
    both(
        2,
        "Remembered and now presented",
        Target::Group(
            "the two fingerprints together",
            &[
                (Name::StartsWith("Remembered "), 0),
                (Name::StartsWith("Now presented "), 0),
            ],
        ),
    ),
    both(3, "Cancel", control("Cancel")),
    both(
        4,
        "Trust the new identity\u{2026}",
        control("Trust the new identity\u{2026}"),
    ),
];

const IMPORT_REVIEW: &[Hotspot] = &[
    both(
        1,
        "What will be added",
        Target::Group(
            "the summary's name, address and username together",
            // Home's own Username caption is above the summary's.
            &[
                (Name::Is("Name"), 0),
                (Name::Is("Address"), 0),
                (Name::Is("Username"), 1),
            ],
        ),
    ),
    both(2, "On this machine", is("On this machine")),
    both(3, "Host identity", is("Host identity")),
    both(
        4,
        "Import or Cancel",
        Target::Group(
            "the summary's two buttons together",
            &[(Name::Control("Import"), 0), (Name::Control("Cancel"), 0)],
        ),
    ),
];

const HOME_OWN_SETTINGS: &[Hotspot] = &[
    both(
        1,
        "This connection's own settings",
        starts("This connection: "),
    ),
    both(
        2,
        "Use app defaults",
        control(crate::screens::home::USE_APP_DEFAULTS),
    ),
];

const SIGN_IN_REFUSED: &[Hotspot] = &[
    both(1, "What happened", starts("Nobody is signed in at ")),
    both(2, "Details", control("Details")),
    both(3, "OK", control("OK")),
];

const CONNECTING_ONE_DISPLAY: &[Hotspot] = &[
    both(
        1,
        "One display, for this session only",
        starts("Connecting to "),
    ),
    both(2, "Cancel", control("Cancel")),
];

/// Every scene's hotspots, in the manifest's order, then the scenes added
/// since.
pub const TABLE: [(&str, &[Hotspot]); 21] = [
    ("home", HOME),
    ("home-empty", HOME_EMPTY),
    ("add-menu", ADD_MENU),
    ("row-menu", ROW_MENU),
    ("connection-new", CONNECTION_NEW),
    ("connection-picker", CONNECTION_PICKER),
    ("connection-test", CONNECTION_TEST),
    ("settings-streaming", SETTINGS_STREAMING),
    ("settings-displays", SETTINGS_DISPLAYS),
    ("settings-input", SETTINGS_INPUT),
    ("settings-tablet", SETTINGS_TABLET),
    ("settings-logs", SETTINGS_LOGS),
    ("settings-advanced", SETTINGS_ADVANCED),
    ("sign-in-error", SIGN_IN_ERROR),
    ("verify-host", VERIFY_HOST),
    ("identity-changed", IDENTITY_CHANGED),
    ("import-review", IMPORT_REVIEW),
    ("home-own-settings", HOME_OWN_SETTINGS),
    ("sign-in-refused", SIGN_IN_REFUSED),
    ("settings-privacy", SETTINGS_PRIVACY),
    ("connecting-one-display", CONNECTING_ONE_DISPLAY),
];

/// `scene`'s hotspots.
#[must_use]
pub fn scene_hotspots(scene: &str) -> &'static [Hotspot] {
    TABLE
        .iter()
        .find(|(id, _)| *id == scene)
        .map_or(&[], |(_, hotspots)| *hotspots)
}

/// Hotspot `n` of `scene`.
#[must_use]
pub fn hotspot(scene: &str, n: u64) -> Option<&'static Hotspot> {
    scene_hotspots(scene).iter().find(|hotspot| hotspot.n == n)
}

/// Where each of `scene`'s hotspots lands among `widgets` on `platform`'s
/// Deck.
#[must_use]
pub fn place(
    scene: &str,
    platform: DocPlatform,
    widgets: &[Widget],
) -> Vec<(&'static Hotspot, Placement)> {
    scene_hotspots(scene)
        .iter()
        .map(|hotspot| {
            let placement = match hotspot.target(platform) {
                Target::Widget(name, nth) => find(widgets, name).get(nth).map_or(
                    Placement::Missing { widget: name },
                    |found| Placement::At {
                        widget: name,
                        rect: found.rect,
                    },
                ),
                Target::Group(reason, parts) => Placement::Group {
                    reason,
                    parts: parts
                        .iter()
                        .map(|(name, nth)| (*name, find(widgets, *name).get(*nth).map(|w| w.rect)))
                        .collect(),
                },
                Target::Unmapped(reason) => Placement::Unmapped { reason },
                Target::Absent => Placement::Absent,
            };
            (hotspot, placement)
        })
        .collect()
}
