//! Where a qualification record lives, and the names a qualification run
//! derives from it.
//!
//! The production record ([`super::RECORD_PATH`]) is qualified as the
//! temporary account [`super::QUALIFY_TEMPORARY_USER`] in the transient scope
//! [`PRODUCTION_SCOPE_UNIT`], and only while the Pier service is stopped (a
//! qualification session would compete with served sessions for the
//! encoder). A development Pier names its own record (`--record`, or the
//! Pier configuration's `capture.wayland_qualification_record`); a run for
//! such a record uses its own temporary account and scope, derived from the
//! record path, so it never creates, uses or removes the production
//! account, and it may run beside the production service only when the
//! operator says so (`--allow-running-service`).

use std::fmt::{Display, Formatter};

use sha2::{Digest, Sha256};

use super::{QUALIFY_TEMPORARY_USER, QUALIFY_TEMPORARY_USER_GECOS, RECORD_PATH};

/// The transient scope a production qualification runs in.
pub const PRODUCTION_SCOPE_UNIT: &str = "arcen-wayland-qualify";

/// Longest record path accepted.
const MAX_RECORD_PATH: usize = 255;

/// Longest local account name `useradd` accepts.
const MAX_ACCOUNT_NAME: usize = 32;

/// Prefix every temporary qualification account carries.
const ACCOUNT_PREFIX: &str = "arcen";

/// Why a record location or its run cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualificationTargetError {
    /// The record path is not an absolute, plain `.json` path.
    RecordPath {
        /// The path given.
        path: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A temporary account name was given for the production record, or is
    /// not an `arcen…` local account name, or is the production account.
    TemporaryUser {
        /// The name given.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// `--allow-running-service` names the production record.
    RunningServiceNeedsOwnRecord,
    /// The Pier service is running and the run was not allowed beside it.
    ServiceRunning {
        /// The service unit.
        service: String,
    },
}

impl Display for QualificationTargetError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecordPath { path, reason } => {
                write!(formatter, "record path {path:?} cannot be used: {reason}")
            }
            Self::TemporaryUser { name, reason } => {
                write!(
                    formatter,
                    "temporary account {name:?} cannot be used: {reason}"
                )
            }
            Self::RunningServiceNeedsOwnRecord => write!(
                formatter,
                "--allow-running-service needs a record of its own (--record PATH other than \
                 {RECORD_PATH}); the production record is qualified only with the service \
                 stopped"
            ),
            Self::ServiceRunning { service } => write!(
                formatter,
                "{service} is running; stop it first (a qualification session would compete \
                 with served sessions for the encoder)"
            ),
        }
    }
}

impl std::error::Error for QualificationTargetError {}

/// The record a qualification writes or a Pier reads, with the temporary
/// account and scope a run for it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationTarget {
    record_path: String,
    temporary_user: String,
    temporary_user_gecos: String,
    scope_unit: String,
}

impl Default for QualificationTarget {
    fn default() -> Self {
        Self::production()
    }
}

impl QualificationTarget {
    /// The production record and its fixed names.
    #[must_use]
    pub fn production() -> Self {
        Self {
            record_path: RECORD_PATH.to_owned(),
            temporary_user: QUALIFY_TEMPORARY_USER.to_owned(),
            temporary_user_gecos: QUALIFY_TEMPORARY_USER_GECOS.to_owned(),
            scope_unit: PRODUCTION_SCOPE_UNIT.to_owned(),
        }
    }

    /// The record at `path`. The production path gives
    /// [`Self::production`]; any other path gets an account and scope named
    /// after a digest of the path (`arcen-wlq-<8 hex>`), or the account
    /// `temporary_user` when one is named.
    ///
    /// # Errors
    ///
    /// [`QualificationTargetError::RecordPath`] for a path that is not
    /// absolute, plain (`[A-Za-z0-9/._-]`, no `.`/`..` component) and
    /// `.json`; [`QualificationTargetError::TemporaryUser`] for an account
    /// named with the production record, or a name that is not `arcen…`
    /// lowercase letters, digits and `-`, or is the production account.
    pub fn for_record(
        path: &str,
        temporary_user: Option<&str>,
    ) -> Result<Self, QualificationTargetError> {
        if path == RECORD_PATH {
            return match temporary_user {
                None => Ok(Self::production()),
                Some(name) => Err(QualificationTargetError::TemporaryUser {
                    name: name.to_owned(),
                    reason: "the production record always uses its own account",
                }),
            };
        }
        validate_record_path(path)?;
        let tag = path_tag(path);
        let temporary_user = match temporary_user {
            Some(name) => {
                validate_temporary_user(name)?;
                name.to_owned()
            }
            None => format!("{QUALIFY_TEMPORARY_USER}-{tag}"),
        };
        Ok(Self {
            record_path: path.to_owned(),
            temporary_user,
            temporary_user_gecos: format!("{QUALIFY_TEMPORARY_USER_GECOS} record {tag}"),
            scope_unit: format!("{PRODUCTION_SCOPE_UNIT}-{tag}"),
        })
    }

    /// The record for an optional configured path (`None`: production).
    ///
    /// # Errors
    ///
    /// See [`Self::for_record`].
    pub fn from_config(path: Option<&str>) -> Result<Self, QualificationTargetError> {
        path.map_or_else(
            || Ok(Self::production()),
            |path| Self::for_record(path, None),
        )
    }

    /// Whether this is the production record.
    #[must_use]
    pub fn is_production(&self) -> bool {
        self.record_path == RECORD_PATH
    }

    /// Record path.
    #[must_use]
    pub fn record_path(&self) -> &str {
        &self.record_path
    }

    /// The temporary account a default run creates and removes.
    #[must_use]
    pub fn temporary_user(&self) -> &str {
        &self.temporary_user
    }

    /// Its `GECOS` field: how a leftover of an interrupted run for this
    /// record is recognised. Never another record's.
    #[must_use]
    pub fn temporary_user_gecos(&self) -> &str {
        &self.temporary_user_gecos
    }

    /// The transient scope a run for this record executes in.
    #[must_use]
    pub fn scope_unit(&self) -> &str {
        &self.scope_unit
    }

    /// The root-owned list of displays Decks asked Speed for without
    /// evidence, beside the record (`<record stem>-speed-requests.json`).
    #[must_use]
    pub fn requested_outputs_path(&self) -> String {
        let stem = self
            .record_path
            .strip_suffix(".json")
            .unwrap_or(&self.record_path);
        format!("{stem}-speed-requests.json")
    }

    /// The root-owned marker of the last automatic re-qualification, beside
    /// the record (`<record stem>-requalify.json`).
    #[must_use]
    pub fn requalify_marker_path(&self) -> String {
        let stem = self
            .record_path
            .strip_suffix(".json")
            .unwrap_or(&self.record_path);
        format!("{stem}-requalify.json")
    }

    /// The command that (re)qualifies this record.
    #[must_use]
    pub fn qualify_command(&self) -> String {
        if self.is_production() {
            super::QUALIFY_COMMAND.to_owned()
        } else {
            format!("{} --record {}", super::QUALIFY_COMMAND, self.record_path)
        }
    }

    /// Whether a run may start while `service` is `active`.
    ///
    /// `--allow-running-service` is accepted only for a record other than
    /// the production one; without it a running service refuses every run.
    ///
    /// # Errors
    ///
    /// [`QualificationTargetError::RunningServiceNeedsOwnRecord`] or
    /// [`QualificationTargetError::ServiceRunning`].
    pub fn check_service(
        &self,
        service: &str,
        active: bool,
        allow_running_service: bool,
    ) -> Result<(), QualificationTargetError> {
        if allow_running_service && self.is_production() {
            return Err(QualificationTargetError::RunningServiceNeedsOwnRecord);
        }
        if active && !allow_running_service {
            return Err(QualificationTargetError::ServiceRunning {
                service: service.to_owned(),
            });
        }
        Ok(())
    }
}

fn validate_record_path(path: &str) -> Result<(), QualificationTargetError> {
    let refuse = |reason| {
        Err(QualificationTargetError::RecordPath {
            path: path.to_owned(),
            reason,
        })
    };
    if path.len() > MAX_RECORD_PATH {
        return refuse("longer than 255 bytes");
    }
    let Some(relative) = path.strip_prefix('/') else {
        return refuse("not absolute");
    };
    if !path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return refuse("only letters, digits, '/', '.', '_' and '-' are accepted");
    }
    let components = relative.split('/').collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return refuse("empty, '.' or '..' component");
    }
    let file = components.last().copied().unwrap_or_default();
    if components.len() < 2
        || file.starts_with('.')
        || std::path::Path::new(file)
            .extension()
            .is_none_or(|extension| extension != "json")
    {
        return refuse("not a '<directory>/<name>.json' file");
    }
    Ok(())
}

fn validate_temporary_user(name: &str) -> Result<(), QualificationTargetError> {
    let refuse = |reason| {
        Err(QualificationTargetError::TemporaryUser {
            name: name.to_owned(),
            reason,
        })
    };
    if name == QUALIFY_TEMPORARY_USER {
        return refuse("it is the production qualification account");
    }
    if name.len() > MAX_ACCOUNT_NAME || name.len() <= ACCOUNT_PREFIX.len() {
        return refuse("not 6..=32 characters");
    }
    if !name.starts_with(ACCOUNT_PREFIX)
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return refuse("not 'arcen' followed by lowercase letters, digits or '-'");
    }
    Ok(())
}

/// Eight hex digits of the path's SHA-256.
fn path_tag(path: &str) -> String {
    Sha256::digest(path.as_bytes()).iter().take(4).fold(
        String::with_capacity(8),
        |mut tag, byte| {
            use std::fmt::Write as _;
            let _ = write!(tag, "{byte:02x}");
            tag
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "/var/lib/arcen-dev/speed/wayland-qualification.json";

    #[test]
    fn production_keeps_its_fixed_names() {
        let target = QualificationTarget::for_record(RECORD_PATH, None).unwrap();
        assert_eq!(target, QualificationTarget::production());
        assert_eq!(target, QualificationTarget::from_config(None).unwrap());
        assert!(target.is_production());
        assert_eq!(target.temporary_user(), "arcen-wlq");
        assert_eq!(target.temporary_user_gecos(), QUALIFY_TEMPORARY_USER_GECOS);
        assert_eq!(target.scope_unit(), "arcen-wayland-qualify");
        assert_eq!(
            target.requested_outputs_path(),
            "/var/lib/arcen/wayland-qualification-speed-requests.json"
        );
        assert_eq!(
            target.qualify_command(),
            "arcen-pier wayland qualify --write"
        );
        assert!(matches!(
            QualificationTarget::for_record(RECORD_PATH, Some("arcenspd")),
            Err(QualificationTargetError::TemporaryUser { .. })
        ));
    }

    #[test]
    fn another_record_never_uses_the_production_account() {
        let target = QualificationTarget::from_config(Some(DEV)).unwrap();
        assert!(!target.is_production());
        assert_eq!(target.record_path(), DEV);
        let user = target.temporary_user();
        assert!(user.starts_with("arcen-wlq-") && user.len() == "arcen-wlq-".len() + 8);
        assert_ne!(user, QUALIFY_TEMPORARY_USER);
        assert!(user.len() <= MAX_ACCOUNT_NAME);
        assert_ne!(target.temporary_user_gecos(), QUALIFY_TEMPORARY_USER_GECOS);
        assert!(
            target
                .temporary_user_gecos()
                .ends_with(&user["arcen-wlq-".len()..])
        );
        assert_ne!(target.scope_unit(), PRODUCTION_SCOPE_UNIT);
        assert_eq!(
            target.qualify_command(),
            format!("arcen-pier wayland qualify --write --record {DEV}")
        );
        // Stable for a path, distinct between paths.
        assert_eq!(target, QualificationTarget::for_record(DEV, None).unwrap());
        let other =
            QualificationTarget::for_record("/var/lib/arcen-dev/fidelity/q.json", None).unwrap();
        assert_ne!(other.temporary_user(), target.temporary_user());
        assert_ne!(other.scope_unit(), target.scope_unit());
        // A named account, never the production one.
        let named = QualificationTarget::for_record(DEV, Some("arcenspd")).unwrap();
        assert_eq!(named.temporary_user(), "arcenspd");
        assert_eq!(named.temporary_user_gecos(), target.temporary_user_gecos());
        for bad in [
            "arcen-wlq",
            "alice",
            "arcen",
            "Arcenspd",
            "arcen_spd",
            "arcen-a-very-long-name-over-32chx",
        ] {
            assert!(
                matches!(
                    QualificationTarget::for_record(DEV, Some(bad)),
                    Err(QualificationTargetError::TemporaryUser { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn record_paths_must_be_plain_absolute_json_files() {
        for bad in [
            "relative/q.json",
            "/q.json",
            "/var/lib/arcen-dev/q.txt",
            "/var/lib/arcen-dev/.json",
            "/var/lib/arcen-dev/.q.json",
            "/var/lib/../etc/q.json",
            "/var/lib/./q.json",
            "/var/lib//q.json",
            "/var/lib/arcen dev/q.json",
            "/var/lib/arcen:dev/q.json",
            "/var/lib/arcen-dev/q.json/",
        ] {
            assert!(
                matches!(
                    QualificationTarget::for_record(bad, None),
                    Err(QualificationTargetError::RecordPath { .. })
                ),
                "{bad}"
            );
        }
        let long = format!("/var/{}/q.json", "a".repeat(250));
        assert!(QualificationTarget::for_record(&long, None).is_err());
        assert!(QualificationTarget::for_record("/var/lib/arcen-dev/q.json", None).is_ok());
    }

    #[test]
    fn running_beside_the_service_needs_a_record_of_its_own() {
        let production = QualificationTarget::production();
        let dev = QualificationTarget::for_record(DEV, None).unwrap();
        let service = "arcen-pier.service";
        let running = Err(QualificationTargetError::ServiceRunning {
            service: service.to_owned(),
        });
        // Stopped service: every run may start.
        assert_eq!(production.check_service(service, false, false), Ok(()));
        assert_eq!(dev.check_service(service, false, false), Ok(()));
        // Running service: refused unless allowed, and allowing it needs a
        // record other than production's, whether or not it is running.
        assert_eq!(production.check_service(service, true, false), running);
        assert_eq!(dev.check_service(service, true, false), running);
        for active in [true, false] {
            assert_eq!(
                production.check_service(service, active, true),
                Err(QualificationTargetError::RunningServiceNeedsOwnRecord)
            );
        }
        assert_eq!(dev.check_service(service, true, true), Ok(()));
        assert!(
            QualificationTargetError::RunningServiceNeedsOwnRecord
                .to_string()
                .contains(RECORD_PATH)
        );
    }
}
