import CoreFoundation
import Foundation

@_cdecl("cf_string_get_type_id")
public func cf_string_get_type_id() -> Int {
    Int(CFStringGetTypeID())
}

@_cdecl("cf_string_create_with_cstring")
public func cf_string_create_with_cstring(_ value: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let string = acfCFString(from: value) else { return nil }
    return Unmanaged.passRetained(string).toOpaque()
}

@_cdecl("cf_string_copy_cstring")
public func cf_string_copy_cstring(_ value: UnsafeMutableRawPointer) -> UnsafeMutablePointer<CChar>? {
    let string = Unmanaged<CFString>.fromOpaque(value).takeUnretainedValue()
    return acfCopyCString(from: string)
}

@_cdecl("cf_string_get_length")
public func cf_string_get_length(_ value: UnsafeMutableRawPointer) -> Int {
    let string = Unmanaged<CFString>.fromOpaque(value).takeUnretainedValue()
    return CFStringGetLength(string)
}

@_cdecl("cf_number_get_type_id")
public func cf_number_get_type_id() -> Int {
    Int(CFNumberGetTypeID())
}

@_cdecl("cf_number_create_i64")
public func cf_number_create_i64(_ value: Int64) -> UnsafeMutableRawPointer? {
    var value = value
    guard let number = CFNumberCreate(nil, .sInt64Type, &value) else { return nil }
    return Unmanaged.passRetained(number).toOpaque()
}

@_cdecl("cf_number_create_u64")
public func cf_number_create_u64(_ value: UInt64) -> UnsafeMutableRawPointer? {
    var value = value
    guard let number = CFNumberCreate(nil, .longLongType, &value) else { return nil }
    return Unmanaged.passRetained(number).toOpaque()
}

@_cdecl("cf_number_create_f64")
public func cf_number_create_f64(_ value: Double) -> UnsafeMutableRawPointer? {
    var value = value
    guard let number = CFNumberCreate(nil, .doubleType, &value) else { return nil }
    return Unmanaged.passRetained(number).toOpaque()
}

@_cdecl("cf_number_get_i64")
public func cf_number_get_i64(_ value: UnsafeMutableRawPointer, _ out: UnsafeMutablePointer<Int64>) -> Bool {
    let number = Unmanaged<CFNumber>.fromOpaque(value).takeUnretainedValue()
    return CFNumberGetValue(number, .sInt64Type, out)
}

@_cdecl("cf_number_get_u64")
public func cf_number_get_u64(_ value: UnsafeMutableRawPointer, _ out: UnsafeMutablePointer<UInt64>) -> Bool {
    let number = Unmanaged<CFNumber>.fromOpaque(value).takeUnretainedValue()
    return CFNumberGetValue(number, .longLongType, out)
}

@_cdecl("cf_number_get_f64")
public func cf_number_get_f64(_ value: UnsafeMutableRawPointer, _ out: UnsafeMutablePointer<Double>) -> Bool {
    let number = Unmanaged<CFNumber>.fromOpaque(value).takeUnretainedValue()
    return CFNumberGetValue(number, .doubleType, out)
}

@_cdecl("cf_number_is_float_type")
public func cf_number_is_float_type(_ value: UnsafeMutableRawPointer) -> Bool {
    let number = Unmanaged<CFNumber>.fromOpaque(value).takeUnretainedValue()
    return CFNumberIsFloatType(number)
}

@_cdecl("cf_data_get_type_id")
public func cf_data_get_type_id() -> Int {
    Int(CFDataGetTypeID())
}

@_cdecl("cf_data_create")
public func cf_data_create(_ bytes: UnsafePointer<UInt8>?, _ len: Int) -> UnsafeMutableRawPointer? {
    guard let data = CFDataCreate(nil, bytes, len) else { return nil }
    return Unmanaged.passRetained(data).toOpaque()
}

@_cdecl("cf_data_get_length")
public func cf_data_get_length(_ value: UnsafeMutableRawPointer) -> Int {
    let data = Unmanaged<CFData>.fromOpaque(value).takeUnretainedValue()
    return CFDataGetLength(data)
}

@_cdecl("cf_data_copy_bytes")
public func cf_data_copy_bytes(_ value: UnsafeMutableRawPointer, _ buffer: UnsafeMutablePointer<UInt8>) {
    let data = Unmanaged<CFData>.fromOpaque(value).takeUnretainedValue()
    let length = CFDataGetLength(data)
    guard let bytePtr = CFDataGetBytePtr(data), length > 0 else { return }
    buffer.initialize(from: bytePtr, count: length)
}

@_cdecl("cf_date_get_type_id")
public func cf_date_get_type_id() -> Int {
    Int(CFDateGetTypeID())
}

@_cdecl("cf_date_create")
public func cf_date_create(_ absoluteTime: Double) -> UnsafeMutableRawPointer? {
    guard let date = CFDateCreate(nil, absoluteTime) else { return nil }
    return Unmanaged.passRetained(date).toOpaque()
}

@_cdecl("cf_date_get_absolute_time")
public func cf_date_get_absolute_time(_ value: UnsafeMutableRawPointer) -> Double {
    let date = Unmanaged<CFDate>.fromOpaque(value).takeUnretainedValue()
    return CFDateGetAbsoluteTime(date)
}

@_cdecl("cf_uuid_get_type_id")
public func cf_uuid_get_type_id() -> Int {
    Int(CFUUIDGetTypeID())
}

@_cdecl("cf_uuid_create")
public func cf_uuid_create() -> UnsafeMutableRawPointer? {
    guard let uuid = CFUUIDCreate(nil) else { return nil }
    return Unmanaged.passRetained(uuid).toOpaque()
}

@_cdecl("cf_uuid_create_from_string")
public func cf_uuid_create_from_string(_ value: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let string = acfCFString(from: value), let uuid = CFUUIDCreateFromString(nil, string) else {
        return nil
    }
    return Unmanaged.passRetained(uuid).toOpaque()
}

@_cdecl("cf_uuid_copy_string")
public func cf_uuid_copy_string(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let uuid = Unmanaged<CFUUID>.fromOpaque(value).takeUnretainedValue()
    guard let string = CFUUIDCreateString(nil, uuid) else { return nil }
    return Unmanaged.passRetained(string).toOpaque()
}

@_cdecl("cf_uuid_get_bytes")
public func cf_uuid_get_bytes(_ value: UnsafeMutableRawPointer, _ outBytes: UnsafeMutablePointer<UInt8>) {
    let uuid = Unmanaged<CFUUID>.fromOpaque(value).takeUnretainedValue()
    let bytes = CFUUIDGetUUIDBytes(uuid)
    outBytes[0] = bytes.byte0
    outBytes[1] = bytes.byte1
    outBytes[2] = bytes.byte2
    outBytes[3] = bytes.byte3
    outBytes[4] = bytes.byte4
    outBytes[5] = bytes.byte5
    outBytes[6] = bytes.byte6
    outBytes[7] = bytes.byte7
    outBytes[8] = bytes.byte8
    outBytes[9] = bytes.byte9
    outBytes[10] = bytes.byte10
    outBytes[11] = bytes.byte11
    outBytes[12] = bytes.byte12
    outBytes[13] = bytes.byte13
    outBytes[14] = bytes.byte14
    outBytes[15] = bytes.byte15
}

@_cdecl("cf_error_get_type_id")
public func cf_error_get_type_id() -> Int {
    Int(CFErrorGetTypeID())
}

@_cdecl("cf_error_create")
public func cf_error_create(
    _ domain: UnsafeMutableRawPointer,
    _ code: Int64,
    _ description: UnsafePointer<CChar>?
) -> UnsafeMutableRawPointer? {
    let domain = Unmanaged<CFString>.fromOpaque(domain).takeUnretainedValue()
    var userInfo: CFDictionary?
    if let description, let descriptionString = acfCFString(from: description) {
        let dictionary = [kCFErrorLocalizedDescriptionKey as AnyHashable: descriptionString] as NSDictionary
        userInfo = unsafeBitCast(dictionary, to: CFDictionary.self)
    }
    guard let error = CFErrorCreate(nil, domain, Int(code), userInfo) else { return nil }
    return Unmanaged.passRetained(error).toOpaque()
}

@_cdecl("cf_error_get_domain")
public func cf_error_get_domain(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let error = Unmanaged<CFError>.fromOpaque(value).takeUnretainedValue()
    return Unmanaged.passRetained(CFErrorGetDomain(error)).toOpaque()
}

@_cdecl("cf_error_get_code")
public func cf_error_get_code(_ value: UnsafeMutableRawPointer) -> Int64 {
    let error = Unmanaged<CFError>.fromOpaque(value).takeUnretainedValue()
    return Int64(CFErrorGetCode(error))
}

@_cdecl("cf_error_copy_description")
public func cf_error_copy_description(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let error = Unmanaged<CFError>.fromOpaque(value).takeUnretainedValue()
    guard let description = CFErrorCopyDescription(error) else { return nil }
    return Unmanaged.passRetained(description).toOpaque()
}

@_cdecl("cf_error_copy_failure_reason")
public func cf_error_copy_failure_reason(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let error = Unmanaged<CFError>.fromOpaque(value).takeUnretainedValue()
    guard let reason = CFErrorCopyFailureReason(error) else { return nil }
    return Unmanaged.passRetained(reason).toOpaque()
}
