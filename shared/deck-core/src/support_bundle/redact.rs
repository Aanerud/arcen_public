// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Redaction for the Deck's support bundle.
//!
//! Three rules, applied to every document the bundle carries:
//!
//! 1. **Secrets** — any JSON key the shared policy calls secret-bearing
//!    (`arcen_telemetry::SupportBundleRedactionPolicy`: key, secret, token,
//!    password, credential, …) has its value replaced by `<redacted>` and an
//!    audit record. Only the Deck's own settings keys that merely *contain*
//!    such a word (`option_key_mode`, `keyboard_shortcuts`, …) are exempt,
//!    by exact name ([`NON_SECRET_KEYS`]).
//! 2. **Usernames** — values of the Deck's username keys
//!    ([`USER_IDENTITY_KEYS`]) and network names (`ssid`) become per-bundle
//!    pseudonyms from the same key the logs use, so one user correlates
//!    across the bundle and never between bundles. Saved host addresses
//!    stay; in the logs the shared transform pseudonymises them
//!    (`arcen_telemetry::BUNDLE_IDENTITY_FIELDS`).
//! 3. **This computer's account** — the user's home folder is replaced by
//!    `~` in every string value, and the account name (and the home
//!    folder's own name) by `<user>` wherever it is a path component; in
//!    binary crash dumps the account and computer names (as whole names)
//!    and the values of identity and secret environment variables are
//!    overwritten in place with `*`, so the file stays readable.

use std::borrow::Cow;

use arcen_telemetry::{
    BundleIdentityKind, BundlePath, BundlePseudonymizer, MAX_REDACTION_KEY_PATH_BYTES,
    MAX_REDACTION_RECORDS, REDACTED_VALUE, RedactionDecision, RedactionReason, RedactionRecord,
    SupportBundleContractError, SupportBundleRedactionPolicy,
};

/// Deck settings keys that contain a secret-sounding word but hold no
/// secret: enum choices about the keyboard.
pub const NON_SECRET_KEYS: &[&str] = &[
    "option_key_mode",
    "option_keys",
    "keyboard",
    "keyboard_capture",
    "keyboard_grab",
    "keyboard_mode",
    "keyboard_shortcuts",
];

/// Keys whose string value is a person's account name, compared without
/// case or underscores (`remembered_username`, `userName`, `user`).
pub const USER_IDENTITY_KEYS: &[&str] = &[
    "rememberedusername",
    "legacyusernamefallback",
    "username",
    "user",
];

/// What replaces this computer's account name in a path.
pub const USER_PLACEHOLDER: &str = "<user>";
/// What replaces this computer's home folder in text.
pub const HOME_PLACEHOLDER: &str = "~";

/// Keys whose value names a machine — a Pier's host name or address in a
/// saved connection (`host`) or the network probe (`target`), and every
/// address-class log field (`arcen_telemetry::BUNDLE_IDENTITY_FIELDS`).
/// Their values go through the one address rule every bundle uses
/// (`BundlePseudonymizer::pseudonymize_address`).
fn is_address_key(key: &str) -> bool {
    matches!(key, "host" | "target" | "peer_addr")
        || matches!(
            arcen_telemetry::bundle_identity_field(key),
            Some(BundleIdentityKind::Host | BundleIdentityKind::PeerAddress)
        )
}

/// A saved connection's ID: a legacy one is a hash of the host and the
/// connection's other settings, so it could be brute-forced back to the
/// host; every ID becomes a per-bundle pseudonym (the same for an ID and
/// `last_selected_connection_id`, so they still match within the bundle).
fn is_connection_id(entry: &BundlePath, parent: &str, key: &str) -> bool {
    entry.as_str() == "config/connections.json"
        && (key == "last_selected_connection_id"
            || (key == "id" && parent.starts_with("/connections/")))
}

/// Maps whose keys are machine addresses: `trusted_pins.json`'s `pins`
/// (`"<host>:<port>": {…}`).
const ADDRESS_KEYED_MAPS: &[&str] = &["/pins"];

fn is_user_identity_key(key: &str) -> bool {
    let normalised: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    USER_IDENTITY_KEYS.contains(&normalised.as_str())
}

/// This computer's account, whose traces the bundle removes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalIdentity {
    /// The home folder (`/Users/alice`, `C:\Users\alice`).
    pub home: Option<String>,
    /// The account name (`alice`).
    pub user: Option<String>,
    /// This computer's name, removed from binary crash dumps (their
    /// environment block); text keeps host names as the logs do.
    pub computer: Option<String>,
}

impl LocalIdentity {
    /// From the process environment (`HOME`/`USERPROFILE`, `USER`/`USERNAME`,
    /// `COMPUTERNAME`/`HOSTNAME`).
    #[must_use]
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        Self {
            home: var("HOME").or_else(|| var("USERPROFILE")),
            user: var("USER").or_else(|| var("USERNAME")),
            computer: var("COMPUTERNAME").or_else(|| var("HOSTNAME")),
        }
    }
}

/// Removes this computer's home folder and account name from text and
/// bytes. Matching ignores ASCII case (Windows paths do). The account name
/// is replaced only as a whole path component (after `/` or `\`), never
/// as free text, so an account called `User` or `deck` leaves `USER32.dll`,
/// JSON keys and ordinary words alone.
#[derive(Debug, Clone, Default)]
pub struct Scrubber {
    /// Home folder forms (raw, JSON-escaped, forward slashes), longest first.
    homes: Vec<Vec<u8>>,
    /// Account names: the account and the home folder's last component.
    names: Vec<Vec<u8>>,
    /// This computer's name, overwritten in binary dumps only (also before
    /// a domain suffix: `DESK-42.example`).
    computer: Option<Vec<u8>>,
}

impl Scrubber {
    /// The scrubber for `identity`.
    #[must_use]
    pub fn new(identity: &LocalIdentity) -> Self {
        let mut homes: Vec<Vec<u8>> = Vec::new();
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut add_name = |name: &str| {
            let name = name.trim();
            if !name.is_empty()
                && !name.contains(['/', '\\', '"'])
                && !names
                    .iter()
                    .any(|known| eq_ascii_ci(known, name.as_bytes()))
            {
                names.push(name.as_bytes().to_vec());
            }
        };
        if let Some(home) = identity
            .home
            .as_deref()
            .map(|home| home.trim_end_matches(['/', '\\']))
            .filter(|home| home.len() > 3)
        {
            homes.push(home.as_bytes().to_vec());
            if home.contains('\\') {
                homes.push(home.replace('\\', "\\\\").into_bytes());
                homes.push(home.replace('\\', "/").into_bytes());
            }
            if let Some(last) = home.rsplit(['/', '\\']).next() {
                add_name(last);
            }
        }
        if let Some(user) = identity.user.as_deref() {
            add_name(user);
        }
        homes.sort_by_key(|home| std::cmp::Reverse(home.len()));
        names.sort_by_key(|name| std::cmp::Reverse(name.len()));
        let computer = identity
            .computer
            .as_deref()
            .map(str::trim)
            .map(|computer| computer.split('.').next().unwrap_or(computer))
            .filter(|computer| !computer.is_empty())
            .map(|computer| computer.as_bytes().to_vec());
        Self {
            homes,
            names,
            computer,
        }
    }

    /// `text` with the home folder replaced by `~` and the account name, as
    /// a path component, by `<user>`.
    #[must_use]
    pub fn scrub_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.scrub_bytes(text.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(text),
            // Whole UTF-8 patterns matched on ASCII case only: still UTF-8.
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    /// [`Self::scrub_text`] on raw bytes, for a log before the shared
    /// transform reads it: this computer's home folder (raw or
    /// JSON-escaped) becomes `~` there, so the transform's home-folder
    /// pseudonym is left for other accounts' folders. JSON stays valid: the
    /// replacements are plain string content.
    #[must_use]
    pub fn scrub_bytes<'a>(&self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        let mut current: Cow<'a, [u8]> = Cow::Borrowed(bytes);
        for home in &self.homes {
            if let Some(replaced) = replace_ascii_ci(&current, home, HOME_PLACEHOLDER, false) {
                current = Cow::Owned(replaced);
            }
        }
        for name in &self.names {
            if let Some(replaced) = replace_ascii_ci(&current, name, USER_PLACEHOLDER, true) {
                current = Cow::Owned(replaced);
            }
        }
        current
    }

    /// Scrubs every string value of each JSON line in `jsonl` (keys and
    /// structure untouched); lines that do not parse are dropped.
    #[must_use]
    pub fn scrub_jsonl(&self, jsonl: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(jsonl.len());
        for line in jsonl.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            self.scrub_strings(&mut value);
            if serde_json::to_writer(&mut output, &value).is_ok() {
                output.push(b'\n');
            }
        }
        output
    }

    fn scrub_strings(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => {
                if let Cow::Owned(scrubbed) = self.scrub_text(text) {
                    *text = scrubbed;
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    self.scrub_strings(value);
                }
            }
            serde_json::Value::Object(object) => {
                for value in object.values_mut() {
                    self.scrub_strings(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    /// Overwrites, in place and at the same length, the account name and
    /// this computer's name wherever they stand as a whole name (not inside
    /// `Users`, `USER32` or `Administrators`), and every environment-block
    /// value whose variable names an identity or a secret (`USERNAME=`,
    /// `COMPUTERNAME=`, `GH_TOKEN=`, …), in ASCII and UTF-16LE. Returns
    /// how many.
    pub fn scrub_binary(&self, bytes: &mut [u8]) -> usize {
        let mut count = scrub_environment(bytes, 1) + scrub_environment(bytes, 2);
        let names = self
            .names
            .iter()
            .map(|name| (name, false))
            .chain(self.computer.iter().map(|name| (name, true)));
        for (name, host) in names {
            let Ok(text) = std::str::from_utf8(name) else {
                continue;
            };
            let wide: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            count += overwrite_components(bytes, name, 1, host);
            count += overwrite_components(bytes, &wide, 2, host);
        }
        count
    }
}

/// Environment variables whose value names the person or the machine.
const IDENTITY_VARIABLES: &[&str] = &[
    "USERNAME",
    "USER",
    "LOGNAME",
    "COMPUTERNAME",
    "HOSTNAME",
    "USERDOMAIN",
    "USERDOMAIN_ROAMINGPROFILE",
    "LOGONSERVER",
    "CLIENTNAME",
    "PROMPT",
];

/// Overwrites the values of identity and secret variables in every
/// `NAME=value` + NUL entry of an environment block (`unit`-byte chars).
fn scrub_environment(bytes: &mut [u8], unit: usize) -> usize {
    let char_at = |bytes: &[u8], index: usize| -> Option<u8> {
        let slice = bytes.get(index..index + unit)?;
        (unit == 1 || slice[1] == 0).then_some(slice[0])
    };
    let is_key = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'(' | b')');
    let mut count = 0;
    let mut index = unit;
    while index < bytes.len() {
        // An entry starts after a NUL.
        if char_at(bytes, index - unit) != Some(0) || !char_at(bytes, index).is_some_and(is_key) {
            index += unit;
            continue;
        }
        let mut end = index;
        let mut key = String::new();
        while let Some(byte) = char_at(bytes, end).filter(|byte| is_key(*byte)) {
            key.push(char::from(byte));
            end += unit;
            if key.len() > 64 {
                break;
            }
        }
        if key.len() > 64 || char_at(bytes, end) != Some(b'=') {
            index = end.max(index + unit);
            continue;
        }
        let value_start = end + unit;
        let mut value_end = value_start;
        while value_end + unit <= bytes.len()
            && bytes[value_end..value_end + unit]
                .iter()
                .any(|byte| *byte != 0)
            && value_end - value_start < 32_768 * unit
        {
            value_end += unit;
        }
        let upper = key.to_ascii_uppercase();
        let sensitive = IDENTITY_VARIABLES.contains(&upper.as_str())
            || matches!(
                SupportBundleRedactionPolicy::classify_key(&key),
                RedactionDecision::Redact(_)
            );
        if sensitive && value_end > value_start {
            for (offset, byte) in bytes[value_start..value_end].iter_mut().enumerate() {
                *byte = if offset % unit == 0 { b'*' } else { 0 };
            }
            count += 1;
        }
        index = value_end.max(index + unit);
    }
    count
}

/// Overwrites `pattern` where it stands as a whole name (neither
/// neighbour is a name character) in text of `unit`-byte characters
/// (1 = ASCII/UTF-8, 2 = UTF-16LE); a `host` name may be followed by its
/// domain (`.`).
fn overwrite_components(bytes: &mut [u8], pattern: &[u8], unit: usize, host: bool) -> usize {
    let char_at = |bytes: &[u8], index: usize| -> Option<u8> {
        let slice = bytes.get(index..index + unit)?;
        (unit == 1 || slice[1] == 0).then_some(slice[0])
    };
    let mut count = 0;
    let mut index = 0;
    while index + pattern.len() <= bytes.len() {
        let end = index + pattern.len();
        let before = index
            .checked_sub(unit)
            .and_then(|previous| char_at(bytes, previous));
        let matches = eq_ascii_ci(&bytes[index..end], pattern)
            && before.is_none_or(|previous| !is_name_byte(previous))
            && char_at(bytes, end).is_none_or(|next| (host && next == b'.') || !is_name_byte(next));
        if matches {
            for (offset, byte) in bytes[index..end].iter_mut().enumerate() {
                if offset % unit == 0 {
                    *byte = b'*';
                }
            }
            count += 1;
            index = end;
        } else {
            index += unit;
        }
    }
    count
}

fn eq_ascii_ci(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Bytes that continue an account name (so `alice` does not match inside
/// `alice.smith`).
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') || byte >= 0x80
}

/// Replaces `pattern` (ASCII case ignored); with `component`, only where it
/// follows `/` or `\` and is not followed by another name byte.
fn replace_ascii_ci(
    haystack: &[u8],
    pattern: &[u8],
    replacement: &str,
    component: bool,
) -> Option<Vec<u8>> {
    if pattern.is_empty() || haystack.len() < pattern.len() {
        return None;
    }
    let mut output = Vec::with_capacity(haystack.len());
    let mut index = 0;
    let mut replaced = false;
    while index < haystack.len() {
        let end = index + pattern.len();
        let matches = end <= haystack.len()
            && eq_ascii_ci(&haystack[index..end], pattern)
            && (!component
                || (index > 0
                    && matches!(haystack[index - 1], b'/' | b'\\')
                    && haystack.get(end).is_none_or(|next| !is_name_byte(*next))));
        if matches {
            output.extend_from_slice(replacement.as_bytes());
            index = end;
            replaced = true;
        } else {
            output.push(haystack[index]);
            index += 1;
        }
    }
    replaced.then_some(output)
}

/// Applies the three rules to one JSON document and returns the audit
/// records (sorted by key path).
///
/// # Errors
///
/// A key path or record count past the shared schema bounds.
pub fn redact_deck_json(
    entry: &BundlePath,
    document: &mut serde_json::Value,
    pseudonymizer: &BundlePseudonymizer,
    scrubber: &Scrubber,
) -> Result<Vec<RedactionRecord>, SupportBundleContractError> {
    let mut records = Vec::new();
    walk(entry, document, "", pseudonymizer, scrubber, &mut records)?;
    records.sort_by(|left, right| left.key_path.cmp(&right.key_path));
    Ok(records)
}

fn walk(
    entry: &BundlePath,
    value: &mut serde_json::Value,
    parent: &str,
    pseudonymizer: &BundlePseudonymizer,
    scrubber: &Scrubber,
    records: &mut Vec<RedactionRecord>,
) -> Result<(), SupportBundleContractError> {
    match value {
        serde_json::Value::Object(object) if ADDRESS_KEYED_MAPS.contains(&parent) => {
            let renamed: serde_json::Map<String, serde_json::Value> = std::mem::take(object)
                .into_iter()
                .map(|(key, child)| (pseudonymizer.pseudonymize_address(&key), child))
                .collect();
            *object = renamed;
            if !object.is_empty() {
                push(
                    records,
                    entry,
                    parent.to_owned(),
                    RedactionReason::IdentityPseudonymized,
                )?;
            }
            for (key, child) in object.iter_mut() {
                let key_path = child_path(parent, &key.replace('~', "~0").replace('/', "~1"))?;
                walk(entry, child, &key_path, pseudonymizer, scrubber, records)?;
            }
        }
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let key_path = child_path(parent, &key.replace('~', "~0").replace('/', "~1"))?;
                let exempt = NON_SECRET_KEYS.contains(&key.as_str());
                if !exempt
                    && let RedactionDecision::Redact(reason) =
                        SupportBundleRedactionPolicy::classify_key(key)
                {
                    *child = serde_json::Value::String(REDACTED_VALUE.to_owned());
                    push(records, entry, key_path, reason)?;
                    continue;
                }
                let identity = if is_user_identity_key(key) {
                    Some(BundleIdentityKind::User)
                } else if key == "ssid" {
                    Some(BundleIdentityKind::NetworkIdentity)
                } else {
                    None
                };
                if let serde_json::Value::String(text) = &*child
                    && let Some(pseudonym) =
                        machine_pseudonym(entry, parent, key, text, pseudonymizer)
                {
                    *child = serde_json::Value::String(pseudonym);
                    push(
                        records,
                        entry,
                        key_path,
                        RedactionReason::IdentityPseudonymized,
                    )?;
                    continue;
                }
                match (identity, &*child) {
                    (Some(kind), serde_json::Value::String(text)) => {
                        *child = serde_json::Value::String(pseudonymizer.pseudonymize(kind, text));
                        push(
                            records,
                            entry,
                            key_path,
                            RedactionReason::IdentityPseudonymized,
                        )?;
                    }
                    (Some(kind), serde_json::Value::Object(_) | serde_json::Value::Array(_)) => {
                        let text = child.to_string();
                        *child = serde_json::Value::String(pseudonymizer.pseudonymize(kind, &text));
                        push(
                            records,
                            entry,
                            key_path,
                            RedactionReason::IdentityPseudonymized,
                        )?;
                    }
                    _ => walk(entry, child, &key_path, pseudonymizer, scrubber, records)?,
                }
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter_mut().enumerate() {
                let key_path = child_path(parent, &index.to_string())?;
                walk(entry, child, &key_path, pseudonymizer, scrubber, records)?;
            }
        }
        serde_json::Value::String(text) => {
            if let Cow::Owned(scrubbed) = scrubber.scrub_text(text) {
                *text = scrubbed;
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
    Ok(())
}

/// The pseudonym for a value that names a machine or a saved connection,
/// when `key` holds one.
fn machine_pseudonym(
    entry: &BundlePath,
    parent: &str,
    key: &str,
    text: &str,
    pseudonymizer: &BundlePseudonymizer,
) -> Option<String> {
    if is_connection_id(entry, parent, key) {
        Some(pseudonymizer.pseudonymize(BundleIdentityKind::Host, text))
    } else if is_address_key(key) {
        Some(pseudonymizer.pseudonymize_address(text))
    } else {
        None
    }
}

fn child_path(parent: &str, key: &str) -> Result<String, SupportBundleContractError> {
    let path = format!("{parent}/{key}");
    if path.len() > MAX_REDACTION_KEY_PATH_BYTES {
        return Err(SupportBundleContractError::InvalidRedactionKeyPath);
    }
    Ok(path)
}

fn push(
    records: &mut Vec<RedactionRecord>,
    entry: &BundlePath,
    key_path: String,
    reason: RedactionReason,
) -> Result<(), SupportBundleContractError> {
    if records.len() >= MAX_REDACTION_RECORDS {
        return Err(SupportBundleContractError::TooManyRedactions);
    }
    records.push(RedactionRecord {
        entry_path: entry.clone(),
        key_path,
        reason,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_telemetry::BundlePseudonymKey;

    fn pseudonymizer() -> BundlePseudonymizer {
        BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([7; 32]))
    }

    fn mac() -> Scrubber {
        Scrubber::new(&LocalIdentity {
            home: Some("/Users/alice".to_owned()),
            user: Some("alice".to_owned()),
            computer: None,
        })
    }

    #[test]
    fn secrets_go_usernames_are_pseudonymised_keyboard_choices_stay() {
        let entry = BundlePath::new("config/connections.json").unwrap();
        let mut document = serde_json::json!({
            "connections": [{
                "host": "203.0.113.82",
                "name": "Studio",
                "password": "hunter2",
                "settings": {
                    "remembered_username": "alice.smith",
                    "keyboard_shortcuts": "automatic",
                    "auth_token": {"value": "abc"},
                },
            }],
            "option_key_mode": "automatic",
            "ssid": "Home Wi-Fi",
            "private_key": "-----BEGIN",
        });
        let records = redact_deck_json(&entry, &mut document, &pseudonymizer(), &mac()).unwrap();
        let text = document.to_string();
        for secret in ["hunter2", "abc", "alice.smith", "Home Wi-Fi", "BEGIN"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(
            document["connections"][0]["host"],
            pseudonymizer().pseudonymize_address("203.0.113.82")
        );
        assert_eq!(document["option_key_mode"], "automatic");
        assert_eq!(
            document["connections"][0]["settings"]["keyboard_shortcuts"],
            "automatic"
        );
        assert_eq!(
            document["connections"][0]["settings"]["remembered_username"],
            pseudonymizer().pseudonymize(BundleIdentityKind::User, "alice.smith")
        );
        let paths: Vec<_> = records
            .iter()
            .map(|record| (record.key_path.as_str(), record.reason))
            .collect();
        assert_eq!(
            paths,
            [
                (
                    "/connections/0/host",
                    RedactionReason::IdentityPseudonymized
                ),
                ("/connections/0/password", RedactionReason::SensitiveKey),
                (
                    "/connections/0/settings/auth_token",
                    RedactionReason::SensitiveKey
                ),
                (
                    "/connections/0/settings/remembered_username",
                    RedactionReason::IdentityPseudonymized
                ),
                ("/private_key", RedactionReason::SensitiveKey),
                ("/ssid", RedactionReason::IdentityPseudonymized),
            ]
        );
    }

    #[test]
    fn the_home_folder_and_account_name_leave_paths_only() {
        let scrubber = mac();
        assert_eq!(
            scrubber.scrub_text(
                "/Users/Alice/Library/Logs/Arcen; /srv/alice/y; alice, malice, /alice.smith"
            ),
            "~/Library/Logs/Arcen; /srv/<user>/y; alice, malice, /alice.smith"
        );
        let windows = Scrubber::new(&LocalIdentity {
            home: Some(r"C:\Users\artist".to_owned()),
            user: Some("artist".to_owned()),
            computer: None,
        });
        assert_eq!(
            windows.scrub_text(r#"{"p":"C:\\Users\\artist\\AppData","q":"C:\Users\ARTIST\x"}"#),
            r#"{"p":"~\\AppData","q":"~\x"}"#
        );
        let short = Scrubber::new(&LocalIdentity {
            home: Some("/home/dev".to_owned()),
            user: Some("dev".to_owned()),
            computer: None,
        });
        assert_eq!(
            short.scrub_text("/home/dev/x /srv/dev/y dev"),
            "~/x /srv/<user>/y dev"
        );
    }

    #[test]
    fn a_generic_account_name_leaves_keys_words_and_system_files_alone() {
        let scrubber = Scrubber::new(&LocalIdentity {
            home: Some(r"C:\Users\User".to_owned()),
            user: Some("User".to_owned()),
            computer: None,
        });
        let line = br#"{"user":null,"role":"client","message":"user signed in","fields":{"p":"D:\\Profiles\\User\\x","dll":"C:\\Windows\\System32\\USER32.dll"}}"#;
        let value: serde_json::Value = serde_json::from_slice(&scrubber.scrub_jsonl(line)).unwrap();
        assert!(value.get("user").is_some());
        assert_eq!(value["message"], "user signed in");
        assert_eq!(value["fields"]["p"], r"D:\Profiles\<user>\x");
        assert_eq!(value["fields"]["dll"], r"C:\Windows\System32\USER32.dll");
        let mut dump: Vec<u8> = r"C:\Users\User\AppData USER32.dll \Users\x"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(scrubber.scrub_binary(&mut dump), 1);
        let wide: Vec<u16> = dump
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            String::from_utf16_lossy(&wide),
            r"C:\Users\****\AppData USER32.dll \Users\x"
        );
    }

    #[test]
    fn a_dumps_environment_block_loses_identities_and_secrets_but_keeps_the_rest() {
        let scrubber = Scrubber::new(&LocalIdentity {
            home: Some(r"C:\Users\admin".to_owned()),
            user: Some("admin".to_owned()),
            computer: Some("DESK-42".to_owned()),
        });
        let block = "\0USERNAME=admin\0COMPUTERNAME=DESK-42\0GH_TOKEN=ghp_abc\0PROMPT=admin@DESK-42 $P$G\0PATH=C:\\Windows\0Administrators admin@DESK-42.example\0";
        for unit in [1, 2] {
            let mut bytes: Vec<u8> = if unit == 1 {
                block.as_bytes().to_vec()
            } else {
                block.encode_utf16().flat_map(u16::to_le_bytes).collect()
            };
            let length = bytes.len();
            assert!(scrubber.scrub_binary(&mut bytes) >= 5);
            assert_eq!(bytes.len(), length);
            let text = if unit == 1 {
                String::from_utf8_lossy(&bytes).into_owned()
            } else {
                String::from_utf16_lossy(
                    &bytes
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>(),
                )
            };
            for gone in ["=admin", "DESK-42", "ghp_abc", "admin@"] {
                assert!(!text.contains(gone), "{gone} in {text:?}");
            }
            assert!(text.contains("PATH=C:\\Windows"), "{text:?}");
            assert!(text.contains("Administrators"), "{text:?}");
        }
    }

    #[test]
    fn crash_dumps_keep_their_length_and_lose_the_account_name() {
        let mut dump = b"MDMP..C:\\Users\\Alice\\x".to_vec();
        dump.extend(
            "C:\\Users\\alice\\y"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let length = dump.len();
        assert_eq!(mac().scrub_binary(&mut dump), 2);
        assert_eq!(dump.len(), length);
        let ascii = String::from_utf8_lossy(&dump).to_ascii_lowercase();
        assert!(!ascii.contains("alice"));
        let wide: Vec<u16> = dump[22..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16_lossy(&wide), "C:\\Users\\*****\\y");
    }
}
