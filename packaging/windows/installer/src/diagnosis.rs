//! Classification of `validate-config` failures.
//!
//! Kept out of `#[cfg(windows)]`, for the same reason as `acl`: this decides
//! whether the installer preserves or destroys an operator's configuration, so
//! it stays unit-testable on every host.

/// Whether a `validate-config` failure is about TLS material rather than the
/// configuration itself.
///
/// `arcen-pier validate-config` loads the TLS certificate and key *before* it
/// honours `--schema-only`, and reports the result with one exact,
/// Pier-owned prefix. A caller that treats every failure as "this config is
/// unreadable" therefore condemns a perfectly good configuration whenever the
/// certificate, the key, or its ACL is at fault — replacing the operator's
/// settings with defaults and still leaving a host whose service cannot start.
///
/// Matched on that prefix alone, deliberately. An earlier version also matched
/// the bare words `certificate` and `private key` anywhere in the message, and
/// that is unsound: `PierConfig` uses `deny_unknown_fields`, so a stray key
/// named something like `certificate_policy` produces an ordinary schema error
/// carrying the word `certificate`. Keeping a config the Pier genuinely cannot
/// parse is the very failure this classification exists to avoid, so the
/// classification must be narrow enough that only the Pier itself can trigger
/// it.
/// Matched as a true prefix, not a substring, and that distinction matters:
/// Serde quotes offending values back into its errors, so a config that sets a
/// field to the literal text of this prefix would otherwise produce a schema
/// error containing it. A genuine TLS failure always *begins* with it.
#[must_use]
pub(crate) fn is_tls_failure(reason: &str) -> bool {
    // Emitted by the Pier, not by Windows, so it is not localized. Compared
    // case-insensitively only to survive incidental casing changes.
    const TLS_PREFIX: &str = "config validation failed: tls configuration:";
    reason.trim().to_ascii_lowercase().starts_with(TLS_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact line the Pier emits, from `hosts/windows/src/main.rs`.
    #[test]
    fn the_piers_own_tls_wording_is_recognised() {
        assert!(is_tls_failure(
            "config validation failed: TLS configuration: certificate and key do not match"
        ));
        assert!(is_tls_failure(
            "config validation failed: TLS configuration: open \
             C:\\ProgramData\\Arcen\\tls\\host.key: Access is denied. (os error 5)"
        ));
    }

    /// A genuine schema failure must still condemn the config, or the fix for
    /// one bug silently disables the check that exists for the other.
    #[test]
    fn a_schema_failure_is_not_mistaken_for_a_tls_failure() {
        assert!(!is_tls_failure(
            "config validation failed: unknown field `audio.compressed`"
        ));
        assert!(!is_tls_failure(
            "config validation failed: missing field `transport`"
        ));
        assert!(!is_tls_failure("error: expected value at line 3 column 5"));
        assert!(!is_tls_failure("unreadable"));
    }

    /// `PierConfig` denies unknown fields, so a stray key is reported by name.
    /// A key whose *name* contains a TLS word must not buy the file a reprieve:
    /// the Pier cannot parse it, and keeping it leaves a host that will not
    /// start with no explanation of why.
    #[test]
    fn a_schema_failure_naming_a_tls_word_is_still_a_schema_failure() {
        for reason in [
            "config validation failed: unknown field `certificate_policy`",
            "config validation failed: unknown field `private_key_path`",
            "config validation failed: invalid type: string \"x\", expected a \
             certificate list",
            "config validation failed: unknown field `tls_configuration`",
        ] {
            assert!(
                !is_tls_failure(reason),
                "{reason} is a schema failure, not a TLS failure"
            );
        }
    }

    #[test]
    fn matching_ignores_case() {
        assert!(is_tls_failure(
            "CONFIG VALIDATION FAILED: TLS CONFIGURATION: bad"
        ));
    }

    /// Serde quotes an offending value back into its own error text, so a
    /// config that sets a field to the literal prefix produces a genuine schema
    /// error carrying it. Anchoring at the start is what stops that being read
    /// as a TLS fault and preserving a file the Pier cannot parse.
    #[test]
    fn a_schema_error_quoting_the_prefix_is_not_a_tls_failure() {
        assert!(!is_tls_failure(
            "config validation failed: invalid type: string \
             \"config validation failed: TLS configuration: bogus\", expected a boolean"
        ));
    }

    /// Leading whitespace from the captured line must not defeat the anchor.
    #[test]
    fn a_leading_space_does_not_defeat_the_anchor() {
        assert!(is_tls_failure(
            "  config validation failed: TLS configuration: key missing"
        ));
    }

    #[test]
    fn an_empty_reason_is_not_a_tls_failure() {
        assert!(!is_tls_failure(""));
    }
}
