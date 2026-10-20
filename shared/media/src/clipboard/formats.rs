//! Host clipboard formats: which native names carry Arcen's two wire kinds.
//!
//! X11 selection targets and Wayland/Mutter MIME types name the same things
//! (`UTF8_STRING`, `text/plain;charset=utf-8`, `image/png`, ...). This table is
//! the only place a Pier decides which name it reads, which names it offers,
//! and how a name's bytes become UTF-8 text or a PNG. If each host kept its own
//! list, they would drift, and a copy that works on one desktop would silently
//! fail on another.

use std::borrow::Cow;
use std::fmt::{Display, Formatter};

use super::{
    ClipboardFlow, ClipboardKind, ClipboardPolicy, ClipboardPolicyError, ImageLimits, validate_png,
};

/// How a host clipboard name encodes its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFormat {
    /// UTF-8 text (`UTF8_STRING`, `text/plain;charset=utf-8`).
    Utf8Text,
    /// Text without a declared charset (`text/plain`, `TEXT`). Accepted only
    /// when it is valid UTF-8.
    PlainText,
    /// ISO-8859-1 text (`STRING`).
    Latin1Text,
    /// PNG image bytes (`image/png`).
    Png,
}

impl HostFormat {
    /// The wire kind this format carries.
    #[must_use]
    pub const fn kind(self) -> ClipboardKind {
        match self {
            Self::Utf8Text | Self::PlainText | Self::Latin1Text => ClipboardKind::TextUtf8,
            Self::Png => ClipboardKind::ImagePng,
        }
    }
}

/// X11 `UTF8_STRING`.
pub const UTF8_STRING: &str = "UTF8_STRING";
/// UTF-8 text MIME type.
pub const TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
/// Text MIME type without a charset.
pub const TEXT_PLAIN: &str = "text/plain";
/// X11 `TEXT`.
pub const X11_TEXT: &str = "TEXT";
/// X11 `STRING` (ISO-8859-1).
pub const X11_STRING: &str = "STRING";
/// PNG image MIME type.
pub const IMAGE_PNG: &str = "image/png";

/// Text names in the order a host reads them: an unambiguous UTF-8 name
/// first, latin-1 last.
const TEXT_PREFERENCE: [(&str, HostFormat); 5] = [
    (UTF8_STRING, HostFormat::Utf8Text),
    (TEXT_PLAIN_UTF8, HostFormat::Utf8Text),
    (TEXT_PLAIN, HostFormat::PlainText),
    (X11_TEXT, HostFormat::PlainText),
    (X11_STRING, HostFormat::Latin1Text),
];

/// Returns the format a host clipboard name carries, or `None` for a name
/// Arcen does not carry. MIME types compare case-insensitively and ignore
/// spaces around parameters (`text/plain; charset=UTF-8`).
#[must_use]
pub fn host_format(name: &str) -> Option<HostFormat> {
    if name == UTF8_STRING || name == X11_TEXT || name == X11_STRING {
        return TEXT_PREFERENCE
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, format)| *format);
    }
    match canonical(name).as_str() {
        IMAGE_PNG => Some(HostFormat::Png),
        TEXT_PLAIN_UTF8 => Some(HostFormat::Utf8Text),
        TEXT_PLAIN => Some(HostFormat::PlainText),
        _ => None,
    }
}

/// Chooses which of the `offered` names a host reads, given which kinds may
/// leave the host. Text wins over an image (a browser offers both for a
/// copied picture with a caption, and the text is what the person
/// selected); within text the [`TEXT_PREFERENCE`] order applies, whatever
/// order the owner listed its names in.
///
/// Returns the offered name exactly as given, so the host requests the string
/// the owner advertised.
pub fn choose_host_format<'a, I>(
    allows: impl Fn(ClipboardKind) -> bool,
    offered: I,
) -> Option<(&'a str, HostFormat)>
where
    I: IntoIterator<Item = &'a str>,
{
    let offered: Vec<(&'a str, HostFormat)> = offered
        .into_iter()
        .filter_map(|name| host_format(name).map(|format| (name, format)))
        .collect();
    if allows(ClipboardKind::TextUtf8) {
        for (preferred, _) in TEXT_PREFERENCE {
            if let Some(found) = offered.iter().find(|(name, format)| {
                format.kind() == ClipboardKind::TextUtf8 && same_name(name, preferred)
            }) {
                return Some(*found);
            }
        }
    }
    if allows(ClipboardKind::ImagePng) {
        return offered
            .into_iter()
            .find(|(_, format)| *format == HostFormat::Png);
    }
    None
}

fn same_name(offered: &str, preferred: &str) -> bool {
    if preferred.contains('/') {
        canonical(offered) == canonical(preferred)
    } else {
        offered == preferred
    }
}

fn canonical(name: &str) -> String {
    name.chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
        .replace("charset=\"utf-8\"", "charset=utf-8")
}

/// The names a host offers for an item it holds for the client. `STRING` is
/// offered only when the text is representable in ISO-8859-1.
#[must_use]
pub fn offered_names(kind: ClipboardKind, bytes: &[u8]) -> Vec<&'static str> {
    match kind {
        ClipboardKind::TextUtf8 => {
            let mut names = vec![TEXT_PLAIN_UTF8, UTF8_STRING, TEXT_PLAIN, X11_TEXT];
            if latin1_from_utf8(bytes).is_some() {
                names.push(X11_STRING);
            }
            names
        }
        ClipboardKind::ImagePng => vec![IMAGE_PNG],
    }
}

/// The bytes to hand a requestor that asked for `name`, or `None` when this
/// item cannot be served as that name.
#[must_use]
pub fn render_for_host<'a>(
    name: &str,
    kind: ClipboardKind,
    bytes: &'a [u8],
) -> Option<Cow<'a, [u8]>> {
    let format = host_format(name)?;
    if format.kind() != kind {
        return None;
    }
    match format {
        HostFormat::Latin1Text => latin1_from_utf8(bytes).map(Cow::Owned),
        HostFormat::Utf8Text | HostFormat::PlainText | HostFormat::Png => {
            Some(Cow::Borrowed(bytes))
        }
    }
}

/// Converts UTF-8 to ISO-8859-1, or `None` when a character does not fit.
#[must_use]
pub fn latin1_from_utf8(bytes: &[u8]) -> Option<Vec<u8>> {
    std::str::from_utf8(bytes)
        .ok()?
        .chars()
        .map(|character| u8::try_from(u32::from(character)).ok())
        .collect()
}

/// Converts ISO-8859-1 to UTF-8. Every byte is a character, so this cannot
/// fail.
#[must_use]
pub fn utf8_from_latin1(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|byte| char::from(*byte))
        .collect::<String>()
        .into_bytes()
}

/// Why a payload does not cross a clipboard boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPayloadError {
    /// The host policy or negotiation refuses it (direction, content or size).
    Refused(ClipboardPolicyError),
    /// The bytes are not what their label says (not UTF-8, not a PNG within
    /// the image limits).
    Malformed,
    /// The payload is empty.
    Empty,
}

impl ClipboardPayloadError {
    /// Stable token for logs and counters.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Refused(ClipboardPolicyError::TooLarge { .. }) => "too_large",
            Self::Refused(_) => "policy",
            Self::Malformed => "malformed",
            Self::Empty => "empty",
        }
    }
}

impl Display for ClipboardPayloadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => Display::fmt(error, formatter),
            Self::Malformed => formatter.write_str("clipboard payload is not what its label says"),
            Self::Empty => formatter.write_str("clipboard payload is empty"),
        }
    }
}

impl std::error::Error for ClipboardPayloadError {}

/// Checks a complete payload before it crosses a boundary: the policy for
/// this flow, kind and size, then the content (UTF-8 text, or a PNG within
/// [`ImageLimits`] bounded by the policy maximum).
///
/// # Errors
///
/// [`ClipboardPayloadError`] naming why the payload does not cross.
pub fn validate_payload(
    policy: ClipboardPolicy,
    flow: ClipboardFlow,
    kind: ClipboardKind,
    bytes: &[u8],
) -> Result<(), ClipboardPayloadError> {
    if bytes.is_empty() {
        return Err(ClipboardPayloadError::Empty);
    }
    policy
        .check_size(flow, kind, bytes.len())
        .map_err(ClipboardPayloadError::Refused)?;
    let valid = match kind {
        ClipboardKind::TextUtf8 => std::str::from_utf8(bytes).is_ok(),
        ClipboardKind::ImagePng => validate_png(
            bytes,
            ImageLimits {
                max_encoded_bytes: policy.max_bytes,
                ..ImageLimits::default()
            },
        )
        .is_ok(),
    };
    if valid {
        Ok(())
    } else {
        Err(ClipboardPayloadError::Malformed)
    }
}

/// Turns what a host read as `format` into a wire payload, checked against
/// the host-to-client policy.
///
/// # Errors
///
/// [`ClipboardPayloadError`] naming why the payload does not cross.
pub fn decode_host_payload(
    policy: ClipboardPolicy,
    format: HostFormat,
    bytes: Vec<u8>,
) -> Result<(ClipboardKind, Vec<u8>), ClipboardPayloadError> {
    let bytes = match format {
        HostFormat::Latin1Text => {
            let mut original = bytes;
            let converted = utf8_from_latin1(&original);
            zeroize::Zeroize::zeroize(&mut original);
            converted
        }
        HostFormat::Utf8Text | HostFormat::PlainText | HostFormat::Png => bytes,
    };
    let kind = format.kind();
    match validate_payload(policy, ClipboardFlow::HostToClient, kind, &bytes) {
        Ok(()) => Ok((kind, bytes)),
        Err(error) => {
            let mut bytes = bytes;
            zeroize::Zeroize::zeroize(&mut bytes);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{ClipboardContent, ClipboardDirection};

    fn everything(kind: ClipboardKind) -> bool {
        let _ = kind;
        true
    }

    #[test]
    fn x11_text_order_is_unchanged_whatever_order_the_owner_lists() {
        // The order the dedicated-Xorg child used before this table existed.
        let offered = [
            X11_STRING,
            X11_TEXT,
            TEXT_PLAIN_UTF8,
            UTF8_STRING,
            IMAGE_PNG,
        ];
        assert_eq!(
            choose_host_format(everything, offered),
            Some((UTF8_STRING, HostFormat::Utf8Text))
        );
        let offered = [X11_STRING, X11_TEXT, TEXT_PLAIN_UTF8];
        assert_eq!(
            choose_host_format(everything, offered),
            Some((TEXT_PLAIN_UTF8, HostFormat::Utf8Text))
        );
        let offered = [X11_STRING, X11_TEXT];
        assert_eq!(
            choose_host_format(everything, offered),
            Some((X11_TEXT, HostFormat::PlainText))
        );
        assert_eq!(
            choose_host_format(everything, [X11_STRING]),
            Some((X11_STRING, HostFormat::Latin1Text))
        );
    }

    #[test]
    fn text_wins_over_an_image_unless_text_may_not_leave() {
        let offered = [IMAGE_PNG, TEXT_PLAIN];
        assert_eq!(
            choose_host_format(everything, offered),
            Some((TEXT_PLAIN, HostFormat::PlainText))
        );
        assert_eq!(
            choose_host_format(|kind| kind == ClipboardKind::ImagePng, offered),
            Some((IMAGE_PNG, HostFormat::Png))
        );
        assert_eq!(
            choose_host_format(|kind| kind == ClipboardKind::TextUtf8, [IMAGE_PNG]),
            None
        );
    }

    #[test]
    fn mime_parameters_are_matched_loosely_and_returned_exactly() {
        let offered = ["text/html", "text/plain; charset=UTF-8", "image/jpeg"];
        assert_eq!(
            choose_host_format(everything, offered),
            Some(("text/plain; charset=UTF-8", HostFormat::Utf8Text))
        );
        assert_eq!(host_format("IMAGE/PNG"), Some(HostFormat::Png));
        assert_eq!(host_format("text/html"), None);
        assert_eq!(host_format("utf8_string"), None);
        assert_eq!(
            choose_host_format(everything, ["text/html", "image/jpeg"]),
            None
        );
    }

    #[test]
    fn offered_names_cover_both_desktops_and_string_only_when_it_fits() {
        assert_eq!(
            offered_names(ClipboardKind::TextUtf8, "café".as_bytes()),
            vec![
                TEXT_PLAIN_UTF8,
                UTF8_STRING,
                TEXT_PLAIN,
                X11_TEXT,
                X11_STRING
            ]
        );
        assert!(!offered_names(ClipboardKind::TextUtf8, "Ω".as_bytes()).contains(&X11_STRING));
        assert_eq!(
            offered_names(ClipboardKind::ImagePng, b"x"),
            vec![IMAGE_PNG]
        );
    }

    #[test]
    fn rendering_converts_latin1_and_refuses_the_wrong_kind() {
        let text = "café".as_bytes();
        assert_eq!(
            render_for_host(X11_STRING, ClipboardKind::TextUtf8, text).as_deref(),
            Some(&b"caf\xe9"[..])
        );
        assert_eq!(
            render_for_host(TEXT_PLAIN_UTF8, ClipboardKind::TextUtf8, text).as_deref(),
            Some(text)
        );
        assert!(render_for_host(IMAGE_PNG, ClipboardKind::TextUtf8, text).is_none());
        assert!(render_for_host("text/html", ClipboardKind::TextUtf8, text).is_none());
        assert!(render_for_host(X11_STRING, ClipboardKind::TextUtf8, "Ω".as_bytes()).is_none());
    }

    #[test]
    fn decoding_converts_latin1_and_refuses_rather_than_truncates() {
        let policy =
            ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::All, 8).unwrap();
        assert_eq!(
            decode_host_payload(policy, HostFormat::Latin1Text, b"caf\xe9".to_vec()),
            Ok((ClipboardKind::TextUtf8, "café".as_bytes().to_vec()))
        );
        assert_eq!(
            decode_host_payload(policy, HostFormat::Utf8Text, b"123456789".to_vec()).unwrap_err(),
            ClipboardPayloadError::Refused(ClipboardPolicyError::TooLarge {
                bytes: 9,
                maximum: 8
            })
        );
        assert_eq!(
            decode_host_payload(policy, HostFormat::PlainText, vec![0xff, 0xfe]).unwrap_err(),
            ClipboardPayloadError::Malformed
        );
        assert_eq!(
            decode_host_payload(policy, HostFormat::Png, b"nope".to_vec()).unwrap_err(),
            ClipboardPayloadError::Malformed
        );
        assert_eq!(
            decode_host_payload(policy, HostFormat::Utf8Text, Vec::new()).unwrap_err(),
            ClipboardPayloadError::Empty
        );
    }

    #[test]
    fn the_policy_direction_and_content_are_checked_before_content() {
        let host_only = ClipboardPolicy::new(
            ClipboardDirection::ClientToHost,
            ClipboardContent::Text,
            1024,
        )
        .unwrap();
        assert_eq!(
            validate_payload(
                host_only,
                ClipboardFlow::HostToClient,
                ClipboardKind::TextUtf8,
                b"a"
            )
            .unwrap_err()
            .token(),
            "policy"
        );
        assert_eq!(
            validate_payload(
                host_only,
                ClipboardFlow::ClientToHost,
                ClipboardKind::ImagePng,
                b"a"
            )
            .unwrap_err()
            .token(),
            "policy"
        );
        assert!(
            validate_payload(
                host_only,
                ClipboardFlow::ClientToHost,
                ClipboardKind::TextUtf8,
                b"a"
            )
            .is_ok()
        );
    }
}
