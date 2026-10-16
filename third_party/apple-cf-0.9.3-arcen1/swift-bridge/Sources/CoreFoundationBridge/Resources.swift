import CoreFoundation
import Foundation

@_cdecl("cf_url_get_type_id")
public func cf_url_get_type_id() -> Int {
    Int(CFURLGetTypeID())
}

@_cdecl("cf_url_create_with_string")
public func cf_url_create_with_string(_ value: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let string = acfCFString(from: value), let url = CFURLCreateWithString(nil, string, nil) else {
        return nil
    }
    return Unmanaged.passRetained(url).toOpaque()
}

@_cdecl("cf_url_create_file_path")
public func cf_url_create_file_path(_ path: UnsafePointer<CChar>, _ isDirectory: Bool) -> UnsafeMutableRawPointer? {
    guard let path = acfCFString(from: path) else { return nil }
    guard let url = CFURLCreateWithFileSystemPath(nil, path, .cfurlposixPathStyle, isDirectory) else {
        return nil
    }
    return Unmanaged.passRetained(url).toOpaque()
}

@_cdecl("cf_url_copy_absolute_string")
public func cf_url_copy_absolute_string(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let url = Unmanaged<CFURL>.fromOpaque(value).takeUnretainedValue()
    return Unmanaged.passRetained(CFURLGetString(url)).toOpaque()
}

@_cdecl("cf_url_copy_file_system_path")
public func cf_url_copy_file_system_path(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let url = Unmanaged<CFURL>.fromOpaque(value).takeUnretainedValue()
    return Unmanaged.passRetained(CFURLCopyFileSystemPath(url, .cfurlposixPathStyle)).toOpaque()
}

@_cdecl("cf_url_has_directory_path")
public func cf_url_has_directory_path(_ value: UnsafeMutableRawPointer) -> Bool {
    let url = Unmanaged<CFURL>.fromOpaque(value).takeUnretainedValue()
    return CFURLHasDirectoryPath(url)
}

@_cdecl("cf_bundle_get_type_id")
public func cf_bundle_get_type_id() -> Int {
    Int(CFBundleGetTypeID())
}

@_cdecl("cf_bundle_get_main")
public func cf_bundle_get_main() -> UnsafeMutableRawPointer? {
    guard let bundle = CFBundleGetMainBundle() else { return nil }
    return Unmanaged.passRetained(bundle).toOpaque()
}

@_cdecl("cf_bundle_create")
public func cf_bundle_create(_ url: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let url = Unmanaged<CFURL>.fromOpaque(url).takeUnretainedValue()
    guard let bundle = CFBundleCreate(nil, url) else { return nil }
    return Unmanaged.passRetained(bundle).toOpaque()
}

@_cdecl("cf_bundle_copy_identifier")
public func cf_bundle_copy_identifier(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let bundle = Unmanaged<CFBundle>.fromOpaque(value).takeUnretainedValue()
    guard let identifier = CFBundleGetIdentifier(bundle) else { return nil }
    return Unmanaged.passRetained(identifier).toOpaque()
}

@_cdecl("cf_bundle_copy_bundle_url")
public func cf_bundle_copy_bundle_url(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let bundle = Unmanaged<CFBundle>.fromOpaque(value).takeUnretainedValue()
    guard let url = CFBundleCopyBundleURL(bundle) else { return nil }
    return Unmanaged.passRetained(url).toOpaque()
}

@_cdecl("cf_bundle_copy_resource_url")
public func cf_bundle_copy_resource_url(
    _ value: UnsafeMutableRawPointer,
    _ name: UnsafePointer<CChar>,
    _ ext: UnsafePointer<CChar>?,
    _ subdir: UnsafePointer<CChar>?
) -> UnsafeMutableRawPointer? {
    let bundle = Unmanaged<CFBundle>.fromOpaque(value).takeUnretainedValue()
    guard let name = acfCFString(from: name) else { return nil }
    let extString = acfCFString(from: ext)
    let subdirString = acfCFString(from: subdir)
    guard let url = CFBundleCopyResourceURL(bundle, name, extString, subdirString) else { return nil }
    return Unmanaged.passRetained(url).toOpaque()
}

@_cdecl("cf_locale_get_type_id")
public func cf_locale_get_type_id() -> Int {
    Int(CFLocaleGetTypeID())
}

@_cdecl("cf_locale_copy_current")
public func cf_locale_copy_current() -> UnsafeMutableRawPointer? {
    guard let locale = CFLocaleCopyCurrent() else { return nil }
    return Unmanaged.passRetained(locale).toOpaque()
}

@_cdecl("cf_locale_create")
public func cf_locale_create(_ identifier: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let identifier = acfCFString(from: identifier) else { return nil }
    let localeIdentifier = CFLocaleIdentifier(rawValue: identifier)
    guard let locale = CFLocaleCreate(nil, localeIdentifier) else {
        return nil
    }
    return Unmanaged.passRetained(locale).toOpaque()
}

@_cdecl("cf_locale_copy_identifier")
public func cf_locale_copy_identifier(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let locale = Unmanaged<CFLocale>.fromOpaque(value).takeUnretainedValue()
    guard let identifier = CFLocaleGetIdentifier(locale) else { return nil }
    return Unmanaged.passRetained(identifier.rawValue as CFString).toOpaque()
}

@_cdecl("cf_calendar_get_type_id")
public func cf_calendar_get_type_id() -> Int {
    Int(CFCalendarGetTypeID())
}

@_cdecl("cf_calendar_copy_current")
public func cf_calendar_copy_current() -> UnsafeMutableRawPointer? {
    guard let calendar = CFCalendarCopyCurrent() else { return nil }
    return Unmanaged.passRetained(calendar).toOpaque()
}

@_cdecl("cf_calendar_create")
public func cf_calendar_create(_ identifier: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let identifier = acfCFString(from: identifier) else { return nil }
    let calendarIdentifier = CFCalendarIdentifier(rawValue: identifier)
    guard let calendar = CFCalendarCreateWithIdentifier(nil, calendarIdentifier) else {
        return nil
    }
    return Unmanaged.passRetained(calendar).toOpaque()
}

@_cdecl("cf_calendar_copy_identifier")
public func cf_calendar_copy_identifier(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let calendar = Unmanaged<CFCalendar>.fromOpaque(value).takeUnretainedValue()
    guard let identifier = CFCalendarGetIdentifier(calendar) else { return nil }
    return Unmanaged.passRetained(identifier.rawValue).toOpaque()
}

@_cdecl("cf_calendar_copy_time_zone")
public func cf_calendar_copy_time_zone(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let calendar = Unmanaged<CFCalendar>.fromOpaque(value).takeUnretainedValue()
    guard let timeZone = CFCalendarCopyTimeZone(calendar) else { return nil }
    return Unmanaged.passRetained(timeZone).toOpaque()
}

@_cdecl("cf_calendar_set_time_zone")
public func cf_calendar_set_time_zone(_ value: UnsafeMutableRawPointer, _ timeZone: UnsafeMutableRawPointer) {
    let calendar = Unmanaged<CFCalendar>.fromOpaque(value).takeUnretainedValue()
    let timeZone = Unmanaged<CFTimeZone>.fromOpaque(timeZone).takeUnretainedValue()
    CFCalendarSetTimeZone(calendar, timeZone)
}

@_cdecl("cf_time_zone_get_type_id")
public func cf_time_zone_get_type_id() -> Int {
    Int(CFTimeZoneGetTypeID())
}

@_cdecl("cf_time_zone_copy_current")
public func cf_time_zone_copy_current() -> UnsafeMutableRawPointer? {
    guard let timeZone = CFTimeZoneCopySystem() else { return nil }
    return Unmanaged.passRetained(timeZone).toOpaque()
}

@_cdecl("cf_time_zone_create")
public func cf_time_zone_create(_ name: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let name = acfCFString(from: name), let timeZone = CFTimeZoneCreateWithName(nil, name, true) else {
        return nil
    }
    return Unmanaged.passRetained(timeZone).toOpaque()
}

@_cdecl("cf_time_zone_copy_name")
public func cf_time_zone_copy_name(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let timeZone = Unmanaged<CFTimeZone>.fromOpaque(value).takeUnretainedValue()
    return Unmanaged.passRetained(CFTimeZoneGetName(timeZone)).toOpaque()
}

@_cdecl("cf_time_zone_get_seconds_from_gmt")
public func cf_time_zone_get_seconds_from_gmt(_ value: UnsafeMutableRawPointer, _ date: UnsafeMutableRawPointer) -> Int32 {
    let timeZone = Unmanaged<CFTimeZone>.fromOpaque(value).takeUnretainedValue()
    let date = Unmanaged<CFDate>.fromOpaque(date).takeUnretainedValue()
    return Int32(CFTimeZoneGetSecondsFromGMT(timeZone, CFDateGetAbsoluteTime(date)))
}

@_cdecl("cf_character_set_get_type_id")
public func cf_character_set_get_type_id() -> Int {
    Int(CFCharacterSetGetTypeID())
}

@_cdecl("cf_character_set_create_with_characters_in_string")
public func cf_character_set_create_with_characters_in_string(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let string = Unmanaged<CFString>.fromOpaque(value).takeUnretainedValue()
    guard let characterSet = CFCharacterSetCreateWithCharactersInString(nil, string) else { return nil }
    return Unmanaged.passRetained(characterSet).toOpaque()
}

@_cdecl("cf_character_set_create_inverted_set")
public func cf_character_set_create_inverted_set(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let characterSet = Unmanaged<CFCharacterSet>.fromOpaque(value).takeUnretainedValue()
    guard let inverted = CFCharacterSetCreateInvertedSet(nil, characterSet) else { return nil }
    return Unmanaged.passRetained(inverted).toOpaque()
}

@_cdecl("cf_character_set_is_character_member")
public func cf_character_set_is_character_member(_ value: UnsafeMutableRawPointer, _ scalar: UInt32) -> Bool {
    let characterSet = Unmanaged<CFCharacterSet>.fromOpaque(value).takeUnretainedValue()
    guard let scalar = UniChar(exactly: scalar) else { return false }
    return CFCharacterSetIsCharacterMember(characterSet, scalar)
}

@_cdecl("cf_number_formatter_get_type_id")
public func cf_number_formatter_get_type_id() -> Int {
    Int(CFNumberFormatterGetTypeID())
}

@_cdecl("cf_number_formatter_create")
public func cf_number_formatter_create(_ locale: UnsafeMutableRawPointer?, _ style: Int32) -> UnsafeMutableRawPointer? {
    let locale = locale.map { Unmanaged<CFLocale>.fromOpaque($0).takeUnretainedValue() }
    guard let formatter = CFNumberFormatterCreate(nil, locale, CFNumberFormatterStyle(rawValue: Int(style))!) else {
        return nil
    }
    return Unmanaged.passRetained(formatter).toOpaque()
}

@_cdecl("cf_number_formatter_create_string_with_number")
public func cf_number_formatter_create_string_with_number(
    _ value: UnsafeMutableRawPointer,
    _ number: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let formatter = Unmanaged<CFNumberFormatter>.fromOpaque(value).takeUnretainedValue()
    let number = Unmanaged<CFNumber>.fromOpaque(number).takeUnretainedValue()
    guard let string = CFNumberFormatterCreateStringWithNumber(nil, formatter, number) else { return nil }
    return Unmanaged.passRetained(string).toOpaque()
}

@_cdecl("cf_number_formatter_create_number_from_string")
public func cf_number_formatter_create_number_from_string(
    _ value: UnsafeMutableRawPointer,
    _ string: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let formatter = Unmanaged<CFNumberFormatter>.fromOpaque(value).takeUnretainedValue()
    let string = Unmanaged<CFString>.fromOpaque(string).takeUnretainedValue()
    guard let number = CFNumberFormatterCreateNumberFromString(nil, formatter, string, nil, 0) else {
        return nil
    }
    return Unmanaged.passRetained(number).toOpaque()
}

@_cdecl("cf_date_formatter_get_type_id")
public func cf_date_formatter_get_type_id() -> Int {
    Int(CFDateFormatterGetTypeID())
}

@_cdecl("cf_date_formatter_create")
public func cf_date_formatter_create(
    _ locale: UnsafeMutableRawPointer?,
    _ dateStyle: Int32,
    _ timeStyle: Int32
) -> UnsafeMutableRawPointer? {
    let locale = locale.map { Unmanaged<CFLocale>.fromOpaque($0).takeUnretainedValue() }
    guard let formatter = CFDateFormatterCreate(
        nil,
        locale,
        CFDateFormatterStyle(rawValue: Int(dateStyle))!,
        CFDateFormatterStyle(rawValue: Int(timeStyle))!
    ) else {
        return nil
    }
    return Unmanaged.passRetained(formatter).toOpaque()
}

@_cdecl("cf_date_formatter_create_string_with_date")
public func cf_date_formatter_create_string_with_date(
    _ value: UnsafeMutableRawPointer,
    _ date: UnsafeMutableRawPointer
) -> UnsafeMutableRawPointer? {
    let formatter = Unmanaged<CFDateFormatter>.fromOpaque(value).takeUnretainedValue()
    let date = Unmanaged<CFDate>.fromOpaque(date).takeUnretainedValue()
    guard let string = CFDateFormatterCreateStringWithDate(nil, formatter, date) else { return nil }
    return Unmanaged.passRetained(string).toOpaque()
}

@_cdecl("cf_file_security_get_type_id")
public func cf_file_security_get_type_id() -> Int {
    Int(CFFileSecurityGetTypeID())
}

@_cdecl("cf_file_security_create")
public func cf_file_security_create() -> UnsafeMutableRawPointer? {
    guard let fileSecurity = CFFileSecurityCreate(nil) else { return nil }
    return Unmanaged.passRetained(fileSecurity).toOpaque()
}

@_cdecl("cf_file_security_copy_owner_uuid")
public func cf_file_security_copy_owner_uuid(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let fileSecurity = Unmanaged<CFFileSecurity>.fromOpaque(value).takeUnretainedValue()
    var uuid: Unmanaged<CFUUID>?
    guard CFFileSecurityCopyOwnerUUID(fileSecurity, &uuid), let uuid else { return nil }
    return uuid.toOpaque()
}

@_cdecl("cf_file_security_set_owner_uuid")
public func cf_file_security_set_owner_uuid(_ value: UnsafeMutableRawPointer, _ uuid: UnsafeMutableRawPointer) -> Bool {
    let fileSecurity = Unmanaged<CFFileSecurity>.fromOpaque(value).takeUnretainedValue()
    let uuid = Unmanaged<CFUUID>.fromOpaque(uuid).takeUnretainedValue()
    return CFFileSecuritySetOwnerUUID(fileSecurity, uuid)
}

@_cdecl("cf_file_security_get_mode")
public func cf_file_security_get_mode(_ value: UnsafeMutableRawPointer, _ outMode: UnsafeMutablePointer<UInt32>) -> Bool {
    let fileSecurity = Unmanaged<CFFileSecurity>.fromOpaque(value).takeUnretainedValue()
    var mode: mode_t = 0
    let ok = CFFileSecurityGetMode(fileSecurity, &mode)
    outMode.pointee = UInt32(mode)
    return ok
}

@_cdecl("cf_file_security_set_mode")
public func cf_file_security_set_mode(_ value: UnsafeMutableRawPointer, _ mode: UInt32) -> Bool {
    let fileSecurity = Unmanaged<CFFileSecurity>.fromOpaque(value).takeUnretainedValue()
    return CFFileSecuritySetMode(fileSecurity, mode_t(mode))
}

@_cdecl("cf_preferences_set_app_value")
public func cf_preferences_set_app_value(
    _ key: UnsafeMutableRawPointer,
    _ value: UnsafeMutableRawPointer?,
    _ appID: UnsafeMutableRawPointer
) {
    let key = Unmanaged<CFString>.fromOpaque(key).takeUnretainedValue()
    let appID = Unmanaged<CFString>.fromOpaque(appID).takeUnretainedValue()
    let cfValue = value.map { unsafeBitCast(acfBorrowedAnyObject($0), to: CFTypeRef.self) }
    CFPreferencesSetAppValue(key, cfValue, appID)
}

@_cdecl("cf_preferences_copy_app_value")
public func cf_preferences_copy_app_value(_ key: UnsafeMutableRawPointer, _ appID: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let key = Unmanaged<CFString>.fromOpaque(key).takeUnretainedValue()
    let appID = Unmanaged<CFString>.fromOpaque(appID).takeUnretainedValue()
    guard let value = CFPreferencesCopyAppValue(key, appID) else { return nil }
    return Unmanaged.passRetained(value).toOpaque()
}

@_cdecl("cf_preferences_app_synchronize")
public func cf_preferences_app_synchronize(_ appID: UnsafeMutableRawPointer) -> Bool {
    let appID = Unmanaged<CFString>.fromOpaque(appID).takeUnretainedValue()
    return CFPreferencesAppSynchronize(appID)
}

@_cdecl("cf_xml_create_string_by_escaping_entities")
public func cf_xml_create_string_by_escaping_entities(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let string = Unmanaged<CFString>.fromOpaque(value).takeUnretainedValue()
    guard let escaped = CFXMLCreateStringByEscapingEntities(nil, string, nil) else { return nil }
    return Unmanaged.passRetained(escaped).toOpaque()
}

@_cdecl("cf_xml_create_string_by_unescaping_entities")
public func cf_xml_create_string_by_unescaping_entities(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let string = Unmanaged<CFString>.fromOpaque(value).takeUnretainedValue()
    guard let unescaped = CFXMLCreateStringByUnescapingEntities(nil, string, nil) else { return nil }
    return Unmanaged.passRetained(unescaped).toOpaque()
}
