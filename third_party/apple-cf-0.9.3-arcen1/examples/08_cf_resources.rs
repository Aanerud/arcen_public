use apple_cf::cf::{
    CFBundle, CFCalendar, CFCharacterSet, CFDate, CFDateFormatter, CFDateFormatterStyle,
    CFFileSecurity, CFLocale, CFNumber, CFNumberFormatter, CFNumberFormatterStyle, CFPreferences,
    CFString, CFTimeZone, CFURL, CFUUID, CFXML,
};

fn main() {
    let bundle_url =
        CFURL::from_file_system_path("/System/Library/Frameworks/CoreFoundation.framework", true);
    let bundle = CFBundle::from_url(&bundle_url).expect("bundle");
    assert!(bundle.bundle_url().has_directory_path());

    let locale = CFLocale::new("en_US");
    let calendar = CFCalendar::new("gregorian");
    let time_zone = CFTimeZone::new("GMT");
    calendar.set_time_zone(&time_zone);
    assert_eq!(calendar.time_zone().name().to_string(), "GMT");
    assert_eq!(locale.identifier().to_string(), "en_US");

    let charset = CFCharacterSet::from_characters_in_string(&CFString::new("abc"));
    assert!(charset.contains('b'));

    let number_formatter = CFNumberFormatter::new(Some(&locale), CFNumberFormatterStyle::Decimal);
    let rendered = number_formatter.format_number(&CFNumber::from_i64(1234));
    assert!(!rendered.is_empty());

    let date_formatter = CFDateFormatter::new(
        Some(&locale),
        CFDateFormatterStyle::Short,
        CFDateFormatterStyle::NoStyle,
    );
    assert!(!date_formatter.format_date(&CFDate::now()).is_empty());

    let app_id = CFString::new("com.doomfish.apple-cf.example");
    let pref_key = CFString::new("example-key");
    CFPreferences::set_app_value(&pref_key, Some(&CFString::new("value")), &app_id);
    assert!(CFPreferences::synchronize(&app_id));
    assert!(CFPreferences::app_value(&pref_key, &app_id).is_some());
    CFPreferences::set_app_value(&pref_key, None, &app_id);
    let _ = CFPreferences::synchronize(&app_id);

    let file_security = CFFileSecurity::new();
    let owner = CFUUID::new();
    assert!(file_security.set_owner_uuid(&owner));
    assert!(file_security.owner_uuid().is_some());

    let escaped = CFXML::escape_entities(&CFString::new("<tag>value</tag>"));
    assert_eq!(
        CFXML::unescape_entities(&escaped).to_string(),
        "<tag>value</tag>"
    );
}
