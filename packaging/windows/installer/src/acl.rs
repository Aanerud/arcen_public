// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Access-control classes for the installed files, and verification of the
//! DACL that was actually applied.
//!
//! Deliberately free of Windows APIs and `#[cfg(windows)]`, so the rules that
//! decide who can read the Pier's private key are unit-testable on any host
//! rather than only on a Windows runner.

/// Owner applied to every installed path, as a SID.
///
/// Installed service paths accept only SYSTEM or Administrators as owner, so
/// this is load-bearing rather than cosmetic.
// The installer applies these by handle; the icacls form remains the manual
// procedure in hosts/windows/INSTALL.md and is kept in step by the tests.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) const OWNER_SID: &str = "*S-1-5-32-544";

/// Well-known SIDs, never display names.
///
/// Account names are localized. On a Norwegian Windows the builtin group is
/// `Administratorer`, so `icacls /grant Administrators:...` fails with "Ingen
/// tilordninger ble gjort mellom kontonavn og sikkerhets-IDer" — no mapping
/// between account names and security IDs — and the install aborts before a
/// single file is copied. icacls accepts a SID wherever it accepts a name,
/// spelled `*S-1-...`, and a SID is identical on every localization.
pub(crate) const SID_SYSTEM: &str = "S-1-5-18";
pub(crate) const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
pub(crate) const SID_USERS: &str = "S-1-5-32-545";
pub(crate) const SID_AUTHENTICATED_USERS: &str = "S-1-5-11";

const PUBLIC_DIR_SDDL: &str = "O:SYG:SYD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;AU)";
const SECRET_DIR_SDDL: &str = "O:SYG:SYD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
const PUBLIC_FILE_SDDL: &str = "O:SYG:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;AU)";
const SECRET_FILE_SDDL: &str = "O:SYG:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)";
/// Writable by the per-session agent, which runs under the signed-in user's
/// token and is not elevated. `0x1301bf` is the Modify right set.
const AGENT_DIR_SDDL: &str = "O:SYG:SYD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;AU)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AclClass {
    PublicDirectory,
    SecretDirectory,
    /// A directory the per-session agent must write.
    ///
    /// The agent runs under the signed-in user's unelevated token, so a secret
    /// ACL here does not harden anything, it simply stops the session starting.
    AgentWritableDirectory,
    PublicFile,
    SecretFile,
}

impl AclClass {
    pub(crate) const fn sddl(self) -> &'static str {
        match self {
            Self::PublicDirectory => PUBLIC_DIR_SDDL,
            Self::SecretDirectory => SECRET_DIR_SDDL,
            Self::AgentWritableDirectory => AGENT_DIR_SDDL,
            Self::PublicFile => PUBLIC_FILE_SDDL,
            Self::SecretFile => SECRET_FILE_SDDL,
        }
    }

    /// icacls `/grant:r` arguments, addressed by SID so they are identical on
    /// every display language.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) const fn grants(self) -> &'static [&'static str] {
        match self {
            Self::PublicDirectory => &[
                "*S-1-5-18:(OI)(CI)(F)",
                "*S-1-5-32-544:(OI)(CI)(F)",
                "*S-1-5-11:(OI)(CI)(RX)",
            ],
            Self::SecretDirectory => &["*S-1-5-18:(OI)(CI)(F)", "*S-1-5-32-544:(OI)(CI)(F)"],
            Self::AgentWritableDirectory => &[
                "*S-1-5-18:(OI)(CI)(F)",
                "*S-1-5-32-544:(OI)(CI)(F)",
                "*S-1-5-11:(OI)(CI)(M)",
            ],
            Self::PublicFile => &["*S-1-5-18:(F)", "*S-1-5-32-544:(F)", "*S-1-5-11:(RX)"],
            Self::SecretFile => &["*S-1-5-18:(F)", "*S-1-5-32-544:(F)"],
        }
    }

    /// Broad trustees that an existing ACL may carry but this class forbids.
    ///
    /// `icacls /grant:r` replaces ACEs only for trustees named in that command;
    /// it does not remove an explicit ACE for a different trustee. The first
    /// binary installer used BUILTIN\Users for public paths while the earlier
    /// manual install used Authenticated Users, so moving between those paths
    /// otherwise leaves both ACEs behind and makes strict verification fail.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) const fn revoked_grants(self) -> &'static [&'static str] {
        match self {
            Self::SecretDirectory | Self::SecretFile => &["*S-1-5-32-545", "*S-1-5-11"],
            Self::PublicDirectory | Self::AgentWritableDirectory | Self::PublicFile => {
                &["*S-1-5-32-545"]
            }
        }
    }

    /// Exactly the trustees the applied DACL must end up carrying.
    pub(crate) fn expected_sids(self) -> std::collections::BTreeSet<String> {
        let mut sids = std::collections::BTreeSet::from([
            SID_SYSTEM.to_string(),
            SID_ADMINISTRATORS.to_string(),
        ]);
        if matches!(
            self,
            Self::PublicDirectory | Self::AgentWritableDirectory | Self::PublicFile
        ) {
            sids.insert(SID_AUTHENTICATED_USERS.to_string());
        }
        sids
    }

    /// Whether the class is for a directory.
    pub(crate) const fn is_directory(self) -> bool {
        matches!(
            self,
            Self::PublicDirectory | Self::SecretDirectory | Self::AgentWritableDirectory
        )
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::PublicDirectory => "public directory",
            Self::SecretDirectory => "secret directory",
            Self::AgentWritableDirectory => "agent-writable directory",
            Self::PublicFile => "public file",
            Self::SecretFile => "secret file",
        }
    }
}

/// The DACL of a security descriptor: its flag letters, and every trustee it
/// names.
#[derive(Debug)]
pub(crate) struct ParsedDacl {
    pub(crate) flags: String,
    pub(crate) sids: std::collections::BTreeSet<String>,
}

/// Normalises an SDDL trustee to a plain SID.
///
/// Well-known trustees are usually rendered as a two-letter alias, but a
/// literal `S-1-...` is equally valid, so both are accepted.
fn canonical_sid(token: &str) -> String {
    match token.trim() {
        "SY" => SID_SYSTEM.to_string(),
        "BA" => SID_ADMINISTRATORS.to_string(),
        "BU" => SID_USERS.to_string(),
        "AU" => SID_AUTHENTICATED_USERS.to_string(),
        other => other.to_ascii_uppercase(),
    }
}

/// Extracts the DACL from an SDDL string.
///
/// Scans at parenthesis depth zero so a `D:` or `S:` inside an access control
/// entry — in a conditional expression, say — cannot be mistaken for the start
/// of a section.
///
/// # Errors
///
/// Returns a message when there is no DACL, when an entry is unterminated or
/// names no trustee, or when the DACL grants nothing at all.
pub(crate) fn parse_dacl(sddl: &str) -> Result<ParsedDacl, String> {
    let chars: Vec<char> = sddl.chars().collect();
    let section_start = |from: usize, marker: char| -> Option<usize> {
        let mut depth = 0_usize;
        let mut index = from;
        while index < chars.len() {
            match chars[index] {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                letter if depth == 0 && letter == marker && chars.get(index + 1) == Some(&':') => {
                    return Some(index);
                }
                _ => {}
            }
            index += 1;
        }
        None
    };

    let start =
        section_start(0, 'D').ok_or_else(|| "security descriptor has no DACL".to_string())? + 2;
    // The DACL runs to the SACL if there is one, otherwise to the end.
    let end = section_start(start, 'S').unwrap_or(chars.len());
    let section: String = chars[start..end].iter().collect();

    let flags: String = section.chars().take_while(|c| *c != '(').collect();
    let mut sids = std::collections::BTreeSet::new();
    let mut rest = &section[flags.len()..];
    while let Some(open) = rest.find('(') {
        let close = rest[open..]
            .find(')')
            .ok_or_else(|| "DACL has an unterminated access control entry".to_string())?;
        let fields: Vec<&str> = rest[open + 1..open + close].split(';').collect();
        // type;flags;rights;object;inherited object;trustee[;condition]
        let trustee = fields
            .get(5)
            .map(|field| field.trim())
            .filter(|field| !field.is_empty())
            .ok_or_else(|| "DACL access control entry names no trustee".to_string())?;
        sids.insert(canonical_sid(trustee));
        rest = &rest[open + close + 1..];
    }
    if sids.is_empty() {
        return Err("DACL grants nothing".to_string());
    }
    Ok(ParsedDacl { flags, sids })
}

/// Checks an applied descriptor against what the class requires.
///
/// # Errors
///
/// Returns a message when the DACL cannot be parsed, still inherits, or names a
/// different set of trustees than the class allows.
pub(crate) fn assert_acl_sddl(path: &str, sddl: &str, acl_class: AclClass) -> Result<(), String> {
    let dacl = parse_dacl(sddl).map_err(|e| format!("{e} for {path} (SDDL {sddl})"))?;
    // `/inheritance:r` protects the DACL. Without the P flag the entries below
    // can be exactly right while inherited access still widens them.
    if !dacl.flags.contains('P') {
        return Err(format!(
            "ACL verification failed for {path}: DACL is not protected, so inherited access may \
             remain (SDDL {sddl})"
        ));
    }
    let expected = acl_class.expected_sids();
    if dacl.sids != expected {
        return Err(format!(
            "ACL verification failed for {path}: expected {} trustees {:?}, got {:?} (SDDL {sddl})",
            acl_class.label(),
            expected,
            dacl.sids
        ));
    }
    // The right trustees are not enough: what each one may do, to the
    // folder itself and to what inherits from it, must be the class's, or a
    // grant widened in place (read and execute made full control) would
    // pass.
    let actual = dacl_meaning(sddl).map_err(|e| format!("{e} for {path} (SDDL {sddl})"))?;
    let intended = dacl_meaning(acl_class.sddl())?;
    if actual == intended {
        Ok(())
    } else {
        Err(format!(
            "ACL verification failed for {path}: it does not grant exactly what a {} does \
             (expected {intended:?}, got {actual:?}; SDDL {sddl})",
            acl_class.label()
        ))
    }
}

/// Where a DACL grants or denies rights: the object itself, or what its
/// child folders (`CI`) and files (`OI`) inherit, either all the way down
/// or one level only (`NP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reach {
    Itself,
    Folders { one_level: bool },
    Files { one_level: bool },
}

/// What a DACL means, for comparison: for each entry type, trustee, reach
/// and whether it was inherited, the rights given, generic rights mapped as
/// Windows maps them for files and folders. Windows stores an inheritable
/// grant made with generic rights as an inherit-only generic entry plus an
/// entry for the object with the mapped rights; both mean the same.
pub(crate) type DaclMeaning = std::collections::BTreeMap<(String, String, Reach, bool), u32>;

/// `mask` with its generic rights replaced by the file rights they map to.
const fn file_rights(mask: u32) -> u32 {
    let mut rights = mask & 0x0FFF_FFFF;
    if mask & 0x1000_0000 != 0 {
        rights |= 0x001F_01FF;
    }
    if mask & 0x8000_0000 != 0 {
        rights |= 0x0012_0089;
    }
    if mask & 0x4000_0000 != 0 {
        rights |= 0x0012_0116;
    }
    if mask & 0x2000_0000 != 0 {
        rights |= 0x0012_00A9;
    }
    rights
}

/// The [`DaclMeaning`] of the DACL of `sddl`. An entry that cannot be read
/// exactly (unknown rights, a condition, an object type) is an error.
pub(crate) fn dacl_meaning(sddl: &str) -> Result<DaclMeaning, String> {
    if sddl.contains('"') || sddl.contains('@') {
        return Err("DACL has a conditional entry".to_string());
    }
    let dacl =
        sddl_section(sddl, 'D').ok_or_else(|| "security descriptor has no DACL".to_string())?;
    let flags: String = dacl.chars().take_while(|c| *c != '(').collect();
    let mut rest = &dacl[flags.len()..];
    let mut meaning = DaclMeaning::new();
    while let Some(open) = rest.find('(') {
        let close = rest[open..]
            .find(')')
            .map(|close| open + close)
            .ok_or_else(|| "DACL has an unterminated access control entry".to_string())?;
        let fields: Vec<&str> = rest[open + 1..close].split(';').collect();
        rest = &rest[close + 1..];
        let [kind, ace_flags, rights, object, inherited_object, trustee] = fields.as_slice() else {
            return Err("DACL entry is not a plain entry".to_string());
        };
        if !object.is_empty() || !inherited_object.is_empty() || !matches!(*kind, "A" | "D") {
            return Err(format!("DACL entry of type {kind} with an object type"));
        }
        let rights = file_rights(
            sddl_rights(rights).ok_or_else(|| format!("DACL entry with rights {rights}"))?,
        );
        let mut flag = std::collections::BTreeSet::new();
        for code in ace_flags.as_bytes().chunks(2) {
            let code = std::str::from_utf8(code).map_err(|_| "DACL entry flags".to_string())?;
            if !matches!(code, "OI" | "CI" | "NP" | "IO" | "ID") {
                return Err(format!("DACL entry with flag {code}"));
            }
            flag.insert(code);
        }
        let one_level = flag.contains("NP");
        let mut reaches = Vec::new();
        if !flag.contains("IO") {
            reaches.push(Reach::Itself);
        }
        if flag.contains("CI") {
            reaches.push(Reach::Folders { one_level });
        }
        if flag.contains("OI") {
            reaches.push(Reach::Files { one_level });
        }
        for reach in reaches {
            *meaning
                .entry((
                    (*kind).to_owned(),
                    canonical_sid(trustee),
                    reach,
                    flag.contains("ID"),
                ))
                .or_insert(0) |= rights;
        }
    }
    meaning.retain(|_, rights| *rights != 0);
    Ok(meaning)
}

/// `TrustedInstaller`, which owns `C:\` and `C:\Program Files`.
pub(crate) const SID_TRUSTED_INSTALLER: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Who may own, and hold any right on, a folder above an install directory.
const CHAIN_TRUSTEES: [&str; 3] = [SID_SYSTEM, SID_ADMINISTRATORS, SID_TRUSTED_INSTALLER];

/// `DELETE`: renames or deletes the object itself.
const DELETE: u32 = 0x0001_0000;

/// Rights on a folder above an install directory that let their holder
/// replace it, or a folder in it, with one of their own: what each lets
/// them do, for the message.
const REPLACING_RIGHTS: [(u32, &str); 5] = [
    (0x1000_0000, "take full control of"),
    (DELETE, "rename or delete"),
    (0x0004_0000, "change the permissions of"),
    (0x0008_0000, "take ownership of"),
    (0x0000_0040, "rename or delete the folders in"),
];

/// The access mask of an SDDL rights field: a hexadecimal number or
/// two-letter codes. `None` for a code this does not know, which the
/// caller treats as every right.
fn sddl_rights(field: &str) -> Option<u32> {
    let field = field.trim();
    if let Some(hex) = field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).ok();
    }
    if field.len() % 2 != 0 || !field.is_ascii() {
        return None;
    }
    let mut mask = 0_u32;
    for index in (0..field.len()).step_by(2) {
        mask |= match &field[index..index + 2] {
            "GA" => 0x1000_0000,
            "GR" => 0x8000_0000,
            "GW" => 0x4000_0000,
            "GX" => 0x2000_0000,
            "RC" => 0x0002_0000,
            "SD" => 0x0001_0000,
            "WD" => 0x0004_0000,
            "WO" => 0x0008_0000,
            "CC" => 0x0000_0001,
            "DC" => 0x0000_0002,
            "LC" => 0x0000_0004,
            "SW" => 0x0000_0008,
            "RP" => 0x0000_0010,
            "WP" => 0x0000_0020,
            "DT" => 0x0000_0040,
            "LO" => 0x0000_0080,
            "CR" => 0x0000_0100,
            "FA" => 0x001F_01FF,
            "FR" => 0x0012_0089,
            "FW" => 0x0012_0116,
            "FX" => 0x0012_00A9,
            _ => return None,
        };
    }
    Some(mask)
}

/// The text of the SDDL section `marker` (`O`, `G`, `D` or `S`), found at
/// parenthesis depth zero, up to the next section.
fn sddl_section(sddl: &str, marker: char) -> Option<&str> {
    let bytes = sddl.as_bytes();
    let mut depth = 0_usize;
    let mut start = None;
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'O' | b'G' | b'D' | b'S' if depth == 0 && bytes.get(index + 1) == Some(&b':') => {
                if let Some(start) = start {
                    return Some(&sddl[start..index]);
                }
                if byte == marker as u8 {
                    start = Some(index + 2);
                }
            }
            _ => {}
        }
    }
    start.map(|start| &sddl[start..])
}

/// Why a folder above an install directory is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hazard {
    /// `account` could have replaced the folder: it `how`. Access is
    /// checked when a handle is opened, so a handle opened while this was
    /// so keeps the right after the ACL is fixed in place: the folder is
    /// deleted and made again instead.
    Replaceable {
        path: String,
        account: String,
        how: String,
    },
    /// Who may replace the folder cannot be judged.
    Unreadable { path: String, what: String },
}

impl std::fmt::Display for Hazard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replaceable { path, account, how } => {
                write!(f, "{path} has been replaceable by {account}, who {how}")
            }
            Self::Unreadable { path, what } => {
                write!(f, "cannot judge who may replace {path}: {what}")
            }
        }
    }
}

/// Whether `path` is a drive's root (`X:\\`, `\\\\?\\X:\\`), which can never
/// be renamed or deleted. A share's root (`\\\\server\\share`) is not one:
/// it is an ordinary folder on its server. Read from the text, the same
/// on every host.
pub(crate) fn is_volume_root(path: &str) -> bool {
    let path = path.trim_end_matches('\\');
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Whether `path` names a network location (`\\\\server\\share\\...`,
/// `\\\\?\\UNC\\...`, `//server/share`): anything that does not begin with a
/// drive letter, as is or verbatim. A drive letter mapped to a share is
/// found by the caller from the drive itself.
pub(crate) fn is_network_path(path: &str) -> bool {
    let verbatim_disk = |rest: &str| {
        let bytes = rest.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    // A device path (`\\.\Z:\`) reaches whatever the device name is
    // mapped to, a share included: refused outright.
    if path.starts_with(r"\\.\") {
        return true;
    }
    // An NT path (`\??\UNC\server\share`, `\??\Z:\`) is no Win32 path at
    // all: made absolute it would read as a folder on the current drive.
    if path.starts_with(r"\??\") || path.starts_with("/??/") {
        return true;
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        return !verbatim_disk(rest);
    }
    path.starts_with(r"\\") || path.starts_with("//")
}

/// Judges an existing folder above an install directory (`sddl`, its
/// owner and DACL). It is replaceable when its owner is not SYSTEM,
/// Administrators or `TrustedInstaller`, or when its DACL lets anyone else
/// rename or delete it, change its permissions or owner, or rename or
/// delete the folders in it: any of them could put a folder of their own
/// where the installer's is, and the service would run what they placed
/// there. A volume root ([`is_volume_root`]) cannot be renamed or deleted,
/// so `DELETE` on it is harmless. Creating new folders in it is harmless
/// too: the installer's own already exist and are checked themselves.
/// Denying entries are ignored, so a denial never passes a folder. An
/// entry it cannot read refuses the folder.
///
/// # Errors
///
/// The [`Hazard`].
pub(crate) fn ancestor_hazard(path: &str, sddl: &str) -> Result<(), Hazard> {
    let unreadable = |what: &str| Hazard::Unreadable {
        path: path.to_owned(),
        what: format!("{what} (SDDL {sddl})"),
    };
    let replaceable = |account: &str, how: String| Hazard::Replaceable {
        path: path.to_owned(),
        account: account.to_owned(),
        how,
    };
    // A string literal or an attribute name (only in a conditional entry)
    // could hold a lone parenthesis, which would hide the entries after it
    // from the depth counting below; without them, SDDL's parentheses are
    // balanced. Refused before anything is parsed, whatever the encoding.
    if sddl.contains('"') {
        return Err(unreadable("a conditional entry with a string literal"));
    }
    if sddl.contains('@') {
        return Err(unreadable("a conditional entry with an attribute"));
    }
    let owner = sddl_section(sddl, 'O')
        .map(canonical_sid)
        .ok_or_else(|| unreadable("no owner"))?;
    if !CHAIN_TRUSTEES.contains(&owner.as_str()) {
        return Err(replaceable(
            &owner,
            "owns it (an owner can always change its permissions)".to_owned(),
        ));
    }
    let dacl = sddl_section(sddl, 'D').ok_or_else(|| unreadable("no DACL"))?;
    let flags: String = dacl.chars().take_while(|c| *c != '(').collect();
    if flags.contains("NO_ACCESS_CONTROL") {
        return Err(replaceable(
            "everyone",
            "may do anything to it (it has no access control)".to_owned(),
        ));
    }
    let root = is_volume_root(path);
    let mut rest = &dacl[flags.len()..];
    while let Some(open) = rest.find('(') {
        let mut depth = 0_usize;
        let mut close = None;
        for (index, character) in rest[open..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.ok_or_else(|| unreadable("an unterminated entry"))?;
        let fields: Vec<&str> = rest[open + 1..close].splitn(7, ';').collect();
        rest = &rest[close + 1..];
        let [kind, ace_flags, rights, _, _, trustee, ..] = fields.as_slice() else {
            return Err(unreadable("an entry without a trustee"));
        };
        let conditional = match kind.trim() {
            "A" | "OA" => false,
            "XA" | "ZA" => true,
            "D" | "OD" | "XD" => continue,
            other => return Err(unreadable(&format!("an entry of type {other}"))),
        };
        let inherit_only = ace_flags.as_bytes().chunks(2).any(|flag| flag == b"IO");
        let trustee = canonical_sid(trustee);
        if inherit_only || CHAIN_TRUSTEES.contains(&trustee.as_str()) {
            continue;
        }
        if conditional {
            return Err(unreadable(&format!("a conditional entry for {trustee}")));
        }
        let mask = sddl_rights(rights).unwrap_or(u32::MAX);
        let what: Vec<&str> = REPLACING_RIGHTS
            .iter()
            .filter(|(right, _)| mask & right != 0)
            .filter(|(right, _)| !(root && *right == DELETE))
            .map(|(_, what)| *what)
            .collect();
        if !what.is_empty() {
            return Err(replaceable(&trustee, format!("may {} it", what.join(", "))));
        }
    }
    Ok(())
}

/// Trustees present in `sddl` that `acl_class` does not allow.
///
/// # Errors
///
/// Returns a message when the DACL cannot be parsed.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn unexpected_trustees(sddl: &str, acl_class: AclClass) -> Result<Vec<String>, String> {
    let dacl = parse_dacl(sddl)?;
    let expected = acl_class.expected_sids();
    Ok(dacl
        .sids
        .into_iter()
        .filter(|sid| !expected.contains(sid))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_explorer_added_is_found_and_nothing_else() {
        // Exactly what an operator's machine reported after they opened the
        // folder in Explorer and accepted its permission prompt.
        let sddl = "O:BAG:S-1-5-21-2635842226-2484704124-390085508-513D:PAI(A;OICI;FA;;;SY)\
                    (A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-21-2635842226-2484704124-390085508-1001)";
        assert_eq!(
            unexpected_trustees(sddl, AclClass::SecretDirectory).unwrap(),
            vec!["S-1-5-21-2635842226-2484704124-390085508-1001".to_string()]
        );
        assert!(
            unexpected_trustees(&secret_ok(), AclClass::SecretDirectory)
                .unwrap()
                .is_empty()
        );
    }

    /// Read from a Windows 11 lab host (`Get-Acl`, 2026-11-20).
    const SYSTEM_DRIVE: &str = "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464\
        G:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464D:PAI(A;;LC;;;AU)\
        (A;OICIIO;SDGXGWGR;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)\
        (A;;0x1000a1;;;S-1-15-3-65536-1888954469-739942743-1668119174-2468466756-4239452838-1296943325-355587736-700089176)";
    const PROGRAM_FILES: &str = "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464\
        G:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464D:PAI(A;OICIIO;GA;;;CO)\
        (A;OICIIO;GA;;;SY)(A;;0x1301bf;;;SY)(A;OICIIO;GA;;;BA)(A;;0x1301bf;;;BA)(A;OICIIO;GXGR;;;BU)\
        (A;;0x1200a9;;;BU)(A;CIIO;GA;;;S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)\
        (A;;FA;;;S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)(A;;0x1200a9;;;AC)\
        (A;OICIIO;GXGR;;;AC)(A;;0x1200a9;;;S-1-15-2-2)(A;OICIIO;GXGR;;;S-1-15-2-2)";
    const PROGRAM_DATA: &str = "O:SYG:SYD:PAI(A;OICIIO;GA;;;CO)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)\
        (A;OICI;0x1200a9;;;BU)(A;CI;DCLCRPCR;;;BU)";

    #[test]
    fn a_data_volume_root_is_never_refused_for_what_cannot_happen_to_a_root() {
        // A secondary NTFS volume's root: users may Modify it, delete
        // included, but a root can never be renamed or deleted.
        let root = "O:BAG:SYD:AI(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1301bf;;;AU)(A;OICI;0x1200a9;;;BU)";
        ancestor_hazard(r"D:\", root).expect("a root cannot be renamed");
        // What can still be done to a root is still refused.
        for rights in ["WD", "WO", "DT", "GA"] {
            let sddl = format!("O:BAG:SYD:AI(A;OICI;FA;;;BA)(A;;{rights};;;AU)");
            assert!(ancestor_hazard(r"D:\", &sddl).is_err(), "{rights}");
        }
        // Below the root, the same Modify lets them rename the folder.
        assert!(ancestor_hazard(r"D:\certs", root).is_err());
        for path in [r"D:\", "D:", r"\\?\D:\"] {
            assert!(is_volume_root(path), "{path}");
        }
        // A share's root is an ordinary folder on its server (`C:\Tools`
        // shared as `\\localhost\tools`): DELETE there renames it.
        for path in [
            r"D:\certs",
            r"\\server\share\",
            r"\\?\UNC\server\share",
            r"\\server\share\certs",
            r"\\?\D:\certs",
            r"\\server",
            "",
        ] {
            assert!(!is_volume_root(path), "{path}");
        }
        assert!(ancestor_hazard(r"\\localhost\tools", root).is_err());
        assert!(ancestor_hazard(r"\\?\UNC\localhost\tools\", root).is_err());
        for path in [
            r"\\server\share\Arcen",
            r"\\?\UNC\server\share\Arcen",
            "//server/share/Arcen",
            r"\\localhost\C$\Arcen",
            r"\\?\Volume{0}\Arcen",
            r"\\?\GLOBALROOT\Device\Mup\server\share",
            // A device path: `Z:` may be a mapped drive.
            r"\\.\Z:\Arcen",
            r"\\.\C:\Arcen",
            // NT paths, which `GetFullPathNameW` would put on the current drive.
            r"\??\UNC\server\share\Arcen",
            r"\??\C:\Arcen",
        ] {
            assert!(is_network_path(path), "{path}");
        }
        for path in [r"C:\Arcen", r"\\?\C:\Arcen", r"D:\"] {
            assert!(!is_network_path(path), "{path}");
        }
    }

    #[test]
    fn the_default_install_folders_are_ones_no_user_can_replace() {
        for (path, sddl) in [
            (r"C:\", SYSTEM_DRIVE),
            (r"C:\Program Files", PROGRAM_FILES),
            (r"C:\ProgramData", PROGRAM_DATA),
        ] {
            ancestor_hazard(path, sddl).unwrap_or_else(|error| panic!("{path}: {error}"));
        }
        // What the installer makes for a missing folder above a prefix.
        ancestor_hazard(r"C:\Tools", AclClass::PublicDirectory.sddl()).expect("created folder");
    }

    #[test]
    fn a_folder_a_user_can_replace_is_named_with_who_and_how() {
        // `C:\Tools`, made under the drive root: it inherits Modify.
        let tools = "O:BAG:S-1-5-21-1-2-3-513D:AI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)\
                     (A;OICIID;0x1301bf;;;AU)";
        let error = ancestor_hazard(r"C:\Tools", tools)
            .expect_err("Modify includes delete")
            .to_string();
        assert!(
            error.contains(
                r"C:\Tools has been replaceable by S-1-5-11, who may rename or delete it"
            ),
            "{error}"
        );
        for (sddl, what) in [
            (
                "O:BAD:PAI(A;;FA;;;BA)(A;;WD;;;BU)",
                "change the permissions of",
            ),
            ("O:BAD:PAI(A;;FA;;;BA)(A;;WO;;;WD)", "take ownership of"),
            (
                "O:BAD:PAI(A;;FA;;;BA)(A;;DT;;;AU)",
                "rename or delete the folders in",
            ),
            (
                "O:BAD:PAI(A;;FA;;;BA)(A;;GA;;;S-1-5-21-1-2-3-1001)",
                "take full control of",
            ),
            ("O:BAD:PAI(A;;FA;;;BA)(A;;0x10000;;;AU)", "rename or delete"),
        ] {
            let error = ancestor_hazard("X", sddl).expect_err(sddl).to_string();
            assert!(error.contains(what), "{sddl}: {error}");
        }
        let owned = ancestor_hazard("X", "O:BUD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
            .expect_err("a user owns it")
            .to_string();
        assert!(
            owned.contains("replaceable by S-1-5-32-545, who owns it"),
            "{owned}"
        );
        assert!(ancestor_hazard("X", "O:BAD:NO_ACCESS_CONTROL").is_err());
        // What cannot be read is refused, never passed.
        assert!(ancestor_hazard("X", "O:BAD:PAI(A;;FA;;;BA)(A;;QQ;;;AU)").is_err());
        assert!(ancestor_hazard("X", "O:BAD:PAI(ML;;NW;;;LW)").is_err());
        assert!(ancestor_hazard("X", "D:PAI(A;;FA;;;BA)").is_err());
        // A conditional entry for anyone else is never judged passable,
        // and a string literal cannot hide an entry behind a parenthesis.
        assert!(
            ancestor_hazard(
                "X",
                "O:BAD:PAI(A;;FA;;;BA)(XA;;FR;;;AU;(Member_of {SID(BA)}))"
            )
            .is_err()
        );
        assert!(ancestor_hazard("X", "O:BAD:PAI(A;;FA;;;BA)(XD;;FR;;;AU;(@User.x == 1))").is_err());
        let hidden = "O:BAD:PAI(A;;FA;;;SY)(A;;FA;;;BA)(XA;;FR;;;AU;(@User.x == \"(\"))\
                      (A;;SD;;;AU)(XA;;FR;;;AU;(@User.y == \")\"))";
        let error = ancestor_hazard("X", hidden)
            .expect_err("a hidden entry is never passed")
            .to_string();
        assert!(error.contains("string literal"), "{error}");
    }

    #[test]
    fn creating_folders_inheriting_and_denials_never_make_a_folder_replaceable() {
        for sddl in [
            // Users may add folders to it: theirs, never the installer's.
            "O:BAD:PAI(A;;FA;;;BA)(A;;LCDC;;;AU)(A;;0x116;;;BU)",
            // What only children inherit does not apply to the folder.
            "O:BAD:PAI(A;;FA;;;BA)(A;OICIIO;GA;;;AU)(A;OICIIO;GA;;;CO)",
            // A denial is ignored either way.
            "O:BAD:PAI(A;;FA;;;BA)(D;;SD;;;AU)",
            // Read and execute, generic write without delete.
            "O:BAD:PAI(A;;FA;;;BA)(A;;FRFX;;;BU)(A;;GW;;;BU)",
        ] {
            ancestor_hazard("X", sddl).unwrap_or_else(|error| panic!("{sddl}: {error}"));
        }
    }

    const PATH: &str = r"C:\ProgramData\Arcen\tls\host.key";
    const RUNTIME_PATH: &str = r"C:\ProgramData\Arcen\runtime";

    const ALL_ACL_CLASSES: [AclClass; 5] = [
        AclClass::PublicDirectory,
        AclClass::SecretDirectory,
        AclClass::AgentWritableDirectory,
        AclClass::PublicFile,
        AclClass::SecretFile,
    ];

    /// A protected DACL carrying exactly `aces`.
    fn sddl(aces: &str) -> String {
        format!("O:BAG:SYD:PAI{aces}")
    }

    fn secret_ok() -> String {
        sddl("(A;;FA;;;SY)(A;;FA;;;BA)")
    }

    /// The bug this module was extracted for.
    ///
    /// Reported from a Norwegian Windows 2026-08-21: `icacls` refused
    /// `Administrators` with "Ingen tilordninger ble gjort mellom kontonavn og
    /// sikkerhets-IDer" and the install aborted at the first ACL, before any
    /// file was copied. The Deck then reported only `IO error: timed out`,
    /// because nothing was ever listening.
    #[test]
    fn every_grant_addresses_a_sid_not_a_display_name() {
        for class in ALL_ACL_CLASSES {
            for grant in class.grants() {
                assert!(
                    grant.starts_with("*S-1-"),
                    "{class:?} grants {grant:?} by name; a localized Windows cannot resolve it"
                );
            }
        }
    }

    /// Verification must not reintroduce the same assumption. SDDL names
    /// trustees by SID, so nothing here depends on the display language.
    #[test]
    fn verification_accepts_the_alias_and_raw_sid_spellings_alike() {
        let aliases = parse_dacl(&sddl("(A;;FA;;;SY)(A;;FA;;;BA)")).expect("aliases parse");
        let raw =
            parse_dacl(&sddl("(A;;FA;;;S-1-5-18)(A;;FA;;;S-1-5-32-544)")).expect("raw SIDs parse");
        assert_eq!(aliases.sids, raw.sids);
        assert!(
            assert_acl_sddl(
                PATH,
                &sddl("(A;;FA;;;S-1-5-18)(A;;FA;;;S-1-5-32-544)"),
                AclClass::SecretFile
            )
            .is_ok()
        );
    }

    /// Every class must agree with the SDDL it advertises, or the printed
    /// intent and the enforced result drift apart.
    #[test]
    fn each_class_expects_what_its_own_sddl_declares() {
        for class in ALL_ACL_CLASSES {
            let declared = parse_dacl(class.sddl())
                .unwrap_or_else(|error| panic!("{class:?} has unparseable SDDL: {error}"));
            assert_eq!(
                declared.sids,
                class.expected_sids(),
                "{class:?} verifies a different trustee set than its SDDL declares"
            );
            assert!(
                declared.flags.contains('P'),
                "{class:?} declares an inheriting DACL"
            );
        }
    }

    /// The grant list and the expected trustees must name the same principals,
    /// so a grant cannot be added without also being verified.
    #[test]
    fn each_class_grants_exactly_the_trustees_it_verifies() {
        for class in ALL_ACL_CLASSES {
            let granted: std::collections::BTreeSet<String> = class
                .grants()
                .iter()
                .map(|grant| {
                    let sid = grant
                        .trim_start_matches('*')
                        .split(':')
                        .next()
                        .expect("grant names a trustee");
                    sid.to_string()
                })
                .collect();
            assert_eq!(
                granted,
                class.expected_sids(),
                "{class:?} grants a different trustee set than it verifies"
            );
        }
    }

    #[test]
    fn revocations_remove_only_trustees_the_class_forbids() {
        for class in ALL_ACL_CLASSES {
            let expected = class.expected_sids();
            for revoked in class.revoked_grants() {
                let sid = revoked.trim_start_matches('*');
                assert!(
                    !expected.contains(sid),
                    "{class:?} revokes required trustee {sid}"
                );
            }
        }
    }

    /// The manual install predates the binary and grants Authenticated Users
    /// read/execute on the program tree. Keep the two paths on the same SID so
    /// an operator can move between them without accumulating a second ACE.
    #[test]
    fn public_acl_matches_the_documented_manual_install() {
        const MANUAL_INSTALL_DOC: &str = include_str!("../../../../hosts/windows/INSTALL.md");
        let authenticated_users = format!("'*{SID_AUTHENTICATED_USERS}:(OI)(CI)RX'");
        let builtin_users = format!("'*{SID_USERS}:(OI)(CI)RX'");

        assert!(
            MANUAL_INSTALL_DOC.contains(&authenticated_users),
            "hosts/windows/INSTALL.md and the binary installer must grant the same public trustee"
        );
        assert!(
            !MANUAL_INSTALL_DOC.contains(&builtin_users),
            "the manual install still grants the legacy public trustee"
        );
        assert!(
            AclClass::PublicDirectory
                .expected_sids()
                .contains(SID_AUTHENTICATED_USERS),
            "the binary install must match the documented public trustee"
        );
        assert!(
            AclClass::PublicDirectory
                .revoked_grants()
                .contains(&"*S-1-5-32-545"),
            "upgrades from the shipped BUILTIN\\Users ACL must remove its stale ACE"
        );
    }

    /// Captured from the failed 0.9.8 reinstall: the old manual AU entry
    /// survived while `/grant:r` added BU. The fixed installer converges on AU
    /// and removes BU instead of accepting the widened descriptor.
    #[test]
    fn mixed_public_acl_from_failed_reinstall_is_rejected() {
        let captured = "O:BAG:S-1-5-21-1000000001-1000000002-1000000003-513D:PAI\
                        (A;OICI;0x1200a9;;;AU)(A;OICI;FA;;;SY)\
                        (A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
        assert!(
            assert_acl_sddl(
                r"C:\Program Files\Arcen\Pier",
                captured,
                AclClass::PublicDirectory
            )
            .is_err(),
            "a public directory carrying both old and new trustees must not pass"
        );
        assert!(
            assert_acl_sddl(
                r"C:\Program Files\Arcen\Pier",
                &sddl("(A;OICI;0x1200a9;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                AclClass::PublicDirectory
            )
            .is_ok(),
            "removing the legacy BUILTIN\\Users ACE must produce the canonical ACL"
        );
    }

    #[test]
    fn the_agent_writable_class_requires_authenticated_users() {
        // The per-session agent runs under the signed-in user's unelevated
        // token. A secret ACL on its working directory does not harden
        // anything; it stops every session with "Access is denied".
        assert!(
            assert_acl_sddl(
                RUNTIME_PATH,
                &sddl("(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                AclClass::AgentWritableDirectory
            )
            .is_err(),
            "an administrators-only runtime directory must be rejected"
        );
        assert!(
            assert_acl_sddl(
                RUNTIME_PATH,
                &sddl("(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;AU)"),
                AclClass::AgentWritableDirectory
            )
            .is_ok(),
            "the known-working live ACL must be accepted"
        );
    }

    #[test]
    fn agent_writable_is_not_reachable_by_ordinary_users_beyond_that_directory() {
        // Authenticated Users is granted only for the agent class. A secret
        // file that acquired it must still fail, so widening one directory
        // cannot silently widen the others.
        assert!(
            assert_acl_sddl(
                PATH,
                &sddl("(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1301bf;;;AU)"),
                AclClass::SecretFile
            )
            .is_err(),
            "host.key must never grant Authenticated Users"
        );
    }

    #[test]
    fn a_widened_grant_to_an_allowed_trustee_fails() {
        // Same trustees as the class, but Authenticated Users' read and
        // execute widened to full control, or made to reach only children.
        for dacl in [
            "(A;OICI;FA;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            "(A;OICI;0x1301bf;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            "(A;OICIIO;FA;;;AU)(A;OICI;0x1200a9;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            "(A;CI;0x1200a9;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        ] {
            assert!(
                assert_acl_sddl(
                    r"C:\Program Files\Arcen\Pier",
                    &sddl(dacl),
                    AclClass::PublicDirectory
                )
                .is_err(),
                "{dacl}"
            );
        }
        // The installed forms, generic rights as Windows renders them: as
        // given, mapped, or split into an inherit-only generic entry and a
        // mapped entry for the folder itself.
        assert!(
            assert_acl_sddl(
                r"C:\Program Files\Arcen\Pier",
                &sddl("(A;OICIIO;GXGR;;;AU)(A;;0x1200a9;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                AclClass::PublicDirectory
            )
            .is_ok()
        );
        assert!(
            assert_acl_sddl(
                r"C:\Program Files\Arcen\Pier",
                &sddl("(A;OICI;0x1200a9;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                AclClass::PublicDirectory
            )
            .is_ok()
        );
        assert!(
            assert_acl_sddl(
                r"C:\Program Files\Arcen\Pier",
                &sddl("(A;OICI;GRGX;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                AclClass::PublicDirectory
            )
            .is_ok()
        );
    }

    #[test]
    fn administrators_and_system_only_passes_for_secret_acl() {
        assert!(assert_acl_sddl(PATH, &secret_ok(), AclClass::SecretFile).is_ok());
    }

    #[test]
    fn users_read_access_fails_for_secret_acl() {
        assert!(
            assert_acl_sddl(
                PATH,
                &sddl("(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)"),
                AclClass::SecretFile
            )
            .is_err()
        );
    }

    #[test]
    fn users_full_access_fails_for_secret_acl() {
        assert!(
            assert_acl_sddl(
                PATH,
                &sddl("(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;BU)"),
                AclClass::SecretFile
            )
            .is_err()
        );
    }

    /// An unprotected DACL still inherits, so the entries can be exactly right
    /// while access is still wider than intended.
    #[test]
    fn an_inheriting_dacl_fails_even_with_the_right_trustees() {
        assert!(
            assert_acl_sddl(
                PATH,
                "O:BAG:SYD:AI(A;;FA;;;SY)(A;;FA;;;BA)",
                AclClass::SecretFile
            )
            .is_err(),
            "a DACL that still inherits must be rejected"
        );
    }

    /// A SACL may follow the DACL, and must not be read as part of it.
    #[test]
    fn a_trailing_sacl_is_not_mistaken_for_dacl_entries() {
        let parsed =
            parse_dacl("O:BAG:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)S:AI(AU;SA;FA;;;WD)").expect("parses");
        assert_eq!(
            parsed.sids,
            AclClass::SecretFile.expected_sids(),
            "the audit entry's trustee must not appear as a granted trustee"
        );
    }

    /// Conditional entries carry a seventh field after the trustee.
    #[test]
    fn a_conditional_ace_still_yields_its_trustee() {
        let parsed = parse_dacl(&sddl("(A;;FA;;;SY)(XA;;FA;;;BA;(@USER.Title==\"x\"))"))
            .expect("conditional entry parses");
        assert_eq!(parsed.sids, AclClass::SecretFile.expected_sids());
    }

    /// Real output captured from `(Get-Acl).Sddl` on Windows after the
    /// installer's own secret-directory grants, so the parser is pinned to what
    /// the OS actually emits rather than to what this file assumes.
    ///
    /// Note the group is a raw domain SID ending `-513`, sitting immediately
    /// before `D:`. The scan must not be confused by that, and must not trip
    /// over any letter in the owner or group fields.
    #[test]
    fn the_real_windows_descriptor_parses() {
        let captured = "O:BAG:S-1-5-21-1219702343-3571824762-2993852837-513D:PAI\
                        (A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
        let parsed = parse_dacl(captured).expect("real descriptor parses");
        assert_eq!(parsed.flags, "PAI");
        assert_eq!(parsed.sids, AclClass::SecretDirectory.expected_sids());
        assert!(
            assert_acl_sddl(r"C:\ProgramData\Arcen", captured, AclClass::SecretDirectory).is_ok()
        );
    }

    /// The documented manual install and this binary must not drift apart.
    ///
    /// `hosts/windows/INSTALL.md` hardens the same directories by hand and has
    /// always used SIDs, so it worked on a localized Windows while this
    /// installer — written later, granting by name — never did. Nothing
    /// mechanically compared them, so the divergence survived four weeks.
    ///
    /// Both halves are checked here: every grant this binary issues, and every
    /// principal the document names. `hosts/windows/INSTALL.md` contains no
    /// `Name:` token at all today, so any match is a real regression rather
    /// than prose.
    #[test]
    fn neither_install_path_names_a_principal_instead_of_a_sid() {
        const MANUAL_INSTALL_DOC: &str = include_str!("../../../../hosts/windows/INSTALL.md");
        for principal in [
            "Administrators:",
            "Users:",
            "SYSTEM:",
            "Everyone:",
            "Authenticated Users:",
        ] {
            assert!(
                !MANUAL_INSTALL_DOC.contains(principal),
                "hosts/windows/INSTALL.md grants to {principal:?} by name; a localized Windows \
                 cannot resolve it, and this is the drift that shipped a broken installer"
            );
            for class in ALL_ACL_CLASSES {
                for grant in class.grants() {
                    assert!(
                        !grant.starts_with(principal),
                        "{class:?} grants {grant:?} by name"
                    );
                }
            }
        }
    }

    /// The installed paths accept only SYSTEM or Administrators as owner, and a
    /// directory this installer creates is otherwise owned by whoever ran it.
    /// That is why the owner is set explicitly, by SID, exactly as
    /// `hosts/windows/INSTALL.md` does.
    #[test]
    fn the_owner_is_a_sid_the_pier_will_accept() {
        assert!(
            OWNER_SID.starts_with("*S-1-"),
            "owner must be a SID; a localized Windows cannot resolve a name"
        );
        let sid = OWNER_SID.trim_start_matches('*');
        assert!(
            sid == SID_ADMINISTRATORS || sid == SID_SYSTEM,
            "arcen-pier's restricted_acl accepts only SYSTEM or Administrators as \
             owner, so {OWNER_SID} would make the service fail to start"
        );
    }

    #[test]
    fn empty_descriptor_fails_closed() {
        assert!(parse_dacl("").is_err());
    }

    #[test]
    fn a_descriptor_without_a_dacl_fails_closed() {
        assert!(parse_dacl("O:BAG:SY").is_err());
    }

    #[test]
    fn an_empty_dacl_fails_closed() {
        // "Grants nothing" is not the same as "grants only SYSTEM and
        // Administrators", and must never be read as success.
        assert!(parse_dacl("O:BAG:SYD:PAI").is_err());
    }

    #[test]
    fn an_unterminated_ace_fails_closed() {
        assert!(parse_dacl("O:BAG:SYD:PAI(A;;FA;;;SY").is_err());
    }

    #[test]
    fn an_ace_without_a_trustee_fails_closed() {
        assert!(parse_dacl("O:BAG:SYD:PAI(A;;FA;;)").is_err());
        assert!(parse_dacl("O:BAG:SYD:PAI(A;;FA;;;)").is_err());
    }
}
