//! Client time-zone capture.

/// Returns the current system IANA time-zone identifier when available.
#[cfg(target_os = "macos")]
#[must_use]
pub fn current_identifier() -> Option<String> {
    use objc2_foundation::NSTimeZone;

    let identifier = NSTimeZone::localTimeZone().name().to_string();
    (!identifier.is_empty()).then_some(identifier)
}

/// Time-zone capture is unavailable on portable non-macOS builds.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub const fn current_identifier() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn current_macos_identifier_is_nonempty_and_valid() {
        use arcen_session::restore_lease::IanaTimeZone;

        let identifier = current_identifier().expect("macOS reports a local time zone");
        assert!(!identifier.is_empty());
        IanaTimeZone::new(identifier).expect("NSTimeZone returns an IANA identifier");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn portable_build_has_no_timezone_capture() {
        assert_eq!(current_identifier(), None);
    }
}
