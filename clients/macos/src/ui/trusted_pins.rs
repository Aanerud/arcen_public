//! Remembered host identities.
//!
//! Trust-on-first-use derives all of its security from one property: after the
//! first connection, a *change* in the host's identity is a hard failure. That
//! property needs a baseline that outlives the process. Without one, quitting
//! the Deck resets the world to "no host is known", and the fingerprint prompt
//! reappears on every launch — which trains a user to dismiss it, and means an
//! impostor presenting a fresh certificate raises exactly the same dialog the
//! real host raised yesterday.
//!
//! That matters here more than it would elsewhere. A Deck session carries the
//! user's OS password to the Pier's PAM or `LogonUser`, and Arcen's Piers are
//! routinely domain-joined, so the credential at stake is a domain credential.
//!
//! Pins are keyed by endpoint rather than by saved-connection id on purpose. A
//! user who types a host into quick-connect is exposed to the same substitution
//! as one who clicks a saved entry, and endpoint keying protects both with one
//! mechanism.
//!
//! Nothing here is secret: a certificate fingerprint is public information. The
//! file is integrity-relevant, not confidential — someone who can rewrite it can
//! redirect trust, but so can someone who can rewrite any of the Deck's config.

use std::collections::HashMap;
use std::path::Path;

use arcen_transport::tls::{PinKind, TlsPin};
use serde::{Deserialize, Serialize};

/// Bumped only for a format change that older builds cannot read.
const TRUSTED_PINS_VERSION: u32 = 1;

const KIND_SPKI: &str = "spki_sha256";
const KIND_CERTIFICATE: &str = "certificate_sha256";

/// One remembered identity, as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredPin {
    /// `spki_sha256` or `certificate_sha256`.
    pub kind: String,
    /// Lowercase hex SHA-256.
    pub digest: String,
    /// RFC 3339 timestamp of the user's decision, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<String>,
    /// What the user called the host when they trusted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// On-disk shape of `trusted_pins.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPinStore {
    pub version: u32,
    #[serde(default)]
    pub pins: HashMap<String, StoredPin>,
}

impl Default for TrustedPinStore {
    fn default() -> Self {
        Self {
            version: TRUSTED_PINS_VERSION,
            pins: HashMap::new(),
        }
    }
}

fn encode_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_digest(value: &str) -> Option<[u8; 32]> {
    let trimmed = value.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, slot) in digest.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&trimmed[start..start + 2], 16).ok()?;
    }
    Some(digest)
}

impl StoredPin {
    /// Rebuilds the typed pin, or `None` if the entry is unusable.
    ///
    /// An unreadable entry is dropped rather than defaulted. Guessing a pin
    /// kind would silently compare an SPKI digest against a certificate digest,
    /// which fails closed but reports the wrong reason; guessing the other way
    /// could accept the wrong host.
    #[must_use]
    pub fn to_pin(&self) -> Option<TlsPin> {
        let kind = match self.kind.as_str() {
            KIND_SPKI => PinKind::SubjectPublicKeyInfoSha256,
            KIND_CERTIFICATE => PinKind::CertificateSha256,
            _ => return None,
        };
        Some(TlsPin::new(kind, decode_digest(&self.digest)?))
    }

    /// Builds a storable entry from a live pin.
    #[must_use]
    pub fn from_pin(pin: &TlsPin, label: Option<String>, pinned_at: Option<String>) -> Self {
        let kind = match pin.kind {
            PinKind::SubjectPublicKeyInfoSha256 => KIND_SPKI,
            PinKind::CertificateSha256 => KIND_CERTIFICATE,
        };
        Self {
            kind: kind.to_string(),
            digest: encode_digest(&pin.digest),
            pinned_at,
            label,
        }
    }
}

impl TrustedPinStore {
    /// Typed view of every readable entry.
    #[must_use]
    pub fn resolved(&self) -> HashMap<String, TlsPin> {
        self.pins
            .iter()
            .filter_map(|(endpoint, stored)| Some((endpoint.clone(), stored.to_pin()?)))
            .collect()
    }

    /// Remembers `pin` for `endpoint`, replacing any previous entry.
    pub fn remember(&mut self, endpoint: &str, pin: &TlsPin, label: Option<String>) {
        self.pins.insert(
            endpoint.to_string(),
            StoredPin::from_pin(pin, label, Some(now_rfc3339())),
        );
    }

    /// Forgets `endpoint`, returning whether anything was removed.
    pub fn forget(&mut self, endpoint: &str) -> bool {
        self.pins.remove(endpoint).is_some()
    }
}

/// Seconds-resolution RFC 3339, without pulling in a date library.
fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();

    // Civil-from-days, after Howard Hinnant's algorithm.
    let days = (seconds / 86_400) as i64;
    let time_of_day = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// Reads the store, treating absence and corruption alike as "nothing trusted".
///
/// A corrupt file must not be fatal: the Deck has to stay usable, and the
/// failure mode of an empty store is a fingerprint prompt, which is safe. It
/// must equally never be silent, or a user whose pins vanished would have no
/// way to tell that from a host that genuinely changed.
pub fn load(path: &Path) -> TrustedPinStore {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TrustedPinStore::default();
        }
        Err(error) => {
            tracing::warn!(
                target: crate::logging::target::UI,
                path = %path.display(),
                %error,
                "could not read remembered host identities; every host will prompt again",
            );
            return TrustedPinStore::default();
        }
    };
    match serde_json::from_slice::<TrustedPinStore>(&bytes) {
        Ok(store) if store.version <= TRUSTED_PINS_VERSION => store,
        Ok(store) => {
            tracing::warn!(
                target: crate::logging::target::UI,
                found = store.version,
                supported = TRUSTED_PINS_VERSION,
                "remembered host identities were written by a newer Deck; ignoring them",
            );
            TrustedPinStore::default()
        }
        Err(error) => {
            tracing::warn!(
                target: crate::logging::target::UI,
                path = %path.display(),
                %error,
                "remembered host identities are unreadable; every host will prompt again",
            );
            TrustedPinStore::default()
        }
    }
}

/// Writes the store, creating the directory if needed.
///
/// Written through a temporary file and renamed, so an interrupted write cannot
/// leave a truncated file that would silently forget every host.
pub fn save(path: &Path, store: &TrustedPinStore) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_vec_pretty(store)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, &serialized)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(kind: PinKind, seed: u8) -> TlsPin {
        TlsPin::new(kind, [seed; 32])
    }

    #[test]
    fn a_remembered_pin_survives_a_round_trip() {
        let mut store = TrustedPinStore::default();
        let original = pin(PinKind::SubjectPublicKeyInfoSha256, 0xAB);
        store.remember("pier.example.internal:18444", &original, None);

        let encoded = serde_json::to_vec(&store).expect("serialize");
        let decoded: TrustedPinStore = serde_json::from_slice(&encoded).expect("deserialize");

        assert_eq!(
            decoded.resolved().get("pier.example.internal:18444"),
            Some(&original)
        );
    }

    #[test]
    fn certificate_and_spki_pins_do_not_collapse_into_each_other() {
        // The kinds share a digest length. If the kind were dropped or guessed,
        // an SPKI pin could be compared against a certificate digest, which is
        // a different assertion about a different host property.
        let mut store = TrustedPinStore::default();
        store.remember("a:1", &pin(PinKind::SubjectPublicKeyInfoSha256, 7), None);
        store.remember("b:1", &pin(PinKind::CertificateSha256, 7), None);

        let resolved = store.resolved();
        assert_eq!(
            resolved["a:1"].kind,
            PinKind::SubjectPublicKeyInfoSha256,
            "spki pin changed kind"
        );
        assert_eq!(
            resolved["b:1"].kind,
            PinKind::CertificateSha256,
            "certificate pin changed kind"
        );
        assert_ne!(resolved["a:1"], resolved["b:1"]);
    }

    #[test]
    fn unreadable_entries_are_dropped_rather_than_guessed() {
        for (kind, digest) in [
            ("spki_sha256", "not-hex"),
            ("spki_sha256", "ab"),
            ("wat", &"ab".repeat(32) as &str),
            ("certificate_sha256", &"zz".repeat(32) as &str),
        ] {
            let stored = StoredPin {
                kind: kind.to_string(),
                digest: digest.to_string(),
                pinned_at: None,
                label: None,
            };
            assert!(
                stored.to_pin().is_none(),
                "kind={kind} digest={digest} should not resolve"
            );
        }
    }

    #[test]
    fn forgetting_reports_whether_anything_was_there() {
        let mut store = TrustedPinStore::default();
        store.remember("a:1", &pin(PinKind::SubjectPublicKeyInfoSha256, 1), None);
        assert!(store.forget("a:1"));
        assert!(!store.forget("a:1"));
        assert!(store.resolved().is_empty());
    }

    #[test]
    fn a_corrupt_file_reads_as_nothing_trusted_rather_than_failing() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-pins-corrupt-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create");
        let path = directory.join("trusted_pins.json");
        std::fs::write(&path, b"{ this is not json").expect("write");

        // Empty means every host prompts, which is safe. The alternative --
        // refusing to start, or defaulting to trusting -- is not.
        assert!(load(&path).resolved().is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_store_from_a_newer_deck_is_ignored_not_misread() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-pins-newer-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create");
        let path = directory.join("trusted_pins.json");
        std::fs::write(
            &path,
            br#"{"version":99,"pins":{"a:1":{"kind":"spki_sha256","digest":"00"}}}"#,
        )
        .expect("write");

        assert!(load(&path).resolved().is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn saving_then_loading_preserves_every_pin() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-pins-roundtrip-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = directory.join("trusted_pins.json");

        let mut store = TrustedPinStore::default();
        store.remember(
            "one.example.internal:18444",
            &pin(PinKind::SubjectPublicKeyInfoSha256, 0x11),
            Some("One".to_string()),
        );
        store.remember(
            "two.example.internal:18444",
            &pin(PinKind::CertificateSha256, 0x22),
            None,
        );
        save(&path, &store).expect("save");

        let reloaded = load(&path).resolved();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(
            reloaded["one.example.internal:18444"],
            pin(PinKind::SubjectPublicKeyInfoSha256, 0x11)
        );
        assert_eq!(
            reloaded["two.example.internal:18444"],
            pin(PinKind::CertificateSha256, 0x22)
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_timestamp_is_recorded_so_the_user_can_see_when_they_decided() {
        let mut store = TrustedPinStore::default();
        store.remember("a:1", &pin(PinKind::SubjectPublicKeyInfoSha256, 1), None);
        let stamp = store.pins["a:1"].pinned_at.clone().expect("timestamp");
        assert!(
            stamp.len() == 20 && stamp.ends_with('Z') && stamp.as_bytes()[4] == b'-',
            "expected RFC 3339 UTC, got {stamp}"
        );
        assert!(
            stamp.starts_with("20"),
            "expected a plausible year, got {stamp}"
        );
    }
}
