use arcen_protocol::messages::BuildIdentityMsg;
use std::path::Path;

#[must_use]
pub fn current() -> BuildIdentityMsg {
    arcen_protocol::build_identity::this_build("arcen-deck-macos", env!("CARGO_PKG_VERSION"))
}

/// Which Deck is running: the bundle's own version strings, the build stamp
/// embedded at compile time, and where the bundle lives. Logged at start and
/// with every connection so a log names the exact build that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckBuildFacts {
    pub bundle_short_version: String,
    pub bundle_version: String,
    pub build_id: String,
    pub source_revision: String,
    pub bundle_path: String,
}

impl DeckBuildFacts {
    /// The facts of the running process.
    #[must_use]
    pub fn running() -> Self {
        use objc2_foundation::{NSBundle, NSString};
        let bundle = NSBundle::mainBundle();
        let info = |key: &str| {
            bundle
                .objectForInfoDictionaryKey(&NSString::from_str(key))
                .and_then(|value| value.downcast::<NSString>().ok())
                .map(|value| value.to_string())
        };
        Self::from_parts(
            info("CFBundleShortVersionString"),
            info("CFBundleVersion"),
            &current(),
            &bundle.bundlePath().to_string(),
            std::env::var_os("HOME").as_deref().map(Path::new),
        )
    }

    /// Assembles the facts; a value the bundle does not carry (a binary run
    /// outside an app bundle) is `unbundled`, and the home directory is
    /// written as `~` so a log does not carry the account name.
    #[must_use]
    pub fn from_parts(
        bundle_short_version: Option<String>,
        bundle_version: Option<String>,
        identity: &BuildIdentityMsg,
        bundle_path: &str,
        home: Option<&Path>,
    ) -> Self {
        let bundle_path = match home.and_then(|home| Path::new(bundle_path).strip_prefix(home).ok())
        {
            Some(relative) if !relative.as_os_str().is_empty() => {
                format!("~/{}", relative.display())
            }
            _ => bundle_path.to_owned(),
        };
        Self {
            bundle_short_version: bundle_short_version.unwrap_or_else(|| "unbundled".to_owned()),
            bundle_version: bundle_version.unwrap_or_else(|| "unbundled".to_owned()),
            build_id: identity.build_id.clone(),
            source_revision: identity.source_revision.clone(),
            bundle_path,
        }
    }

    /// One line for a log message or a support report.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} ({}) build {} rev {} at {}",
            self.bundle_short_version,
            self.bundle_version,
            self.build_id,
            self.source_revision,
            self.bundle_path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> BuildIdentityMsg {
        arcen_protocol::build_identity::from_environment(
            "arcen-deck-macos",
            "0.17.0",
            arcen_protocol::build_identity::BuildEnvironment {
                build_id: Some("20261019T2200Z-0.17.0-ed28caf0"),
                source_revision: Some("ed28caf0"),
                ..Default::default()
            },
            None,
        )
    }

    #[test]
    fn a_bundled_deck_names_its_versions_stamp_and_location_without_the_account() {
        let facts = DeckBuildFacts::from_parts(
            Some("0.17.0".to_owned()),
            Some("0.17.0".to_owned()),
            &identity(),
            "/Users/someone/arcen/dist/macos/Arcen Deck.app",
            Some(Path::new("/Users/someone")),
        );
        assert_eq!(facts.bundle_short_version, "0.17.0");
        assert_eq!(facts.bundle_version, "0.17.0");
        assert_eq!(facts.build_id, "20261019T2200Z-0.17.0-ed28caf0");
        assert_eq!(facts.source_revision, "ed28caf0");
        assert_eq!(facts.bundle_path, "~/arcen/dist/macos/Arcen Deck.app");
        assert!(!facts.summary().contains("someone"));
        assert_eq!(
            facts.summary(),
            "0.17.0 (0.17.0) build 20261019T2200Z-0.17.0-ed28caf0 rev ed28caf0 at ~/arcen/dist/macos/Arcen Deck.app"
        );
    }

    #[test]
    fn an_unbundled_or_unstamped_deck_says_so() {
        let unstamped = arcen_protocol::build_identity::from_environment(
            "arcen-deck-macos",
            "0.17.0",
            arcen_protocol::build_identity::BuildEnvironment::default(),
            None,
        );
        let facts = DeckBuildFacts::from_parts(
            None,
            None,
            &unstamped,
            "/Applications/Arcen Deck.app",
            Some(Path::new("/Users/someone")),
        );
        assert_eq!(facts.bundle_short_version, "unbundled");
        assert_eq!(facts.bundle_version, "unbundled");
        assert_eq!(facts.build_id, "development");
        assert_eq!(facts.source_revision, "unknown");
        assert_eq!(facts.bundle_path, "/Applications/Arcen Deck.app");
    }

    #[test]
    fn the_running_test_binary_reports_facts_without_panicking() {
        let facts = DeckBuildFacts::running();
        assert!(!facts.bundle_path.is_empty());
        assert_eq!(facts.build_id, current().build_id);
    }
}
