//! OS-free host certificate provisioning decisions.
//!
//! Validation, pinning and rotation already live in [`crate::tls`]. What was
//! missing was the step before any of that: given whatever happens to be on
//! disk and what the operator asked for, decide whether to create material,
//! reuse it, renew it, replace the key, or refuse.
//!
//! Each host used to answer this differently — Linux inferred it from command
//! line flags, Windows from its installer, macOS not at all — which is exactly
//! how three platforms drift into three different trust stories. The decision
//! is pure and lives here; platform code only reports what it found and
//! carries out the plan.
//!
//! Two rules drive most of the behaviour:
//!
//! * **Half a key pair is never quietly completed.** A certificate without its
//!   key, or a key without its certificate, means something went wrong or
//!   something else owns the directory. Generating the missing half would
//!   silently change which key clients are trusting.
//! * **Changing what clients trust is never implicit.** Renewal keeps the key,
//!   so existing pins keep working. Anything that replaces the key has to be
//!   asked for, because every pinned Deck will refuse the new identity until
//!   it is re-pinned.

use std::fmt::{Display, Formatter};

/// What the operator asked the host to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningRequest {
    /// Make sure usable material exists, without disturbing anything valid.
    Ensure,
    /// Issue a new certificate for the existing key, preserving pins.
    Renew,
    /// Issue a new certificate *and* a new key. This breaks existing pins.
    Rekey,
    /// Take ownership of material this host did not create.
    AdoptLegacy,
}

/// Whether the material on disk carries this host's ownership marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialOwnership {
    /// Created and marked by this host.
    Owned,
    /// Present but not marked as ours.
    Foreign,
}

/// What a host found in its certificate directory.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterialState {
    /// A certificate file is present.
    pub certificate_present: bool,
    /// A private key file is present.
    pub key_present: bool,
    /// Ownership of the material, when anything was found.
    pub ownership: Option<MaterialOwnership>,
    /// Whether the certificate parsed and passed validation.
    pub certificate_valid: bool,
    /// Whether the certificate is within its renewal window or already past
    /// its expiry.
    pub expiring_or_expired: bool,
    /// Whether an interrupted publication left staged files behind.
    pub stale_staging_present: bool,
}

impl MaterialState {
    /// A directory with no certificate material at all.
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            certificate_present: false,
            key_present: false,
            ownership: None,
            certificate_valid: false,
            expiring_or_expired: false,
            stale_staging_present: false,
        }
    }

    /// Healthy material this host created.
    #[must_use]
    pub const fn owned_valid() -> Self {
        Self {
            certificate_present: true,
            key_present: true,
            ownership: Some(MaterialOwnership::Owned),
            certificate_valid: true,
            expiring_or_expired: false,
            stale_staging_present: false,
        }
    }

    /// Returns whether exactly one half of the pair is present.
    #[must_use]
    pub const fn is_partial(self) -> bool {
        self.certificate_present != self.key_present
    }

    /// Returns whether both halves are present.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.certificate_present && self.key_present
    }
}

/// What the host should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningAction {
    /// Existing material is fine; change nothing.
    KeepExisting,
    /// Create a new key and certificate.
    CreateNew,
    /// Issue a new certificate over the existing key. Pins survive.
    RenewPreservingKey,
    /// Create a new key and certificate, replacing trusted material.
    ReplaceKeyAndCertificate,
    /// Reissue over the existing key and record this host as the owner.
    ///
    /// Adoption reissues rather than only writing a marker. Marking someone
    /// else's certificate as ours would claim ownership of material we cannot
    /// reproduce or renew; reissuing over the same key leaves clients' pins
    /// intact while putting the host genuinely in control of the certificate.
    AdoptAndRenew,
}

impl ProvisioningAction {
    /// Returns whether carrying out this action changes what clients trust.
    ///
    /// Callers use this to decide whether operators and pinned clients have to
    /// be warned before the change, not merely told after it.
    #[must_use]
    pub const fn changes_client_trust(self) -> bool {
        matches!(self, Self::CreateNew | Self::ReplaceKeyAndCertificate)
    }

    /// Returns whether this action writes new material.
    #[must_use]
    pub const fn writes_material(self) -> bool {
        matches!(
            self,
            Self::CreateNew
                | Self::RenewPreservingKey
                | Self::ReplaceKeyAndCertificate
                | Self::AdoptAndRenew
        )
    }
}

/// Why provisioning was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningRefusal {
    /// Only one half of the key pair exists.
    IncompleteMaterial,
    /// The material belongs to something else and adoption was not requested.
    ForeignMaterial,
    /// Renewal needs an existing key to renew over.
    NothingToRenew,
    /// A rekey was asked for but there is nothing to rekey.
    ///
    /// This is deliberately not treated as first-time creation. Asking to
    /// replace a key when no key exists usually means the wrong directory was
    /// given, and silently creating material there is worse than stopping.
    NothingToRekey,
    /// Adoption was requested but there is nothing to adopt.
    NothingToAdopt,
    /// Adoption was requested for material this host already owns.
    AlreadyOwned,
    /// The certificate on disk did not validate and was not replaceable
    /// without an explicit instruction.
    InvalidCertificate,
}

impl ProvisioningRefusal {
    /// Returns a stable operator-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IncompleteMaterial => "incomplete_material",
            Self::ForeignMaterial => "foreign_material",
            Self::NothingToRenew => "nothing_to_renew",
            Self::NothingToRekey => "nothing_to_rekey",
            Self::NothingToAdopt => "nothing_to_adopt",
            Self::AlreadyOwned => "already_owned",
            Self::InvalidCertificate => "invalid_certificate",
        }
    }

    /// Returns a sentence explaining what the operator should do instead.
    #[must_use]
    pub const fn guidance(self) -> &'static str {
        match self {
            Self::IncompleteMaterial => {
                "only one of the certificate and key is present; remove or restore the pair rather than generating the missing half"
            }
            Self::ForeignMaterial => {
                "the certificate was not created by this host; re-run with the adopt-legacy request to take ownership"
            }
            Self::NothingToRenew => "there is no existing key to renew; request creation instead",
            Self::NothingToRekey => {
                "there is no existing key to replace; check the directory, or request creation instead"
            }
            Self::NothingToAdopt => {
                "there is no existing material to adopt; request creation instead"
            }
            Self::AlreadyOwned => "this host already owns the material; adoption is unnecessary",
            Self::InvalidCertificate => {
                "the certificate did not validate; request renewal or rekey to replace it"
            }
        }
    }
}

impl Display for ProvisioningRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for ProvisioningRefusal {}

/// A decided provisioning plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvisioningPlan {
    /// What to do.
    pub action: ProvisioningAction,
    /// Whether interrupted staging files must be cleared first.
    ///
    /// A previous run that died between writing and publishing leaves these
    /// behind. They are never adopted as real material, because nothing proves
    /// they were completely written.
    pub clear_stale_staging: bool,
    /// Whether this plan invalidates existing client pins.
    pub invalidates_pins: bool,
}

/// Decides what to do with the material a host found.
///
/// # Errors
///
/// Returns a [`ProvisioningRefusal`] rather than guessing whenever the state on
/// disk and the request do not describe exactly one safe outcome.
pub fn plan(
    request: ProvisioningRequest,
    state: MaterialState,
) -> Result<ProvisioningPlan, ProvisioningRefusal> {
    // Half a pair is ambiguous under every request. Completing it would change
    // the served identity without anyone asking.
    if state.is_partial() {
        return Err(ProvisioningRefusal::IncompleteMaterial);
    }

    let foreign = matches!(state.ownership, Some(MaterialOwnership::Foreign));
    let action = match request {
        ProvisioningRequest::AdoptLegacy => {
            if !state.is_complete() {
                return Err(ProvisioningRefusal::NothingToAdopt);
            }
            if !foreign {
                return Err(ProvisioningRefusal::AlreadyOwned);
            }
            ProvisioningAction::AdoptAndRenew
        }
        ProvisioningRequest::Ensure => {
            if !state.is_complete() {
                ProvisioningAction::CreateNew
            } else if foreign {
                // Silently replacing someone else's material would break every
                // client already trusting it.
                return Err(ProvisioningRefusal::ForeignMaterial);
            } else if !state.certificate_valid {
                return Err(ProvisioningRefusal::InvalidCertificate);
            } else if state.expiring_or_expired {
                // Renewal keeps the key, so pins survive the rotation.
                ProvisioningAction::RenewPreservingKey
            } else {
                ProvisioningAction::KeepExisting
            }
        }
        ProvisioningRequest::Renew => {
            if !state.is_complete() {
                return Err(ProvisioningRefusal::NothingToRenew);
            }
            if foreign {
                return Err(ProvisioningRefusal::ForeignMaterial);
            }
            ProvisioningAction::RenewPreservingKey
        }
        ProvisioningRequest::Rekey => {
            // Rekeying nothing is almost always a wrong directory rather than a
            // request to bootstrap, so it stops instead of creating material in
            // an unexpected place.
            if !state.is_complete() {
                return Err(ProvisioningRefusal::NothingToRekey);
            }
            if foreign {
                return Err(ProvisioningRefusal::ForeignMaterial);
            }
            ProvisioningAction::ReplaceKeyAndCertificate
        }
    };

    Ok(ProvisioningPlan {
        action,
        // Staging debris is cleared before anything that writes, so a new
        // publication never races a half-written file from a dead run.
        clear_stale_staging: state.stale_staging_present && action.writes_material(),
        invalidates_pins: action.changes_client_trust(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_on_an_empty_directory_creates_material() {
        let plan = plan(ProvisioningRequest::Ensure, MaterialState::absent())
            .expect("empty directory is provisionable");
        assert_eq!(plan.action, ProvisioningAction::CreateNew);
        assert!(plan.invalidates_pins);
    }

    #[test]
    fn healthy_owned_material_is_left_alone() {
        let plan = plan(ProvisioningRequest::Ensure, MaterialState::owned_valid())
            .expect("valid material is usable");
        assert_eq!(plan.action, ProvisioningAction::KeepExisting);
        assert!(!plan.invalidates_pins);
        assert!(!plan.clear_stale_staging);
    }

    #[test]
    fn half_a_key_pair_is_always_refused() {
        // Generating the missing half would silently change the served
        // identity, so no request may proceed from here.
        for request in [
            ProvisioningRequest::Ensure,
            ProvisioningRequest::Renew,
            ProvisioningRequest::Rekey,
            ProvisioningRequest::AdoptLegacy,
        ] {
            let cert_only = MaterialState {
                certificate_present: true,
                key_present: false,
                ..MaterialState::absent()
            };
            let key_only = MaterialState {
                certificate_present: false,
                key_present: true,
                ..MaterialState::absent()
            };
            assert_eq!(
                plan(request, cert_only),
                Err(ProvisioningRefusal::IncompleteMaterial)
            );
            assert_eq!(
                plan(request, key_only),
                Err(ProvisioningRefusal::IncompleteMaterial)
            );
        }
    }

    #[test]
    fn foreign_material_is_never_replaced_without_being_asked() {
        let foreign = MaterialState {
            ownership: Some(MaterialOwnership::Foreign),
            ..MaterialState::owned_valid()
        };
        assert_eq!(
            plan(ProvisioningRequest::Ensure, foreign),
            Err(ProvisioningRefusal::ForeignMaterial)
        );
        assert_eq!(
            plan(ProvisioningRequest::Renew, foreign),
            Err(ProvisioningRefusal::ForeignMaterial)
        );
        assert_eq!(
            plan(ProvisioningRequest::Rekey, foreign),
            Err(ProvisioningRefusal::ForeignMaterial)
        );
    }

    #[test]
    fn adoption_is_explicit_and_only_applies_to_foreign_material() {
        let foreign = MaterialState {
            ownership: Some(MaterialOwnership::Foreign),
            ..MaterialState::owned_valid()
        };
        let adopted = plan(ProvisioningRequest::AdoptLegacy, foreign).expect("adoptable");
        // Adoption reissues over the same key rather than only marking someone
        // else's certificate as ours.
        assert_eq!(adopted.action, ProvisioningAction::AdoptAndRenew);
        assert!(adopted.action.writes_material());
        // The key is preserved, so pins survive.
        assert!(!adopted.invalidates_pins);

        assert_eq!(
            plan(
                ProvisioningRequest::AdoptLegacy,
                MaterialState::owned_valid()
            ),
            Err(ProvisioningRefusal::AlreadyOwned)
        );
        assert_eq!(
            plan(ProvisioningRequest::AdoptLegacy, MaterialState::absent()),
            Err(ProvisioningRefusal::NothingToAdopt)
        );
    }

    #[test]
    fn renewal_preserves_the_key_but_rekey_does_not() {
        let renewed =
            plan(ProvisioningRequest::Renew, MaterialState::owned_valid()).expect("renewable");
        assert_eq!(renewed.action, ProvisioningAction::RenewPreservingKey);
        assert!(
            !renewed.invalidates_pins,
            "renewal must keep existing pins working"
        );

        let rekeyed =
            plan(ProvisioningRequest::Rekey, MaterialState::owned_valid()).expect("rekeyable");
        assert_eq!(rekeyed.action, ProvisioningAction::ReplaceKeyAndCertificate);
        assert!(
            rekeyed.invalidates_pins,
            "replacing the key must be reported as breaking pins"
        );
    }

    #[test]
    fn expiring_material_renews_rather_than_rekeying() {
        let expiring = MaterialState {
            expiring_or_expired: true,
            ..MaterialState::owned_valid()
        };
        let plan = plan(ProvisioningRequest::Ensure, expiring).expect("renewable");
        assert_eq!(plan.action, ProvisioningAction::RenewPreservingKey);
        assert!(!plan.invalidates_pins);
    }

    #[test]
    fn an_invalid_certificate_is_reported_rather_than_silently_replaced() {
        let invalid = MaterialState {
            certificate_valid: false,
            ..MaterialState::owned_valid()
        };
        assert_eq!(
            plan(ProvisioningRequest::Ensure, invalid),
            Err(ProvisioningRefusal::InvalidCertificate)
        );
        // But an explicit renew or rekey may replace it.
        assert!(plan(ProvisioningRequest::Renew, invalid).is_ok());
        assert!(plan(ProvisioningRequest::Rekey, invalid).is_ok());
    }

    #[test]
    fn renew_on_an_empty_directory_is_refused_rather_than_creating() {
        // Asking to renew when there is nothing to renew is a mistake worth
        // reporting, not an implicit first-time creation.
        assert_eq!(
            plan(ProvisioningRequest::Renew, MaterialState::absent()),
            Err(ProvisioningRefusal::NothingToRenew)
        );
    }

    #[test]
    fn rekey_on_an_empty_directory_is_refused_rather_than_bootstrapping() {
        // Matches the established Linux behaviour: asking to replace a key
        // when none exists usually means the wrong directory was given, and
        // quietly creating material there is worse than stopping.
        assert_eq!(
            plan(ProvisioningRequest::Rekey, MaterialState::absent()),
            Err(ProvisioningRefusal::NothingToRekey)
        );
    }

    #[test]
    fn interrupted_staging_is_cleared_only_when_writing() {
        let interrupted_write = MaterialState {
            stale_staging_present: true,
            ..MaterialState::absent()
        };
        let creating = plan(ProvisioningRequest::Ensure, interrupted_write).expect("creatable");
        assert!(creating.clear_stale_staging);

        let interrupted_idle = MaterialState {
            stale_staging_present: true,
            ..MaterialState::owned_valid()
        };
        let keeping = plan(ProvisioningRequest::Ensure, interrupted_idle).expect("usable");
        assert_eq!(keeping.action, ProvisioningAction::KeepExisting);
        assert!(
            !keeping.clear_stale_staging,
            "leaving material untouched must not delete anything"
        );
    }

    #[test]
    fn refusals_carry_a_stable_code_and_operator_guidance() {
        for refusal in [
            ProvisioningRefusal::IncompleteMaterial,
            ProvisioningRefusal::ForeignMaterial,
            ProvisioningRefusal::NothingToRenew,
            ProvisioningRefusal::NothingToRekey,
            ProvisioningRefusal::NothingToAdopt,
            ProvisioningRefusal::AlreadyOwned,
            ProvisioningRefusal::InvalidCertificate,
        ] {
            assert!(!refusal.as_str().is_empty());
            assert!(!refusal.guidance().is_empty());
            assert_ne!(refusal.as_str(), refusal.guidance());
        }
    }
}
