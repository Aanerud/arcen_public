//! When `/etc/arcen/xorg.conf` may be trusted as a root-run X server's
//! configuration.
//!
//! The template is handed to a root Xorg, so anyone who can write it can make
//! that server load code (`ModulePath`, `Load`). The launcher refuses one that
//! is not a regular, root-owned file closed to group and other writes, and the
//! installer must refuse to repair such a file: rewriting it root-owned `0644`
//! would launder an attacker's edit past the launcher's check. Both read the
//! rule here. The installer compiles this file too, so it uses only `std`.

/// Why a template is not trusted, or that it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateTrust {
    Trusted,
    /// A directory, device, socket — anything but a regular file.
    NotRegularFile,
    /// Owned by this user instead of the expected owner.
    WrongOwner {
        uid: u32,
    },
    /// Group- or other-writable; carries the permission bits.
    WritableByOthers {
        mode: u32,
    },
}

impl TemplateTrust {
    /// One clause explaining a refusal, for a log line or a terminal.
    pub fn describe(self) -> String {
        match self {
            Self::Trusted => "trusted".to_owned(),
            Self::NotRegularFile => "it is not a regular file".to_owned(),
            Self::WrongOwner { uid } => format!("it is owned by uid {uid}, not root"),
            Self::WritableByOthers { mode } => format!(
                "its mode {:04o} lets group or other users write it",
                mode & 0o7777
            ),
        }
    }
}

/// The rule, over facts from an `fstat` of the already-opened file.
pub fn template_trust(is_regular_file: bool, uid: u32, mode: u32, owner: u32) -> TemplateTrust {
    if !is_regular_file {
        TemplateTrust::NotRegularFile
    } else if uid != owner {
        TemplateTrust::WrongOwner { uid }
    } else if mode & 0o022 != 0 {
        TemplateTrust::WritableByOthers { mode }
    } else {
        TemplateTrust::Trusted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_root_owned_regular_file_closed_to_others_is_trusted() {
        assert_eq!(template_trust(true, 0, 0o100644, 0), TemplateTrust::Trusted);
        assert_eq!(template_trust(true, 0, 0o100600, 0), TemplateTrust::Trusted);
        assert_eq!(
            template_trust(false, 0, 0o040755, 0),
            TemplateTrust::NotRegularFile
        );
        assert_eq!(
            template_trust(true, 1000, 0o100644, 0),
            TemplateTrust::WrongOwner { uid: 1000 }
        );
        assert_eq!(
            template_trust(true, 0, 0o100664, 0),
            TemplateTrust::WritableByOthers { mode: 0o100664 }
        );
        assert_eq!(
            template_trust(true, 0, 0o100646, 0),
            TemplateTrust::WritableByOthers { mode: 0o100646 }
        );
        assert_eq!(
            TemplateTrust::WritableByOthers { mode: 0o100666 }.describe(),
            "its mode 0666 lets group or other users write it"
        );
    }
}
