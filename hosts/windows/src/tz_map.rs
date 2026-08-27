//! Build-generated CLDR IANA-to-Windows time-zone mapping.

include!(concat!(env!("OUT_DIR"), "/windows_zones.rs"));

const RENAMED_IANA_ZONES: &[(&str, &str)] = &[
    ("Africa/Asmara", "Africa/Asmera"),
    ("America/Argentina/Buenos_Aires", "America/Buenos_Aires"),
    ("America/Argentina/Catamarca", "America/Catamarca"),
    ("America/Argentina/Cordoba", "America/Cordoba"),
    ("America/Argentina/Jujuy", "America/Jujuy"),
    ("America/Argentina/Mendoza", "America/Mendoza"),
    ("America/Atikokan", "America/Coral_Harbour"),
    ("America/Indiana/Indianapolis", "America/Indianapolis"),
    ("America/Kentucky/Louisville", "America/Louisville"),
    ("America/Nuuk", "America/Godthab"),
    ("Asia/Ho_Chi_Minh", "Asia/Saigon"),
    ("Asia/Kathmandu", "Asia/Katmandu"),
    ("Asia/Kolkata", "Asia/Calcutta"),
    ("Asia/Yangon", "Asia/Rangoon"),
    ("Atlantic/Faroe", "Atlantic/Faeroe"),
    ("Europe/Kyiv", "Europe/Kiev"),
    ("Pacific/Chuuk", "Pacific/Truk"),
    ("Pacific/Kanton", "Pacific/Enderbury"),
    ("Pacific/Pohnpei", "Pacific/Ponape"),
];

#[must_use]
pub(crate) fn windows_zone(iana: &str) -> Option<&'static str> {
    let lookup = RENAMED_IANA_ZONES
        .binary_search_by_key(&iana, |(modern, _)| modern)
        .map_or(iana, |index| RENAMED_IANA_ZONES[index].1);
    WINDOWS_ZONES
        .binary_search_by_key(&lookup, |(identifier, _)| identifier)
        .ok()
        .map(|index| WINDOWS_ZONES[index].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_table_is_complete_sorted_and_unique() {
        assert_eq!(WINDOWS_ZONES.len(), 445);
        assert!(WINDOWS_ZONES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(RENAMED_IANA_ZONES
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0));
        assert!(WINDOWS_ZONES
            .iter()
            .all(|(iana, windows)| !iana.is_empty() && !windows.is_empty()));
    }

    #[test]
    fn modern_canonical_renames_match_their_cldr_legacy_zones() {
        for (modern, legacy) in RENAMED_IANA_ZONES {
            assert_eq!(
                windows_zone(modern),
                windows_zone(legacy),
                "{modern} -> {legacy}"
            );
            assert!(windows_zone(modern).is_some(), "{modern}");
        }
    }

    #[test]
    fn maps_required_cldr_spot_checks() {
        for (iana, expected) in [
            ("Europe/Oslo", "W. Europe Standard Time"),
            ("America/Los_Angeles", "Pacific Standard Time"),
            ("Asia/Kolkata", "India Standard Time"),
            ("Asia/Kathmandu", "Nepal Standard Time"),
            ("America/Nuuk", "Greenland Standard Time"),
            ("Asia/Yangon", "Myanmar Standard Time"),
            ("Europe/Kyiv", "FLE Standard Time"),
            ("Pacific/Chatham", "Chatham Islands Standard Time"),
        ] {
            assert_eq!(windows_zone(iana), Some(expected), "{iana}");
        }
    }

    #[test]
    fn rejects_unknown_zone() {
        assert_eq!(windows_zone("Arcen/Unknown"), None);
    }
}
