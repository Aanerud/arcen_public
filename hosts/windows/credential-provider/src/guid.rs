//! The pinned class id as a windows-rs `GUID`, kept byte-for-byte in sync with
//! [`crate::registration::CLSID_STRING`].

use windows::core::GUID;

/// `{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}` — the Arcen Credential Provider.
pub const CLSID_ARCEN: GUID = GUID::from_u128(0x2FBE_34F2_9E7A_42FA_BFBF_4489_7694_BE60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_matches_the_canonical_string() {
        // Reconstruct the braced string from the GUID and compare to the source
        // of truth used by the registry keys and install scripts.
        let g = CLSID_ARCEN;
        let d4 = g.data4;
        let rendered = format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            g.data1, g.data2, g.data3, d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
        );
        assert_eq!(rendered, crate::registration::CLSID_STRING);
    }
}
