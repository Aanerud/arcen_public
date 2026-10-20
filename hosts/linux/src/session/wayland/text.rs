//! Committed text into a headless Wayland session.
//!
//! The session types a commit by keysym through Mutter `RemoteDesktop`,
//! which types only keysyms its active layout group holds (at levels 0–2) and
//! drops any other silently. So the session first reads the compositor's
//! keymap (as a Wayland client, [`super::wl_keymap`]) and indexes it
//! ([`super::xkb`]), works out which group is active, and types only what
//! the index proves typeable; the rest is committed through the
//! compositor's input method when it offers one (`ei_text`), or refused and
//! reported. The planning is the shared [`arcen_input::text`] code; this
//! module holds the session's live state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use arcen_input::text::{
    layouts_from_symbols_section, plan_text, resolve_active_group, resolve_active_group_by_names,
    symbols_section_name, GroupResolution, InputSource, KeymapIndex, TextPlan, TextRoutes,
    XkbRegistry,
};
use arcen_protocol::messages::TextRefusalReason;
use serde::{Deserialize, Serialize};

/// How long the desktop's input-source settings are trusted before they are
/// read again.
const INPUT_SOURCES_TTL: Duration = Duration::from_millis(500);

/// The compositor's keymap, indexed.
#[derive(Debug)]
pub struct SessionKeymapState {
    /// The index.
    pub index: KeymapIndex,
    /// The groups' layouts, from the keymap's symbols section.
    pub layouts: Vec<String>,
    /// Increases with every keymap the compositor sent.
    pub generation: u64,
    /// The keymap's symbols section name, when it has one (diagnostics).
    pub symbols_section: Option<String>,
    /// The groups' descriptive names (diagnostics).
    pub group_names: Vec<String>,
}

/// Reads the desktop's configured and most recently used input sources.
pub trait InputSourceReader: Send + Sync {
    /// `(sources, mru_sources)`, or `None` when they cannot be read.
    fn read(&self) -> Option<(Vec<InputSource>, Vec<InputSource>)>;
}

/// Commits text through the compositor's input method (`ei_text.utf8`).
pub trait Utf8Committer: Send + Sync {
    /// Commits `chunk` (at most 254 bytes).
    ///
    /// # Errors
    ///
    /// When the compositor connection refused or lost it.
    fn commit(&self, chunk: &str) -> Result<(), String>;
}

/// Where the compositor's input method stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Utf8Route {
    /// The compositor offers no EIS connection at all (`ConnectToEIS`).
    #[default]
    NoEis,
    /// EIS is offered, but its seat has no `ei_text`.
    NotOffered,
    /// `ei_text` is bound: commits go through the input method.
    Offered,
}

/// What a session's `hello` says about committed text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextFacts {
    /// The compositor's keymap has been read.
    pub keymap_known: bool,
    /// The keymap's layouts, in group order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layouts: Vec<String>,
    /// The input-method route.
    pub utf8: Utf8Route,
    /// The keymap generation.
    pub keymap_generation: u64,
    /// Evidence from the compositor's EIS seat: the `ei_text` version the
    /// handshake negotiated, and the seat's capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ei_text_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eis_seat: Vec<String>,
    /// Why the EIS connection for committed text last ended, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eis_error: Option<String>,
}

/// What became of one commit, in the reply to [`super::InputCommand::Text`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextOutcome {
    /// Characters typed through the layout, proven.
    pub keymap_chars: u32,
    /// Characters typed by keysym without proof.
    #[serde(default)]
    pub unverified_chars: u32,
    /// Characters committed through the input method.
    #[serde(default)]
    pub utf8_chars: u32,
    /// Characters not typed.
    pub refused_chars: u32,
    /// The characters not typed (at most 64).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refused_text: String,
    /// Why not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TextRefusalReason>,
    /// The keymap generation the commit was planned on.
    pub keymap_generation: u64,
    /// How the active group was established.
    pub group: String,
}

impl TextOutcome {
    /// Adds the outcome of the next part of the same commit.
    pub fn absorb(&mut self, next: &Self) {
        self.keymap_chars = self.keymap_chars.saturating_add(next.keymap_chars);
        self.unverified_chars = self.unverified_chars.saturating_add(next.unverified_chars);
        self.utf8_chars = self.utf8_chars.saturating_add(next.utf8_chars);
        self.refused_chars = self.refused_chars.saturating_add(next.refused_chars);
        let room = arcen_protocol::messages::MAX_REFUSED_TEXT_CHARS
            .saturating_sub(self.refused_text.chars().count());
        self.refused_text
            .extend(next.refused_text.chars().take(room));
        self.reason = self.reason.or(next.reason);
        self.keymap_generation = next.keymap_generation;
        if !next.group.is_empty() {
            self.group.clone_from(&next.group);
        }
    }
}

/// The characters one key position types, by evdev code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPositionChars {
    /// Linux evdev code.
    pub evdev: u32,
    /// Unmodified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// With Shift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<String>,
}

/// The session's layout, in reply to [`super::InputCommand::Keymap`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKeymap {
    /// The keymap generation.
    pub generation: u64,
    /// The layouts, in group order.
    pub layouts: Vec<String>,
    /// The active layout, when the active group is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_layout: Option<String>,
    /// How the active group was established.
    pub group: String,
    /// The active group's printable positions; empty when the group is
    /// unknown.
    #[serde(default)]
    pub keys: Vec<KeyPositionChars>,
    /// Diagnostics: the keymap's group count, symbols section name, group
    /// names, and the input sources read (`kind:id`, most recent first).
    #[serde(default)]
    pub groups: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols_section: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_sources: Option<Vec<String>>,
}

/// The keymap's character for a keysym level: none for dead keys and
/// controls.
fn printable(character: Option<char>) -> Option<String> {
    character
        .filter(|character| !character.is_control())
        .map(String::from)
}

/// Configured and most recently used input sources.
type InputSources = (Vec<InputSource>, Vec<InputSource>);

/// The session's committed-text state.
pub struct SessionText {
    keymap: RwLock<Option<Arc<SessionKeymapState>>>,
    generation: AtomicU64,
    sources: Option<Box<dyn InputSourceReader>>,
    sources_cache: std::sync::Mutex<Option<(Instant, Option<InputSources>)>>,
    utf8: RwLock<(Utf8Route, Option<Arc<dyn Utf8Committer>>)>,
    registry: std::sync::OnceLock<XkbRegistry>,
    eis_evidence: RwLock<(Option<u32>, Vec<String>)>,
    eis_error: RwLock<Option<String>>,
}

/// Where the XKB registry lists layout and variant descriptions.
const XKB_REGISTRY: &str = "/usr/share/X11/xkb/rules/evdev.xml";

impl std::fmt::Debug for SessionText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionText")
            .field(
                "keymap_generation",
                &self.generation.load(Ordering::Relaxed),
            )
            .field("utf8", &self.utf8_route())
            .finish_non_exhaustive()
    }
}

impl Default for SessionText {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SessionText {
    /// A session whose input sources come from `sources`.
    #[must_use]
    pub fn new(sources: Option<Box<dyn InputSourceReader>>) -> Self {
        Self {
            keymap: RwLock::new(None),
            generation: AtomicU64::new(0),
            sources,
            sources_cache: std::sync::Mutex::new(None),
            utf8: RwLock::new((Utf8Route::NoEis, None)),
            registry: std::sync::OnceLock::new(),
            eis_evidence: RwLock::new((None, Vec::new())),
            eis_error: RwLock::new(None),
        }
    }

    /// Records why the EIS connection for committed text ended.
    pub fn set_eis_error(&self, error: Option<String>) {
        *self
            .eis_error
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = error;
    }

    /// Records what the compositor's EIS seat offered.
    pub fn set_eis_evidence(&self, ei_text_version: Option<u32>, seat: Vec<String>) {
        *self
            .eis_evidence
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (ei_text_version, seat);
    }

    /// Uses `registry` for layout descriptions instead of the system's.
    #[must_use]
    pub fn with_registry(self, registry: XkbRegistry) -> Self {
        let _ = self.registry.set(registry);
        self
    }

    fn registry(&self) -> &XkbRegistry {
        self.registry.get_or_init(|| {
            std::fs::read_to_string(XKB_REGISTRY)
                .map(|xml| XkbRegistry::parse(&xml))
                .unwrap_or_default()
        })
    }

    /// Records a keymap the compositor sent (text format), indexed.
    pub fn set_keymap(&self, text: &str, index: KeymapIndex) -> u64 {
        let layouts = symbols_section_name(text)
            .map(layouts_from_symbols_section)
            .unwrap_or_default();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let state = SessionKeymapState {
            index,
            layouts,
            generation,
            symbols_section: symbols_section_name(text).map(str::to_owned),
            group_names: arcen_input::text::group_names(text),
        };
        *self
            .keymap
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(state));
        generation
    }

    /// The number of keymaps read so far.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The current keymap.
    #[must_use]
    pub fn keymap(&self) -> Option<Arc<SessionKeymapState>> {
        self.keymap
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Records the input-method route.
    pub fn set_utf8(&self, route: Utf8Route, committer: Option<Arc<dyn Utf8Committer>>) {
        *self
            .utf8
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (route, committer);
    }

    /// The input-method route.
    #[must_use]
    pub fn utf8_route(&self) -> Utf8Route {
        self.utf8
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
    }

    fn utf8_committer(&self) -> Option<Arc<dyn Utf8Committer>> {
        self.utf8
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
            .clone()
    }

    fn input_sources(&self) -> Option<(Vec<InputSource>, Vec<InputSource>)> {
        let reader = self.sources.as_ref()?;
        let mut cache = self
            .sources_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((read_at, sources)) = cache.as_ref() {
            if read_at.elapsed() < INPUT_SOURCES_TTL {
                return sources.clone();
            }
        }
        let sources = reader.read();
        *cache = Some((Instant::now(), sources.clone()));
        sources
    }

    /// The active group of `keymap`.
    #[must_use]
    pub fn active_group(&self, keymap: &SessionKeymapState) -> GroupResolution {
        if keymap.index.group_count() <= 1 {
            return GroupResolution::Single;
        }
        let Some((sources, mru)) = self.input_sources() else {
            return GroupResolution::Unknown;
        };
        let groups = keymap.index.group_count();
        if keymap.layouts.len() == groups {
            return resolve_active_group(&keymap.layouts, &sources, &mru);
        }
        // A keymap without a symbols section name still names its groups
        // by description; the XKB registry describes the input sources.
        if keymap.group_names.len() == groups {
            let registry = self.registry();
            return resolve_active_group_by_names(&keymap.group_names, &sources, &mru, |source| {
                registry.describe(source)
            });
        }
        GroupResolution::Unknown
    }

    /// What a `hello` reports.
    #[must_use]
    pub fn facts(&self) -> TextFacts {
        let keymap = self.keymap();
        TextFacts {
            keymap_known: keymap.is_some(),
            layouts: keymap
                .as_ref()
                .map(|keymap| keymap.layouts.clone())
                .unwrap_or_default(),
            utf8: self.utf8_route(),
            keymap_generation: keymap.map_or(0, |keymap| keymap.generation),
            ei_text_version: self
                .eis_evidence
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0,
            eis_seat: self
                .eis_evidence
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .1
                .clone(),
            eis_error: self
                .eis_error
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// Plans `text` over what the session knows now. Without a keymap the
    /// session cannot prove anything: it types by keysym unverified (as a
    /// compositor without a readable keymap leaves no better choice).
    #[must_use]
    pub fn plan(&self, text: &str) -> (TextPlan, TextOutcome) {
        let keymap = self.keymap();
        let resolution = keymap
            .as_deref()
            .map_or(GroupResolution::Unknown, |keymap| self.active_group(keymap));
        let utf8 = self.utf8_route() == Utf8Route::Offered && self.utf8_committer().is_some();
        let routes = TextRoutes {
            keymap: keymap
                .as_deref()
                .zip(resolution.group())
                .map(|(keymap, group)| (&keymap.index, group)),
            unverified_keysyms: true,
            utf8,
        };
        let mut plan = plan_text(text, &routes, fallback_keysym);
        // A known keymap whose active group is not: characters typed through
        // it are not proven.
        if keymap.is_some() && resolution.group().is_none() {
            plan.report.reason = plan.report.reason.or_else(|| {
                (plan.report.unverified_chars > 0).then_some(TextRefusalReason::KeymapUnknown)
            });
        }
        let report = &plan.report;
        let outcome = TextOutcome {
            keymap_chars: report.keymap_chars,
            unverified_chars: report.unverified_chars,
            utf8_chars: report.utf8_chars,
            refused_chars: report.refused_chars,
            refused_text: report.refused_text.clone(),
            reason: report.reason,
            keymap_generation: keymap.as_ref().map_or(0, |keymap| keymap.generation),
            group: resolution.token().to_owned(),
        };
        (plan, outcome)
    }

    /// Commits one chunk through the input method.
    ///
    /// # Errors
    ///
    /// When no input method is bound, or it refused.
    pub fn commit_utf8(&self, chunk: &str) -> Result<(), String> {
        self.utf8_committer()
            .ok_or_else(|| "no input-method route".to_owned())?
            .commit(chunk)
    }

    /// The session's layout, for the client.
    #[must_use]
    pub fn session_keymap(&self) -> Option<SessionKeymap> {
        let keymap = self.keymap()?;
        let resolution = self.active_group(&keymap);
        let group = resolution.group();
        let keys = group
            .and_then(|group| keymap.index.positions(group))
            .map(|positions| {
                positions
                    .iter()
                    .filter(|(_, chars)| chars.base.is_some() || chars.shift.is_some())
                    .map(|(keycode, chars)| KeyPositionChars {
                        evdev: keycode.saturating_sub(8),
                        base: printable(chars.base),
                        shift: printable(chars.shift),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let input_sources = self.input_sources().map(|(sources, mru)| {
            mru.iter()
                .map(|source| format!("mru {}:{}", source.kind, source.id))
                .chain(
                    sources
                        .iter()
                        .map(|source| format!("{}:{}", source.kind, source.id)),
                )
                .collect()
        });
        Some(SessionKeymap {
            generation: keymap.generation,
            layouts: keymap.layouts.clone(),
            active_layout: group.and_then(|group| {
                keymap
                    .layouts
                    .get(group as usize)
                    .or_else(|| keymap.group_names.get(group as usize))
                    .cloned()
            }),
            group: resolution.token().to_owned(),
            keys,
            groups: keymap.index.group_count(),
            symbols_section: keymap.symbols_section.clone(),
            group_names: keymap.group_names.clone(),
            input_sources,
        })
    }
}

/// The keysym naming `character` when no keymap proves one
/// (`xkb_utf32_to_keysym`).
fn fallback_keysym(character: char) -> Option<u32> {
    #[cfg(all(target_os = "linux", feature = "wayland-provider"))]
    {
        let keysym: u32 = xkeysym::Keysym::from_char(character).into();
        (keysym != 0).then_some(keysym)
    }
    #[cfg(not(all(target_os = "linux", feature = "wayland-provider")))]
    {
        match u32::from(character) {
            code @ (0x20..=0x7e | 0xa0..=0xff) => Some(code),
            code => Some(0x0100_0000 | code),
        }
    }
}

/// Reads GNOME's input sources with `gsettings` (the session user's own
/// settings database; reading needs no bus).
#[derive(Debug, Default)]
pub struct GsettingsInputSources;

impl InputSourceReader for GsettingsInputSources {
    fn read(&self) -> Option<(Vec<InputSource>, Vec<InputSource>)> {
        let read = |key: &str| -> Option<Vec<InputSource>> {
            let output = std::process::Command::new("gsettings")
                .args(["get", "org.gnome.desktop.input-sources", key])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            output.status.success().then(|| {
                arcen_input::text::parse_input_sources(&String::from_utf8_lossy(&output.stdout))
            })
        };
        Some((read("sources")?, read("mru-sources").unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::text::KeymapEntry;

    #[test]
    fn the_parts_of_one_commit_add_up_to_one_outcome() {
        let mut outcome = TextOutcome {
            keymap_chars: 60,
            refused_chars: 4,
            refused_text: "😀".repeat(4),
            reason: Some(TextRefusalReason::CompositorLacksTextInput),
            keymap_generation: 1,
            group: "single".to_owned(),
            ..TextOutcome::default()
        };
        outcome.absorb(&TextOutcome {
            keymap_chars: 2,
            refused_chars: 62,
            refused_text: "日".repeat(62),
            reason: Some(TextRefusalReason::CompositorLacksTextInput),
            keymap_generation: 2,
            ..TextOutcome::default()
        });
        assert_eq!(outcome.keymap_chars, 62);
        assert_eq!(outcome.refused_chars, 66);
        assert_eq!(
            outcome.refused_text.chars().count(),
            arcen_protocol::messages::MAX_REFUSED_TEXT_CHARS
        );
        assert!(outcome.refused_text.starts_with("😀😀😀😀日"));
        assert_eq!(outcome.keymap_generation, 2);
        assert_eq!(outcome.group, "single");
    }

    fn entry(keycode: u32, group: u32, level: u32, keysym: u32) -> KeymapEntry {
        KeymapEntry {
            keycode,
            group,
            level,
            keysym,
            character: char::from_u32(keysym).filter(|_| keysym < 0x100),
        }
    }

    const TWO_GROUPS: &str = "xkb_keymap {\nxkb_symbols \"pc+no+us:2+inet(evdev)\" {\n};\n};";

    fn two_groups() -> KeymapIndex {
        KeymapIndex::new(
            [
                entry(50, 0, 0, 0xffe1),
                entry(50, 1, 0, 0xffe1),
                entry(48, 0, 0, 0xe6),
                entry(48, 0, 1, 0xc6),
                entry(48, 1, 0, 0x27),
                entry(48, 1, 1, 0x22),
                entry(38, 0, 0, 0x61),
                entry(38, 1, 0, 0x61),
            ],
            255,
            Vec::new(),
        )
    }

    struct Sources(Vec<InputSource>, Vec<InputSource>);

    impl InputSourceReader for Sources {
        fn read(&self) -> Option<(Vec<InputSource>, Vec<InputSource>)> {
            Some((self.0.clone(), self.1.clone()))
        }
    }

    fn xkb(id: &str) -> InputSource {
        InputSource {
            kind: "xkb".to_owned(),
            id: id.to_owned(),
        }
    }

    struct Recorder(std::sync::Mutex<Vec<String>>);

    impl Utf8Committer for Recorder {
        fn commit(&self, chunk: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(chunk.to_owned());
            Ok(())
        }
    }

    #[test]
    fn without_a_keymap_text_goes_unverified() {
        let text = SessionText::default();
        assert!(!text.facts().keymap_known);
        let (plan, outcome) = text.plan("æ");
        assert!(plan.types_keysyms());
        assert_eq!(outcome.unverified_chars, 1);
        assert_eq!(outcome.group, "unknown");
        assert!(text.session_keymap().is_none());
    }

    #[test]
    fn the_active_group_decides_what_is_typeable() {
        let text = SessionText::new(Some(Box::new(Sources(
            vec![xkb("no"), xkb("us")],
            vec![xkb("us")],
        ))));
        assert_eq!(text.set_keymap(TWO_GROUPS, two_groups()), 1);
        assert_eq!(text.facts().layouts, ["no", "us"]);
        let (_, outcome) = text.plan("aæ");
        assert_eq!(outcome.keymap_chars, 1);
        assert_eq!(outcome.refused_text, "æ");
        assert_eq!(
            outcome.reason,
            Some(TextRefusalReason::CompositorLacksTextInput)
        );
        assert_eq!(outcome.group, "input_sources");
        let keymap = text.session_keymap().unwrap();
        assert_eq!(keymap.active_layout.as_deref(), Some("us"));
        assert!(keymap.keys.contains(&KeyPositionChars {
            evdev: 40,
            base: Some("'".to_owned()),
            shift: Some("\"".to_owned()),
        }));
    }

    #[test]
    fn an_unknown_group_types_unverified_and_says_so() {
        let text = SessionText::default();
        text.set_keymap(TWO_GROUPS, two_groups());
        let (plan, outcome) = text.plan("æ");
        assert!(plan.types_keysyms());
        assert_eq!(outcome.unverified_chars, 1);
        assert_eq!(outcome.reason, Some(TextRefusalReason::KeymapUnknown));
        assert!(text.session_keymap().unwrap().keys.is_empty());
    }

    #[test]
    fn the_input_method_takes_what_the_layout_lacks() {
        let text = SessionText::new(Some(Box::new(Sources(vec![xkb("no")], vec![]))));
        text.set_keymap(TWO_GROUPS, two_groups());
        let recorder = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        text.set_utf8(Utf8Route::Offered, Some(recorder.clone()));
        let (plan, outcome) = text.plan("æ😀");
        assert_eq!(outcome.keymap_chars, 1);
        assert_eq!(outcome.utf8_chars, 1);
        assert_eq!(outcome.refused_chars, 0);
        assert_eq!(plan.runs.len(), 2);
        text.commit_utf8("😀").unwrap();
        assert_eq!(recorder.0.lock().unwrap().as_slice(), ["😀"]);
    }

    #[test]
    fn an_unnamed_symbols_section_resolves_through_group_names() {
        const NAMED_GROUPS: &str = "xkb_keymap {\nxkb_symbols {\n    name[Group1]=\"English (US)\";\n    name[Group2]=\"Norwegian\";\n};\n};";
        let registry = XkbRegistry::parse(
            "<layoutList><layout><configItem><name>no</name><description>Norwegian</description></configItem></layout><layout><configItem><name>us</name><description>English (US)</description></configItem></layout></layoutList>",
        );
        let text = SessionText::new(Some(Box::new(Sources(
            vec![xkb("us"), xkb("no")],
            vec![xkb("no")],
        ))))
        .with_registry(registry);
        text.set_keymap(NAMED_GROUPS, two_groups());
        // Group 1 of `two_groups` is the US-shaped one; the names put
        // Norwegian second, so the active source `no` is group 1.
        let (_, outcome) = text.plan("'");
        assert_eq!(outcome.group, "input_sources");
        assert_eq!(outcome.keymap_chars, 1);
        assert_eq!(
            text.session_keymap().unwrap().active_layout.as_deref(),
            Some("Norwegian")
        );
    }
}
