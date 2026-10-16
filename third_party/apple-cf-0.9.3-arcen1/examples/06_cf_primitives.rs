use apple_cf::cf::{CFData, CFDate, CFError as CoreFoundationError, CFNumber, CFString, CFUUID};

fn main() {
    let string = CFString::new("apple-cf");
    let number = CFNumber::from_i64(42);
    let bytes = CFData::from_bytes([0xAA, 0xBB, 0xCC]);
    let now = CFDate::now();
    let uuid = CFUUID::new();
    let error = CoreFoundationError::new(
        &CFString::new("com.doomfish.apple-cf.example"),
        7,
        Some("demo error"),
    );

    assert_eq!(string.to_string(), "apple-cf");
    assert_eq!(number.to_i64(), Some(42));
    assert_eq!(bytes.to_vec(), vec![0xAA, 0xBB, 0xCC]);
    assert!(now.to_system_time().is_some());
    assert_eq!(uuid.bytes().len(), 16);
    assert_eq!(error.code(), 7);
}
