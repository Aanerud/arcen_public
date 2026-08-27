//! Field layout and the credential tile state machine.
//!
//! The Arcen tile exposes exactly the fields the task calls for — a label,
//! a username edit, a masked password, a submit button, and a status line — and
//! nothing else. The state machine here is deliberately platform-independent:
//! LogonUI drives it through the COM credential object, but all of the interesting
//! transition and bounds logic lives here where it is unit-tested on any OS. The
//! COM layer only translates Win32 field ids to [`FieldId`] and copies strings.

use crate::secret::SecretWide;

/// Number of fields the provider advertises.
pub const FIELD_COUNT: u32 = 5;

/// Max UTF-16 units accepted into the username field. Matches the broker's
/// 256-unit account-name safety cap.
pub const MAX_USERNAME_UNITS: usize = 256;

/// Max UTF-16 units accepted into the password field. Comfortably under the
/// broker's 4096-unit credential cap while bounding LogonUI-provided input.
pub const MAX_PASSWORD_UNITS: usize = 512;

/// Stable field identifiers. The discriminants are the Win32 `dwFieldID`s and
/// must stay in this order to match [`field_specs`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldId {
    Label = 0,
    Username = 1,
    Password = 2,
    Submit = 3,
    Status = 4,
}

impl FieldId {
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Label),
            1 => Some(Self::Username),
            2 => Some(Self::Password),
            3 => Some(Self::Submit),
            4 => Some(Self::Status),
            _ => None,
        }
    }
}

/// Platform-independent mirror of the `CPFT_*` field types this provider uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldKind {
    LargeText,
    EditText,
    PasswordText,
    SubmitButton,
    SmallText,
}

/// Static metadata for one field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldSpec {
    pub id: FieldId,
    pub kind: FieldKind,
    pub label: &'static str,
}

/// The full, ordered field table. Index equals `dwFieldID`.
pub fn field_specs() -> [FieldSpec; FIELD_COUNT as usize] {
    [
        FieldSpec {
            id: FieldId::Label,
            kind: FieldKind::LargeText,
            label: "Arcen",
        },
        FieldSpec {
            id: FieldId::Username,
            kind: FieldKind::EditText,
            label: "User name",
        },
        FieldSpec {
            id: FieldId::Password,
            kind: FieldKind::PasswordText,
            label: "Password",
        },
        FieldSpec {
            id: FieldId::Submit,
            kind: FieldKind::SubmitButton,
            label: "Sign in with Arcen",
        },
        FieldSpec {
            id: FieldId::Status,
            kind: FieldKind::SmallText,
            label: "Status",
        },
    ]
}

/// Why a field mutation was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum FieldError {
    /// The `dwFieldID` did not map to a known field.
    UnknownField,
    /// The field exists but is not an input the UI may write.
    NotWritable,
    /// The supplied value exceeded the field's bound.
    TooLong,
}

/// What `GetCredentialCount` should report to LogonUI for the current state.
///
/// `autologon` is true for **exactly one** query after a broker credential is
/// armed: LogonUI then serializes the default tile once. A later query returns
/// `autologon == false` (with the tile still selected as default) so a failed
/// first-login attempt cannot loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialCountReport {
    pub count: u32,
    pub default_index: Option<u32>,
    pub autologon: bool,
}

/// A broker-pushed credential awaiting a single automatic submission.
struct PendingAutologon {
    username: String,
    password: SecretWide,
    /// The sealed-envelope request id, kept for correlated logging only.
    request_id: u64,
    /// Monotonic wall-clock deadline after which this credential must be scrubbed.
    expires_at_ms: u64,
}

impl Drop for PendingAutologon {
    fn drop(&mut self) {
        self.password.clear();
        // The account name is not a secret, but scrub it so nothing about the
        // remote sign-in lingers after the one-shot is consumed or cleared.
        use zeroize::Zeroize;
        self.username.zeroize();
    }
}

/// Mutable per-tile input state driven by LogonUI callbacks.
pub struct CredentialFields {
    username: String,
    password: SecretWide,
    status: Option<String>,
    selected: bool,
    /// A broker-pushed credential to auto-submit exactly once, plus whether the
    /// one autologon offer has already been made this arming.
    pending_autologon: Option<PendingAutologon>,
    autologon_offered: bool,
}

impl CredentialFields {
    pub fn new() -> Self {
        Self {
            username: String::new(),
            password: SecretWide::new(),
            status: None,
            selected: false,
            pending_autologon: None,
            autologon_offered: false,
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &SecretWide {
        &self.password
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// The tile became the active tile.
    pub fn set_selected(&mut self) {
        self.selected = true;
    }

    /// The tile was deselected: scrub every secret and clear transient status so
    /// nothing sensitive survives a tile switch.
    pub fn set_deselected(&mut self) {
        self.selected = false;
        self.clear_secret();
        self.status = None;
    }

    /// Apply a `SetStringValue` from the UI. Only the username and password
    /// fields are writable; the rest are display-only.
    pub fn set_string(&mut self, field: FieldId, value: &str) -> Result<(), FieldError> {
        match field {
            FieldId::Username => {
                if value.encode_utf16().count() > MAX_USERNAME_UNITS {
                    return Err(FieldError::TooLong);
                }
                self.username.clear();
                self.username.push_str(value);
                Ok(())
            }
            FieldId::Password => self.set_password(SecretWide::from_text(value)),
            FieldId::Label | FieldId::Submit | FieldId::Status => Err(FieldError::NotWritable),
        }
    }

    /// Replace the password with already-wide secret storage. This avoids an
    /// intermediate UTF-8 password allocation in the COM input path.
    pub fn set_password(&mut self, password: SecretWide) -> Result<(), FieldError> {
        if password.len() > MAX_PASSWORD_UNITS {
            return Err(FieldError::TooLong);
        }
        self.password = password;
        Ok(())
    }

    /// Move the password out, immediately replacing it with an empty secret.
    /// The returned value remains zeroizing and is scrubbed on every exit path.
    pub fn take_password(&mut self) -> SecretWide {
        core::mem::take(&mut self.password)
    }

    /// Value the UI should render for a `GetStringValue`.
    ///
    /// The password field always reports empty: we never hand LogonUI a plaintext
    /// copy of the stored secret to manage.
    pub fn get_string(&self, field: FieldId) -> Result<String, FieldError> {
        match field {
            FieldId::Label => Ok(field_specs()[FieldId::Label as usize].label.to_string()),
            FieldId::Username => Ok(self.username.clone()),
            FieldId::Password => Ok(String::new()),
            FieldId::Status => Ok(self.status.clone().unwrap_or_default()),
            FieldId::Submit => Err(FieldError::NotWritable),
        }
    }

    /// Set the status line shown under the tile.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// Scrub only the password, leaving username/status intact.
    pub fn clear_secret(&mut self) {
        self.password.clear();
    }

    /// Post-submit / post-result cleanup: scrub the secret and drop status.
    /// Username is preserved so a retry does not force re-typing the account.
    /// Any one-shot autologon is cleared: a completed sign-in attempt, success
    /// or failure, must never re-submit a pushed credential.
    pub fn reset_after_result(&mut self) {
        self.clear_secret();
        self.status = None;
        self.clear_autologon();
    }

    /// Reset all per-scenario input when LogonUI changes usage scenario.
    pub fn reset(&mut self) {
        self.username.clear();
        self.clear_secret();
        self.status = None;
        self.selected = false;
        self.clear_autologon();
    }

    /// Both required inputs are present.
    pub fn can_submit(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty()
    }

    /// The field the submit button is adjacent to (pressing Enter here submits).
    pub fn submit_target(&self) -> FieldId {
        FieldId::Password
    }

    /// Arm a broker-pushed credential for a single automatic submission. Any
    /// previously pending credential is scrubbed and the one-shot offer latch is
    /// reset so the freshly armed credential triggers exactly one autologon.
    pub fn arm_autologon(
        &mut self,
        username: String,
        password: SecretWide,
        request_id: u64,
        expires_at_ms: u64,
    ) {
        self.pending_autologon = Some(PendingAutologon {
            username,
            password,
            request_id,
            expires_at_ms,
        });
        self.autologon_offered = false;
    }

    /// The request id of the pending autologon, for correlated logging.
    pub fn autologon_request_id(&self) -> Option<u64> {
        self.pending_autologon.as_ref().map(|p| p.request_id)
    }

    pub fn has_pending_autologon(&self) -> bool {
        self.pending_autologon.is_some()
    }

    /// Compute what `GetCredentialCount` should report given whether the current
    /// usage scenario is enumerable, latching the one autologon offer.
    pub fn autologon_report(&mut self, enumerable: bool, now_ms: u64) -> CredentialCountReport {
        self.expire_autologon_at(now_ms);
        if !enumerable {
            return CredentialCountReport {
                count: 0,
                default_index: None,
                autologon: false,
            };
        }
        if self.pending_autologon.is_some() {
            let autologon = !self.autologon_offered;
            self.autologon_offered = true;
            CredentialCountReport {
                count: 1,
                default_index: Some(0),
                autologon,
            }
        } else {
            CredentialCountReport {
                count: 1,
                default_index: None,
                autologon: false,
            }
        }
    }

    /// Consume the pending autologon for serialization, returning the account
    /// name and its secret. The one-shot is cleared so it can never fire twice.
    pub fn take_autologon(&mut self, now_ms: u64) -> Option<(String, SecretWide)> {
        self.expire_autologon_at(now_ms);
        let mut pending = self.pending_autologon.take()?;
        self.autologon_offered = false;
        let username = core::mem::take(&mut pending.username);
        let password = core::mem::take(&mut pending.password);
        Some((username, password))
    }

    /// Scrub and drop any pending autologon and reset the offer latch.
    pub fn clear_autologon(&mut self) {
        // `PendingAutologon::drop` scrubs the secret and account name.
        self.pending_autologon = None;
        self.autologon_offered = false;
    }

    /// Clear an expired credential only when it still belongs to `request_id`.
    ///
    /// This makes delayed expiry workers harmless after a newer credential has
    /// replaced the request they were created for.
    pub fn expire_autologon(&mut self, request_id: u64, now_ms: u64) -> bool {
        let should_clear = self.pending_autologon.as_ref().is_some_and(|pending| {
            pending.request_id == request_id && now_ms >= pending.expires_at_ms
        });
        if should_clear {
            self.clear_autologon();
        }
        should_clear
    }

    /// Clear a request after an independently elapsed timer, regardless of wall
    /// clock movement, but only if it is still the active request.
    pub fn clear_autologon_request(&mut self, request_id: u64) -> bool {
        let should_clear = self
            .pending_autologon
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id);
        if should_clear {
            self.clear_autologon();
        }
        should_clear
    }

    fn expire_autologon_at(&mut self, now_ms: u64) {
        let should_clear = self
            .pending_autologon
            .as_ref()
            .is_some_and(|pending| now_ms >= pending.expires_at_ms);
        if should_clear {
            self.clear_autologon();
        }
    }
}

impl Default for CredentialFields {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_table_is_ordered_and_indexable_by_id() {
        let specs = field_specs();
        for (index, spec) in specs.iter().enumerate() {
            assert_eq!(spec.id.as_u32() as usize, index);
            assert_eq!(FieldId::from_u32(index as u32), Some(spec.id));
        }
        assert_eq!(FieldId::from_u32(FIELD_COUNT), None);
        assert_eq!(specs.len() as u32, FIELD_COUNT);
    }

    #[test]
    fn kinds_match_the_required_tile_shape() {
        let specs = field_specs();
        assert_eq!(specs[0].kind, FieldKind::LargeText);
        assert_eq!(specs[1].kind, FieldKind::EditText);
        assert_eq!(specs[2].kind, FieldKind::PasswordText);
        assert_eq!(specs[3].kind, FieldKind::SubmitButton);
        assert_eq!(specs[4].kind, FieldKind::SmallText);
    }

    #[test]
    fn username_and_password_are_writable_others_are_not() {
        let mut fields = CredentialFields::new();
        assert!(fields.set_string(FieldId::Username, "alice").is_ok());
        assert!(fields.set_string(FieldId::Password, "pw").is_ok());
        assert_eq!(
            fields.set_string(FieldId::Label, "x"),
            Err(FieldError::NotWritable)
        );
        assert_eq!(
            fields.set_string(FieldId::Submit, "x"),
            Err(FieldError::NotWritable)
        );
        assert_eq!(
            fields.set_string(FieldId::Status, "x"),
            Err(FieldError::NotWritable)
        );
    }

    #[test]
    fn field_bounds_are_enforced() {
        let mut fields = CredentialFields::new();
        let long_user = "u".repeat(MAX_USERNAME_UNITS + 1);
        assert_eq!(
            fields.set_string(FieldId::Username, &long_user),
            Err(FieldError::TooLong)
        );
        let long_pass = "p".repeat(MAX_PASSWORD_UNITS + 1);
        assert_eq!(
            fields.set_string(FieldId::Password, &long_pass),
            Err(FieldError::TooLong)
        );
        // Exactly at the bound is accepted.
        assert!(fields
            .set_string(FieldId::Username, &"u".repeat(MAX_USERNAME_UNITS))
            .is_ok());
    }

    #[test]
    fn get_string_never_returns_the_password() {
        let mut fields = CredentialFields::new();
        fields.set_string(FieldId::Password, "hunter2").unwrap();
        assert_eq!(fields.get_string(FieldId::Password).unwrap(), "");
        fields.set_string(FieldId::Username, "bob").unwrap();
        assert_eq!(fields.get_string(FieldId::Username).unwrap(), "bob");
        assert_eq!(fields.get_string(FieldId::Label).unwrap(), "Arcen");
    }

    #[test]
    fn can_submit_requires_both_inputs() {
        let mut fields = CredentialFields::new();
        assert!(!fields.can_submit());
        fields.set_string(FieldId::Username, "bob").unwrap();
        assert!(!fields.can_submit());
        fields.set_string(FieldId::Password, "pw").unwrap();
        assert!(fields.can_submit());
        assert_eq!(fields.submit_target(), FieldId::Password);
    }

    #[test]
    fn deselect_scrubs_secret_and_status_but_keeps_selected_flag_false() {
        let mut fields = CredentialFields::new();
        fields.set_selected();
        fields.set_string(FieldId::Password, "secret").unwrap();
        fields.set_status("working");
        assert!(fields.is_selected());
        fields.set_deselected();
        assert!(!fields.is_selected());
        assert!(fields.password().is_empty());
        assert_eq!(fields.status(), None);
    }

    #[test]
    fn reset_after_result_keeps_username_scrubs_secret() {
        let mut fields = CredentialFields::new();
        fields.set_string(FieldId::Username, "carol").unwrap();
        fields.set_string(FieldId::Password, "pw").unwrap();
        fields.set_status("bad password");
        fields.reset_after_result();
        assert_eq!(fields.username(), "carol");
        assert!(fields.password().is_empty());
        assert_eq!(fields.status(), None);
    }

    #[test]
    fn full_reset_clears_all_per_scenario_state() {
        let mut fields = CredentialFields::new();
        fields.set_string(FieldId::Username, "carol").unwrap();
        fields.set_string(FieldId::Password, "pw").unwrap();
        fields.set_status("retry");
        fields.set_selected();
        fields.reset();
        assert_eq!(fields.username(), "");
        assert!(fields.password().is_empty());
        assert_eq!(fields.status(), None);
        assert!(!fields.is_selected());
    }

    #[test]
    fn taking_password_leaves_empty_state_and_keeps_zeroizing_owner() {
        let mut fields = CredentialFields::new();
        fields.set_string(FieldId::Password, "pw").unwrap();
        let password = fields.take_password();
        assert_eq!(password.as_utf16(), &[b'p' as u16, b'w' as u16]);
        assert!(fields.password().is_empty());
    }

    #[test]
    fn autologon_offers_exactly_once_then_stays_default() {
        let mut fields = CredentialFields::new();
        // Nothing armed: enumerable manual tile, no default, no autologon.
        assert_eq!(
            fields.autologon_report(true, 0),
            CredentialCountReport {
                count: 1,
                default_index: None,
                autologon: false
            }
        );
        fields.arm_autologon("artist".to_string(), SecretWide::from_text("pw"), 7, 30_000);
        assert_eq!(fields.autologon_request_id(), Some(7));
        // First query auto-submits the default tile exactly once.
        assert_eq!(
            fields.autologon_report(true, 0),
            CredentialCountReport {
                count: 1,
                default_index: Some(0),
                autologon: true
            }
        );
        // A repeat query keeps the default but must not auto-submit again.
        assert_eq!(
            fields.autologon_report(true, 0),
            CredentialCountReport {
                count: 1,
                default_index: Some(0),
                autologon: false
            }
        );
    }

    #[test]
    fn autologon_is_not_offered_when_scenario_is_not_enumerable() {
        let mut fields = CredentialFields::new();
        fields.arm_autologon("artist".to_string(), SecretWide::from_text("pw"), 1, 30_000);
        assert_eq!(
            fields.autologon_report(false, 0),
            CredentialCountReport {
                count: 0,
                default_index: None,
                autologon: false
            }
        );
    }

    #[test]
    fn taking_autologon_consumes_the_one_shot() {
        let mut fields = CredentialFields::new();
        fields.arm_autologon(
            r"CORP\artist".to_string(),
            SecretWide::from_text("pw"),
            5,
            30_000,
        );
        assert!(fields.has_pending_autologon());
        let (username, password) = fields.take_autologon(0).expect("pending");
        assert_eq!(username, r"CORP\artist");
        assert_eq!(password.as_utf16(), SecretWide::from_text("pw").as_utf16());
        assert!(!fields.has_pending_autologon());
        // Consumed: a later count query offers no autologon.
        assert_eq!(
            fields.autologon_report(true, 0),
            CredentialCountReport {
                count: 1,
                default_index: None,
                autologon: false
            }
        );
        assert!(fields.take_autologon(0).is_none());
    }

    #[test]
    fn result_and_reset_clear_a_pending_autologon() {
        let mut fields = CredentialFields::new();
        fields.arm_autologon("artist".to_string(), SecretWide::from_text("pw"), 1, 30_000);
        fields.reset_after_result();
        assert!(!fields.has_pending_autologon());

        fields.arm_autologon("artist".to_string(), SecretWide::from_text("pw"), 2, 30_000);
        fields.reset();
        assert!(!fields.has_pending_autologon());

        fields.arm_autologon("artist".to_string(), SecretWide::from_text("pw"), 3, 30_000);
        fields.clear_autologon();
        assert!(!fields.has_pending_autologon());
    }

    #[test]
    fn expired_autologon_is_scrubbed_before_offer_or_serialization() {
        let mut fields = CredentialFields::new();
        fields.arm_autologon("artist".to_string(), SecretWide::from_text("pw"), 7, 100);
        assert_eq!(
            fields.autologon_report(true, 100),
            CredentialCountReport {
                count: 1,
                default_index: None,
                autologon: false,
            }
        );
        assert!(fields.take_autologon(100).is_none());
    }

    #[test]
    fn old_expiry_cannot_clear_a_replacement_request() {
        let mut fields = CredentialFields::new();
        fields.arm_autologon("first".to_string(), SecretWide::from_text("one"), 1, 100);
        fields.arm_autologon("second".to_string(), SecretWide::from_text("two"), 2, 200);
        assert!(!fields.expire_autologon(1, 100));
        assert!(fields.has_pending_autologon());
        assert!(!fields.clear_autologon_request(1));
        assert!(fields.clear_autologon_request(2));
        assert!(!fields.has_pending_autologon());
    }
}
