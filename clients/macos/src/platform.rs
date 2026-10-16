//! This Deck's platform profile, resolved once per process.
//!
//! The Deck gathers the facts -- the architecture it was compiled for, the
//! running macOS version, and whether Rosetta is translating it -- and hands
//! them to [`arcen_media::deck_platform::DeckPlatform`], which owns every
//! decision that differs between Apple silicon and Intel. Nothing here
//! decides anything.

use std::sync::OnceLock;

use arcen_media::deck_platform::{DeckArchitecture, DeckPlatform, MacOsVersion};

/// The architecture this binary was compiled for. The Intel Deck is an
/// `x86_64-apple-darwin` build; every other Deck build is Apple silicon.
#[cfg(target_arch = "x86_64")]
pub const COMPILED_ARCHITECTURE: DeckArchitecture = DeckArchitecture::Intel;
#[cfg(not(target_arch = "x86_64"))]
pub const COMPILED_ARCHITECTURE: DeckArchitecture = DeckArchitecture::AppleSilicon;

static PLATFORM: OnceLock<DeckPlatform> = OnceLock::new();

/// The running Deck's platform profile. Facts are gathered on first use and
/// cannot change for the life of the process.
#[must_use]
pub fn current() -> DeckPlatform {
    *PLATFORM.get_or_init(|| {
        DeckPlatform::new(COMPILED_ARCHITECTURE, running_macos(), rosetta_translated())
    })
}

#[cfg(target_os = "macos")]
fn running_macos() -> MacOsVersion {
    let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
    MacOsVersion::from_reported(
        version.majorVersion,
        version.minorVersion,
        version.patchVersion,
    )
}

#[cfg(not(target_os = "macos"))]
fn running_macos() -> MacOsVersion {
    MacOsVersion::new(0, 0, 0)
}

/// Whether Rosetta is running this process: an Intel Deck on an Apple-silicon
/// Mac. `sysctl.proc_translated` is 1 under Rosetta, 0 for a native process
/// on Apple silicon, and absent on an Intel Mac.
#[cfg(target_os = "macos")]
fn rosetta_translated() -> bool {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is a NUL-terminated C string literal; `value` and
    // `size` are live locals and `size` is exactly `value`'s size, so the
    // kernel cannot write past it; a null `newp` with length 0 sets nothing.
    let status = unsafe {
        libc::sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            std::ptr::from_mut(&mut value).cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    status == 0 && value == 1
}

#[cfg(not(target_os = "macos"))]
fn rosetta_translated() -> bool {
    false
}

/// Logs the resolved profile once at start-up, so every log and support
/// report says which Deck it came from.
pub fn log_platform() {
    let platform = current();
    tracing::info!(
        target: crate::logging::target::SESSION,
        event = "deck_platform",
        deck_platform = platform.label(),
        arch = platform.architecture().token(),
        macos = %platform.macos(),
        rosetta = platform.translated(),
        av1_decode_possible = platform.av1_decode_possible(),
        software_decode_expected = platform.software_decode_expected(),
        native_usb_helper_supported = platform.native_usb_helper_supported(),
        "Deck platform profile",
    );
    if platform.translated() {
        tracing::warn!(
            target: crate::logging::target::SESSION,
            event = "deck_platform_translated",
            deck_platform = platform.label(),
            "Arcen Deck Intel is running under Rosetta on an Apple-silicon Mac; install \
             Arcen Deck, the Apple-silicon app, which runs natively and can decode AV1",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_architecture_matches_the_build_target() {
        assert_eq!(
            DeckArchitecture::from_target_arch(std::env::consts::ARCH),
            Some(COMPILED_ARCHITECTURE)
        );
    }

    #[test]
    fn the_profile_is_resolved_once() {
        assert_eq!(current(), current());
        assert_eq!(current().architecture(), COMPILED_ARCHITECTURE);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_running_macos_is_at_least_the_minimum() {
        assert!(current().macos().major >= 12, "{}", current());
    }
}
