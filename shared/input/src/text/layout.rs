//! Which layout group of a session keymap is active.
//!
//! A compositor's keymap can hold up to four layouts as groups; which one is
//! active is state the compositor keeps. A host that cannot ask for that
//! state reads it from two facts instead:
//!
//! - the keymap's own symbols section name, which libxkbcommon writes from
//!   the components it compiled (`pc+no+us:2+inet(evdev)`): the layout of
//!   each group, in order;
//! - the desktop's input-source settings (GNOME:
//!   `org.gnome.desktop.input-sources` `sources` and `mru-sources`): the
//!   source the user selected last is the active one, and GNOME puts it in
//!   the keymap as one of the groups.
//!
//! A single-group keymap needs neither. When the facts do not determine one
//! group, the answer is [`GroupResolution::Unknown`], never a guess.

/// How the active group was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupResolution {
    /// The keymap has exactly one group.
    Single,
    /// The active input source's layout is this group of the keymap.
    FromInputSources(u32),
    /// The compositor reported it (an EIS keyboard's modifiers).
    Reported(u32),
    /// Not determinable.
    Unknown,
}

impl GroupResolution {
    /// The active group, when known.
    #[must_use]
    pub const fn group(self) -> Option<u32> {
        match self {
            Self::Single => Some(0),
            Self::FromInputSources(group) | Self::Reported(group) => Some(group),
            Self::Unknown => None,
        }
    }

    /// A short token for logs and diagnostics.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Single => "single_group",
            Self::FromInputSources(_) => "input_sources",
            Self::Reported(_) => "compositor_reported",
            Self::Unknown => "unknown",
        }
    }
}

/// The name of the `xkb_symbols` section of a keymap in the text format
/// (`xkb_symbols "pc+no+inet(evdev)" {`).
#[must_use]
pub fn symbols_section_name(keymap: &str) -> Option<&str> {
    let start = keymap.find("xkb_symbols")? + "xkb_symbols".len();
    let rest = keymap[start..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The groups' descriptive names (`name[Group1]="Norwegian";`), in group
/// order, as libxkbcommon writes them into a keymap.
#[must_use]
pub fn group_names(keymap: &str) -> Vec<String> {
    let mut names: Vec<Option<String>> = Vec::new();
    for line in keymap.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("name[") else {
            continue;
        };
        let Some((group, value)) = rest.split_once(']') else {
            continue;
        };
        let group = group.trim();
        let digits = group
            .strip_prefix("Group")
            .or_else(|| group.strip_prefix("group"))
            .unwrap_or(group);
        let Ok(index) = digits.parse::<usize>() else {
            continue;
        };
        let Some(value) = value.split('"').nth(1) else {
            continue;
        };
        if !(1..=4).contains(&index) {
            continue;
        }
        if names.len() < index {
            names.resize(index, None);
        }
        names[index - 1].get_or_insert_with(|| value.to_owned());
    }
    names.into_iter().map(Option::unwrap_or_default).collect()
}

/// Symbol files that are options or model parts, never a group's layout.
const NON_LAYOUT_FILES: &[&str] = &[
    "pc",
    "inet",
    "group",
    "level3",
    "level5",
    "lv3",
    "lv5",
    "compose",
    "ctrl",
    "caps",
    "capslock",
    "altwin",
    "eurosign",
    "keypad",
    "kpdl",
    "nbsp",
    "terminate",
    "shift",
    "srvr_ctrl",
    "japan",
    "korean",
    "mod_led",
    "numpad",
    "rupeesign",
    "grp_led",
    "parens",
    "apple",
    "macintosh_vndr",
    "sun_vndr",
    "jolla_vndr",
    "olpc",
    "fkeys",
    "scrolllock",
    "custom",
];

/// The layout of each group, in group order, from a symbols section name
/// such as `pc+no(mac)+us:2+inet(evdev)+group(alt_shift_toggle)`: the first
/// layout file is group 1, a `:N` suffix names group N.
#[must_use]
pub fn layouts_from_symbols_section(section: &str) -> Vec<String> {
    let mut groups: Vec<Option<String>> = Vec::new();
    for token in section.split(['+', '|']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (name, group) = match token.rsplit_once(':') {
            Some((name, group)) => match group.parse::<usize>() {
                Ok(group) if (1..=4).contains(&group) => (name, Some(group)),
                _ => continue,
            },
            None => (token, None),
        };
        let file = name.split('(').next().unwrap_or(name);
        if NON_LAYOUT_FILES.contains(&file) || file.is_empty() {
            continue;
        }
        let group = match group {
            Some(group) => group,
            // Only the first layout file goes without a group suffix.
            None if groups.first().is_none_or(Option::is_none) => 1,
            None => continue,
        };
        if groups.len() < group {
            groups.resize(group, None);
        }
        if groups[group - 1].is_none() {
            groups[group - 1] = Some(name.to_owned());
        }
    }
    groups.into_iter().map(Option::unwrap_or_default).collect()
}

/// One configured input source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSource {
    /// `xkb` or `ibus`.
    pub kind: String,
    /// `no`, `us+mac`, an `IBus` engine name.
    pub id: String,
}

impl InputSource {
    /// The XKB layout name of an `xkb` source as a keymap writes it:
    /// `us+mac` becomes `us(mac)`.
    #[must_use]
    pub fn xkb_layout(&self) -> Option<String> {
        if self.kind != "xkb" {
            return None;
        }
        Some(match self.id.split_once('+') {
            Some((layout, variant)) if !variant.is_empty() => format!("{layout}({variant})"),
            Some((layout, _)) => layout.to_owned(),
            None => self.id.clone(),
        })
    }
}

/// Parses a `GVariant` `a(ss)` in text form, as `gsettings get` prints it
/// (`[('xkb', 'no'), ('xkb', 'us+mac')]`, `@a(ss) []`).
#[must_use]
pub fn parse_input_sources(text: &str) -> Vec<InputSource> {
    let mut sources = Vec::new();
    let mut strings = Vec::new();
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\'' || character == '"' {
            let quote = character;
            let mut value = String::new();
            while let Some(next) = chars.next() {
                match next {
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    next if next == quote => break,
                    next => value.push(next),
                }
            }
            strings.push(value);
        } else if character == ')' {
            if let [kind, id] = strings.as_slice() {
                sources.push(InputSource {
                    kind: kind.clone(),
                    id: id.clone(),
                });
            }
            strings.clear();
        }
    }
    sources
}

/// The source GNOME has active: the most recently used one that is still
/// configured, else the first configured one.
#[must_use]
pub fn active_input_source<'a>(
    sources: &'a [InputSource],
    mru: &[InputSource],
) -> Option<&'a InputSource> {
    mru.iter()
        .find_map(|recent| sources.iter().find(|source| *source == recent))
        .or_else(|| sources.first())
}

/// The active group of a keymap whose groups hold `group_layouts`, given
/// the desktop's configured and most-recently-used input sources.
#[must_use]
pub fn resolve_active_group(
    group_layouts: &[String],
    sources: &[InputSource],
    mru: &[InputSource],
) -> GroupResolution {
    if group_layouts.len() <= 1 {
        return GroupResolution::Single;
    }
    let Some(layout) = active_input_source(sources, mru).and_then(InputSource::xkb_layout) else {
        return GroupResolution::Unknown;
    };
    let mut matches = group_layouts
        .iter()
        .enumerate()
        .filter(|(_, group)| **group == layout);
    match (matches.next(), matches.next()) {
        (Some((group, _)), None) => {
            GroupResolution::FromInputSources(u32::try_from(group).unwrap_or(u32::MAX))
        }
        _ => GroupResolution::Unknown,
    }
}

/// The active group of a keymap that names its groups only by description
/// (`name[Group1]="Norwegian"`), given the desktop's input sources and
/// `describe`, which gives an `xkb` source's description from the XKB
/// registry (`evdev.xml`).
#[must_use]
pub fn resolve_active_group_by_names(
    group_names: &[String],
    sources: &[InputSource],
    mru: &[InputSource],
    describe: impl Fn(&InputSource) -> Option<String>,
) -> GroupResolution {
    if group_names.len() <= 1 {
        return GroupResolution::Single;
    }
    let Some(description) = active_input_source(sources, mru)
        .filter(|source| source.kind == "xkb")
        .and_then(describe)
    else {
        return GroupResolution::Unknown;
    };
    let mut matches = group_names
        .iter()
        .enumerate()
        .filter(|(_, name)| **name == description);
    match (matches.next(), matches.next()) {
        (Some((group, _)), None) => {
            GroupResolution::FromInputSources(u32::try_from(group).unwrap_or(u32::MAX))
        }
        _ => GroupResolution::Unknown,
    }
}

/// Descriptions of XKB layouts and variants, read from the XKB registry
/// (`rules/evdev.xml`): `no` → `Norwegian`, `no+mac` → `Norwegian
/// (Macintosh)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XkbRegistry {
    descriptions: std::collections::HashMap<String, String>,
}

fn xml_text(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = start + block[start..].find(&close)?;
    Some(
        block[start..end]
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

impl XkbRegistry {
    /// Reads the registry's layouts and variants from its XML text.
    #[must_use]
    pub fn parse(xml: &str) -> Self {
        let mut descriptions = std::collections::HashMap::new();
        let layouts = xml.find("<layoutList>").map_or("", |start| &xml[start..]);
        for layout in layouts.split("<layout>").skip(1) {
            let layout = layout.split("</layout>").next().unwrap_or_default();
            let (head, variants) = match layout.split_once("<variantList>") {
                Some((head, variants)) => (head, variants),
                None => (layout, ""),
            };
            let (Some(name), Some(description)) =
                (xml_text(head, "name"), xml_text(head, "description"))
            else {
                continue;
            };
            for variant in variants.split("<variant>").skip(1) {
                if let (Some(variant_name), Some(variant_description)) =
                    (xml_text(variant, "name"), xml_text(variant, "description"))
                {
                    descriptions.insert(format!("{name}+{variant_name}"), variant_description);
                }
            }
            descriptions.insert(name, description);
        }
        Self { descriptions }
    }

    /// The description of an `xkb` source.
    #[must_use]
    pub fn describe(&self, source: &InputSource) -> Option<String> {
        if source.kind != "xkb" {
            return None;
        }
        self.descriptions.get(&source.id).cloned()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.descriptions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xkb(id: &str) -> InputSource {
        InputSource {
            kind: "xkb".to_owned(),
            id: id.to_owned(),
        }
    }

    #[test]
    fn the_symbols_section_names_each_group() {
        let keymap = "xkb_keymap {\nxkb_keycodes \"evdev+aliases(qwerty)\" {\n};\nxkb_symbols \"pc+no+us:2+de(nodeadkeys):3+inet(evdev)+group(alt_shift_toggle)\" {\n";
        let section = symbols_section_name(keymap).unwrap();
        assert_eq!(
            section,
            "pc+no+us:2+de(nodeadkeys):3+inet(evdev)+group(alt_shift_toggle)"
        );
        assert_eq!(
            layouts_from_symbols_section(section),
            ["no", "us", "de(nodeadkeys)"]
        );
        assert_eq!(layouts_from_symbols_section("pc+gb+inet(evdev)"), ["gb"]);
        assert_eq!(
            layouts_from_symbols_section("pc+no(mac)+inet(evdev)+level3(ralt_switch)"),
            ["no(mac)"]
        );
        assert_eq!(symbols_section_name("xkb_keymap { xkb_symbols {"), None);
        assert_eq!(
            group_names(
                "xkb_symbols \"x\" {\n    name[Group1]=\"Norwegian\";\n    name[Group2]=\"English (US)\";\n"
            ),
            ["Norwegian", "English (US)"]
        );
    }

    #[test]
    fn gsettings_sources_parse() {
        assert_eq!(
            parse_input_sources("[('xkb', 'no'), ('xkb', 'us+mac'), ('ibus', 'anthy')]"),
            [
                xkb("no"),
                xkb("us+mac"),
                InputSource {
                    kind: "ibus".to_owned(),
                    id: "anthy".to_owned()
                }
            ]
        );
        assert!(parse_input_sources("@a(ss) []").is_empty());
        assert_eq!(xkb("us+mac").xkb_layout().as_deref(), Some("us(mac)"));
    }

    #[test]
    fn the_active_group_comes_from_the_most_recent_source() {
        let groups = vec!["no".to_owned(), "us".to_owned()];
        let sources = [xkb("no"), xkb("us")];
        assert_eq!(
            resolve_active_group(&groups, &sources, &[xkb("us"), xkb("no")]),
            GroupResolution::FromInputSources(1)
        );
        assert_eq!(
            resolve_active_group(&groups, &sources, &[]),
            GroupResolution::FromInputSources(0)
        );
        // A most-recent source no longer configured is skipped.
        assert_eq!(
            resolve_active_group(&groups, &sources, &[xkb("de"), xkb("us")]),
            GroupResolution::FromInputSources(1)
        );
        // An IBus source's layout is the engine's: unknown.
        let ibus = InputSource {
            kind: "ibus".to_owned(),
            id: "anthy".to_owned(),
        };
        assert_eq!(
            resolve_active_group(&groups, &[ibus.clone(), xkb("no")], &[ibus]),
            GroupResolution::Unknown
        );
        // No sources at all on a multi-group keymap: unknown.
        assert_eq!(
            resolve_active_group(&groups, &[], &[]),
            GroupResolution::Unknown
        );
        assert_eq!(
            resolve_active_group(&["gb".to_owned()], &[], &[]),
            GroupResolution::Single
        );
        assert_eq!(GroupResolution::Single.group(), Some(0));
        assert_eq!(GroupResolution::Unknown.group(), None);
    }

    #[test]
    fn descriptive_group_names_resolve_through_the_registry() {
        let registry = XkbRegistry::parse(
            "<xkbConfigRegistry><layoutList>\n<layout><configItem><name>no</name>\n<description>Norwegian</description></configItem>\n<variantList><variant><configItem><name>mac</name><description>Norwegian (Macintosh)</description></configItem></variant></variantList></layout>\n<layout><configItem><name>us</name><description>English (US)</description></configItem></layout>\n</layoutList></xkbConfigRegistry>",
        );
        assert_eq!(
            registry.describe(&xkb("no+mac")).as_deref(),
            Some("Norwegian (Macintosh)")
        );
        let names = vec!["English (US)".to_owned(), "Norwegian".to_owned()];
        assert_eq!(
            resolve_active_group_by_names(
                &names,
                &[xkb("us"), xkb("no")],
                &[xkb("no")],
                |source| registry.describe(source)
            ),
            GroupResolution::FromInputSources(1)
        );
        assert_eq!(
            resolve_active_group_by_names(&names, &[], &[], |source| registry.describe(source)),
            GroupResolution::Unknown
        );
    }
}
