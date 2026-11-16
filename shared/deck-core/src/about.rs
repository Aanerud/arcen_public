// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What every Deck says about itself: the source location and the
//! AGPL-3.0 source offer (About box, `--help`, `--version`).

/// Canonical location of the corresponding source.
///
/// Arcen is AGPL-3.0 free software. Surfacing the source location from inside
/// the running program means a user who only ever receives a built binary can
/// still find the corresponding source.
///
/// This must point at the public repository, where every release is tagged;
/// that is the one that satisfies the offer.
pub const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";

/// AGPL-3.0 section 13 source offer.
///
/// The Deck is the party that connects *to* a Pier over a network, so section
/// 13's obligation mostly runs the other way. It is surfaced here anyway,
/// because a user who received only this binary is entitled to the same
/// pointer, and because the About box and `--help` are the two places a person
/// looks for it.
pub const SOURCE_OFFER: &str = "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
     You may redistribute it under the terms of that licence. If you run a modified version \
     that others connect to over a network, you must offer them its corresponding source.";

/// The About box's copyright line: the macOS Deck's bundle carries it as
/// `NSHumanReadableCopyright` (its standard About panel shows it), the
/// Windows Deck's About window draws it.
pub const COPYRIGHT_LINE: &str = "Copyright (C) 2026 TUTTIFRUTTI AS. Free software under the GNU AGPL-3.0. NO WARRANTY. Source: https://github.com/Aanerud/arcen_public";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copyright_line_is_the_one_the_mac_bundle_shows_and_names_the_source() {
        let plist = include_str!("../../../packaging/macos/Deck-Info.plist");
        assert!(plist.contains(&format!("<string>{COPYRIGHT_LINE}</string>")));
        assert!(COPYRIGHT_LINE.ends_with(SOURCE_URL));
    }
}
