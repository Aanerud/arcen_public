//! The locale a host-started desktop session runs in.
//!
//! A display manager gives a session its user's language (GDM reads the
//! user's `AccountsService` `Language`) or the system locale. A desktop the
//! Pier starts itself has no display manager, and PAM's `login` stack sets
//! no locale, so without this the whole desktop runs in the `C` locale
//! (ASCII): English whatever the user chose, and non-UTF-8 for every
//! application the shell starts. This module decides the locale from the
//! texts of the system locale file (`/etc/locale.conf` syntax) and the
//! user's `AccountsService` record; reading them is the platform's job.
//!
//! Precedence, as a display manager applies it:
//!
//! - The user's language (`Languages`, or the older `Language`) sets
//!   `LANG`, and the system's `LC_*` overrides are then not applied, so they
//!   cannot override the user's choice.
//! - Without one, the system's `LANG` and `LC_*` (never `LC_ALL`) apply.
//! - The user's `FormatsLocale` (the region chosen for dates, numbers,
//!   currency, measurement and paper) sets those categories in either case.
//!
//! Every value is validated as a locale name; anything else is ignored.

use std::collections::BTreeMap;

/// Locale variables a session may receive. `LC_ALL` is deliberately absent:
/// it would override every category the user or an application sets.
pub const LOCALE_VARIABLES: [&str; 13] = [
    "LANG",
    "LC_CTYPE",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_MESSAGES",
    "LC_PAPER",
    "LC_NAME",
    "LC_ADDRESS",
    "LC_TELEPHONE",
    "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
];

/// The categories a regional-formats locale governs.
pub const FORMATS_VARIABLES: [&str; 5] = [
    "LC_TIME",
    "LC_NUMERIC",
    "LC_MONETARY",
    "LC_MEASUREMENT",
    "LC_PAPER",
];

const MAX_LOCALE_NAME: usize = 64;

/// A syntactically valid locale name: `C`, `POSIX`, or
/// `language[_territory][.codeset][@modifier]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleName(String);

/// Why a locale name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocaleNameError {
    /// Empty, too long, or outside the locale-name grammar.
    Malformed,
}

impl std::fmt::Display for LocaleNameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("malformed locale name")
    }
}

impl std::error::Error for LocaleNameError {}

impl LocaleName {
    /// Parses a locale name.
    ///
    /// # Errors
    ///
    /// [`LocaleNameError::Malformed`] for anything outside the grammar,
    /// including path separators, whitespace and control characters.
    pub fn parse(value: &str) -> Result<Self, LocaleNameError> {
        if value.is_empty() || value.len() > MAX_LOCALE_NAME {
            return Err(LocaleNameError::Malformed);
        }
        let (rest, modifier) = match value.split_once('@') {
            Some((rest, modifier)) => (rest, Some(modifier)),
            None => (value, None),
        };
        let (rest, codeset) = match rest.split_once('.') {
            Some((rest, codeset)) => (rest, Some(codeset)),
            None => (rest, None),
        };
        let (language, territory) = match rest.split_once('_') {
            Some((language, territory)) => (language, Some(territory)),
            None => (rest, None),
        };
        let language_ok = matches!(language, "C" | "POSIX")
            || ((2..=3).contains(&language.len())
                && language.bytes().all(|byte| byte.is_ascii_lowercase()));
        let territory_ok = territory.is_none_or(|territory| {
            (2..=3).contains(&territory.len())
                && territory
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        });
        let codeset_ok = codeset.is_none_or(|codeset| {
            (1..=20).contains(&codeset.len())
                && codeset
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
        let modifier_ok = modifier.is_none_or(|modifier| {
            (1..=20).contains(&modifier.len())
                && modifier.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if language_ok && territory_ok && codeset_ok && modifier_ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(LocaleNameError::Malformed)
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_portable(&self) -> bool {
        let base = self.0.split(['.', '@']).next().unwrap_or_default();
        matches!(base, "C" | "POSIX")
    }

    /// A user's language as `AccountsService` stores it may omit the
    /// codeset (`nb_NO`); a display manager completes it with UTF-8. A bare
    /// language without a territory (`nb`) names no installed locale and is
    /// not used.
    fn completed_user_language(self) -> Option<Self> {
        if self.is_portable() || self.0.contains('.') {
            return Some(self);
        }
        let base = self.0.split('@').next().unwrap_or_default();
        if !base.contains('_') {
            return None;
        }
        let completed = match self.0.split_once('@') {
            Some((base, modifier)) => format!("{base}.UTF-8@{modifier}"),
            None => format!("{}.UTF-8", self.0),
        };
        Self::parse(&completed).ok()
    }
}

/// The locale variables a session receives, in a stable order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionLocale {
    variables: BTreeMap<&'static str, LocaleName>,
}

impl SessionLocale {
    /// Decides the session locale from the system locale file's text and
    /// the user's `AccountsService` record, either of which may be absent.
    #[must_use]
    pub fn resolve(system_locale_conf: Option<&str>, accounts_service_user: Option<&str>) -> Self {
        let (language, formats) =
            accounts_service_user.map_or((None, None), parse_accounts_service_user);
        let mut variables = BTreeMap::new();
        if let Some(language) = language {
            variables.insert("LANG", language);
        } else if let Some(system) = system_locale_conf {
            variables.extend(parse_locale_conf(system));
        }
        if let Some(formats) = formats {
            for name in FORMATS_VARIABLES {
                variables.insert(name, formats.clone());
            }
        }
        Self { variables }
    }

    /// `(variable, locale)` pairs to put into the session's environment.
    pub fn variables(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.variables
            .iter()
            .map(|(name, locale)| (*name, locale.as_str()))
    }

    /// Whether nothing was decided.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }
}

/// `KEY=VALUE` lines of a `locale.conf`, values optionally quoted; comments,
/// unknown keys, `LC_ALL` and invalid names are ignored.
fn parse_locale_conf(text: &str) -> BTreeMap<&'static str, LocaleName> {
    let mut variables = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(name) = LOCALE_VARIABLES
            .iter()
            .copied()
            .find(|name| *name == key.trim())
        else {
            continue;
        };
        if let Ok(locale) = LocaleName::parse(unquote(value.trim())) {
            variables.insert(name, locale);
        }
    }
    variables
}

/// The `[User]` group's language (`Languages` or `Language`) and
/// `FormatsLocale` of an `AccountsService` user record (a `GLib` key file).
/// A list uses its first entry, as `act_user_get_language` does.
fn parse_accounts_service_user(text: &str) -> (Option<LocaleName>, Option<LocaleName>) {
    let mut in_user = false;
    let (mut language, mut formats) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_user = line == "[User]";
            continue;
        }
        if !in_user {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            // `Languages` (as AccountsService 23.13 on Rocky Linux 10 writes
            // it) is a `;`-terminated list; the older `Language` holds one
            // value or a `:` list.
            "Languages" | "Language" => {
                language = value
                    .split([';', ':'])
                    .next()
                    .and_then(|first| LocaleName::parse(first).ok())
                    .and_then(LocaleName::completed_user_language);
            }
            "FormatsLocale" => formats = LocaleName::parse(value).ok(),
            _ => {}
        }
    }
    (language, formats)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(locale: &SessionLocale) -> Vec<(&'static str, String)> {
        locale
            .variables()
            .map(|(name, value)| (name, value.to_owned()))
            .collect()
    }

    #[test]
    fn locale_names_follow_the_grammar() {
        for good in [
            "C",
            "POSIX",
            "C.UTF-8",
            "C.utf8",
            "en_GB.UTF-8",
            "nb_NO.utf8",
            "sr_RS.UTF-8@latin",
            "es_419.UTF-8",
            "ast_ES.UTF-8",
            "de_DE@euro",
        ] {
            assert!(LocaleName::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "en_gb.UTF-8",
            "EN_GB",
            "../../etc/passwd",
            "en_GB.UTF-8 ",
            "en_GB.UTF-8\n",
            "en_GB/UTF-8",
            "en_GB.UTF 8",
            "english_GB",
            "en_GB.@",
            "en_GB.UTF-8@",
            &"a".repeat(65),
        ] {
            assert_eq!(
                LocaleName::parse(bad),
                Err(LocaleNameError::Malformed),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_system_locale_applies_without_a_user_language() {
        let locale = SessionLocale::resolve(
            Some(
                "# comment\nLANG=\"en_GB.UTF-8\"\nLC_TIME='nb_NO.UTF-8'\nLC_ALL=C\nFOO=bar\nLC_PAPER=bad name\n",
            ),
            None,
        );
        assert_eq!(
            pairs(&locale),
            vec![
                ("LANG", "en_GB.UTF-8".to_owned()),
                ("LC_TIME", "nb_NO.UTF-8".to_owned()),
            ]
        );
    }

    #[test]
    fn the_user_language_wins_and_drops_system_overrides() {
        let locale = SessionLocale::resolve(
            Some("LANG=en_US.UTF-8\nLC_MESSAGES=en_US.UTF-8\n"),
            Some("[User]\nLanguage=nb_NO.UTF-8\nXSession=gnome\nSystemAccount=false\n"),
        );
        assert_eq!(pairs(&locale), vec![("LANG", "nb_NO.UTF-8".to_owned())]);
    }

    #[test]
    fn user_formats_set_their_categories() {
        let locale = SessionLocale::resolve(
            Some("LANG=en_US.UTF-8\n"),
            Some("[User]\nFormatsLocale=de_DE.UTF-8\n"),
        );
        let pairs = pairs(&locale);
        assert!(pairs.contains(&("LANG", "en_US.UTF-8".to_owned())));
        for name in FORMATS_VARIABLES {
            assert!(pairs.contains(&(name, "de_DE.UTF-8".to_owned())), "{name}");
        }
        assert!(!pairs.iter().any(|(name, _)| *name == "LC_MESSAGES"));
    }

    #[test]
    fn a_user_language_without_codeset_is_completed_and_a_bare_language_is_not_used() {
        let completed = SessionLocale::resolve(None, Some("[User]\nLanguage=nb_NO\n"));
        assert_eq!(pairs(&completed), vec![("LANG", "nb_NO.UTF-8".to_owned())]);
        let modified = SessionLocale::resolve(None, Some("[User]\nLanguage=sr_RS@latin\n"));
        assert_eq!(
            pairs(&modified),
            vec![("LANG", "sr_RS.UTF-8@latin".to_owned())]
        );
        let bare =
            SessionLocale::resolve(Some("LANG=en_GB.UTF-8\n"), Some("[User]\nLanguage=nb\n"));
        assert_eq!(pairs(&bare), vec![("LANG", "en_GB.UTF-8".to_owned())]);
        let list = SessionLocale::resolve(None, Some("[User]\nLanguage=fr_FR.UTF-8:en_GB.UTF-8\n"));
        assert_eq!(pairs(&list), vec![("LANG", "fr_FR.UTF-8".to_owned())]);
        // As AccountsService 23 writes it (`SetLanguage`), Rocky Linux 10.
        let current = SessionLocale::resolve(
            Some("LANG=en_GB.UTF-8\n"),
            Some("[User]\nLanguages=nb_NO.UTF-8;\nSession=\nSystemAccount=false\n"),
        );
        assert_eq!(pairs(&current), vec![("LANG", "nb_NO.UTF-8".to_owned())]);
    }

    #[test]
    fn keys_outside_the_user_group_and_invalid_values_are_ignored() {
        let locale = SessionLocale::resolve(
            Some("LANG=en_GB.UTF-8\n"),
            Some(
                "[InputSource0]\nLanguage=de_DE.UTF-8\n[User]\nLanguage=$(id)\nFormatsLocale=../x\n",
            ),
        );
        assert_eq!(pairs(&locale), vec![("LANG", "en_GB.UTF-8".to_owned())]);
    }

    #[test]
    fn nothing_known_decides_nothing() {
        assert!(SessionLocale::resolve(None, None).is_empty());
        assert!(SessionLocale::resolve(Some(""), Some("[User]\n")).is_empty());
    }
}
