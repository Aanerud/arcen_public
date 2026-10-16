use apple_cf::cf::{CFData, CFDate, CFError as CoreFoundationError, CFNumber, CFString, CFUUID};

#[test]
fn cf_primitives_round_trip() {
    let string = CFString::new("apple-cf");
    let number = CFNumber::from_f64(3.5);
    let bytes = CFData::from_bytes([1_u8, 2, 3]);
    let now = CFDate::now();
    let uuid = CFUUID::new();
    let error = CoreFoundationError::new(
        &CFString::new("com.doomfish.apple-cf.tests"),
        5,
        Some("failure"),
    );

    assert_eq!(string.to_string(), "apple-cf");
    assert_eq!(number.to_f64(), Some(3.5));
    assert!(number.is_float_type());
    assert_eq!(bytes.to_vec(), vec![1, 2, 3]);
    assert!(now.to_system_time().is_some());
    assert_eq!(uuid.bytes().len(), 16);
    assert_eq!(error.code(), 5);
    assert_eq!(error.domain().to_string(), "com.doomfish.apple-cf.tests");
}
