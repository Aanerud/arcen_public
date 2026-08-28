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
    if dacl.sids == expected {
        Ok(())
    } else {
        Err(format!(
            "ACL verification failed for {path}: expected {} trustees {:?}, got {:?} (SDDL {sddl})",
            acl_class.label(),
            expected,
            dacl.sids
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
