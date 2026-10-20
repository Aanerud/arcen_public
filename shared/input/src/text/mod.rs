//! Committed text: what a client's input method produced, and how a host
//! types it into a desktop.
//!
//! A key event names a key *position*; the host's own keyboard layout
//! decides which character it types. Committed text (dead-key results, input
//! method conversions, characters a host's layout puts elsewhere) names the
//! characters themselves. This module holds the operating-system-free parts
//! of both ends:
//!
//! - [`keymap`]: an index of a host keyboard layout, answering which
//!   characters a compositor can type by keysym — mirroring the rule the
//!   GNOME compositor applies, so a host can prove a character typeable
//!   before sending it instead of having it dropped silently.
//! - [`plan`]: one commit turned into keysym runs, IM-commit chunks and
//!   refusals, with the report a client is told.
//! - [`layout`]: which layout group of a session keymap is active, from the
//!   keymap's own names and the desktop's input-source settings.
//! - [`policy`]: the client-side choice, per keystroke, between sending the
//!   key position and sending the characters.

pub mod keymap;
pub mod layout;
pub mod plan;
pub mod policy;
pub mod releases;

pub use keymap::{KeyLocation, KeymapEntry, KeymapIndex, PositionChars};
pub use layout::{
    GroupResolution, InputSource, XkbRegistry, active_input_source, group_names,
    layouts_from_symbols_section, parse_input_sources, resolve_active_group,
    resolve_active_group_by_names, symbols_section_name,
};
pub use plan::{
    MAX_TEXT_PART_CHARS, MAX_UTF8_CHUNK_BYTES, TextDeliveryReport, TextPlan, TextRoutes, TextRun,
    plan_text, text_parts, utf8_chunks,
};
pub use policy::{
    DeckTextAction, DeckTextEvent, HostKeyPositions, HostTextTruth, KeyPhase, KeyStroke,
    OptionKeyMode, OptionSide, SuppressReason, TextInputPolicy, TypingMode, split_commit,
};
pub use releases::{LOST_RELEASE_GRACE, ReleaseRecovery, SentPress};
