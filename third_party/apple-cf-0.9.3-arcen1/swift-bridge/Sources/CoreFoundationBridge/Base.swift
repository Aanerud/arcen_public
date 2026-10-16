import CoreFoundation
import Foundation

func acfCFString(from cString: UnsafePointer<CChar>?) -> CFString? {
    guard let cString else { return nil }
    return CFStringCreateWithCString(nil, cString, CFStringBuiltInEncodings.UTF8.rawValue)
}

func acfCopyCString(from string: CFString) -> UnsafeMutablePointer<CChar>? {
    let length = CFStringGetLength(string)
    let capacity = CFStringGetMaximumSizeForEncoding(length, CFStringBuiltInEncodings.UTF8.rawValue) + 1
    let buffer = UnsafeMutablePointer<CChar>.allocate(capacity: capacity)
    let ok = CFStringGetCString(
        string,
        buffer,
        capacity,
        CFStringBuiltInEncodings.UTF8.rawValue
    )
    if ok {
        return buffer
    }
    buffer.deallocate()
    return nil
}

func acfRetainedPointer<T: AnyObject>(_ value: T?) -> UnsafeMutableRawPointer? {
    guard let value else { return nil }
    return Unmanaged.passRetained(value).toOpaque()
}

func acfBorrowedAnyObject(_ value: UnsafeMutableRawPointer) -> AnyObject {
    Unmanaged<AnyObject>.fromOpaque(value).takeUnretainedValue()
}

func acfRetainedCFType(_ value: UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer? {
    guard let value else { return nil }
    let object = acfBorrowedAnyObject(value)
    return Unmanaged.passRetained(object).toOpaque()
}

@_cdecl("acf_object_release")
public func acf_object_release(_ value: UnsafeMutableRawPointer) {
    Unmanaged<AnyObject>.fromOpaque(value).release()
}

@_cdecl("acf_object_retain")
public func acf_object_retain(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer {
    let object = acfBorrowedAnyObject(value)
    return Unmanaged.passRetained(object).toOpaque()
}

@_cdecl("acf_object_hash")
public func acf_object_hash(_ value: UnsafeMutableRawPointer) -> Int {
    let object = acfBorrowedAnyObject(value)
    return ObjectIdentifier(object).hashValue
}

@_cdecl("cf_type_release")
public func cf_type_release(_ value: UnsafeMutableRawPointer) {
    Unmanaged<AnyObject>.fromOpaque(value).release()
}

@_cdecl("cf_type_retain")
public func cf_type_retain(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer {
    let object = acfBorrowedAnyObject(value)
    return Unmanaged.passRetained(object).toOpaque()
}

@_cdecl("cf_type_hash")
public func cf_type_hash(_ value: UnsafeMutableRawPointer) -> Int {
    let object = unsafeBitCast(acfBorrowedAnyObject(value), to: CFTypeRef.self)
    return Int(CFHash(object))
}

@_cdecl("cf_type_equal")
public func cf_type_equal(_ lhs: UnsafeMutableRawPointer, _ rhs: UnsafeMutableRawPointer) -> Bool {
    let lhsObject = unsafeBitCast(acfBorrowedAnyObject(lhs), to: CFTypeRef.self)
    let rhsObject = unsafeBitCast(acfBorrowedAnyObject(rhs), to: CFTypeRef.self)
    return CFEqual(lhsObject, rhsObject)
}

@_cdecl("cf_type_get_type_id")
public func cf_type_get_type_id(_ value: UnsafeMutableRawPointer) -> Int {
    let object = unsafeBitCast(acfBorrowedAnyObject(value), to: CFTypeRef.self)
    return Int(CFGetTypeID(object))
}

@_cdecl("cf_type_copy_description")
public func cf_type_copy_description(_ value: UnsafeMutableRawPointer) -> UnsafeMutablePointer<CChar>? {
    let object = unsafeBitCast(acfBorrowedAnyObject(value), to: CFTypeRef.self)
    guard let description = CFCopyDescription(object) else {
        return nil
    }
    return acfCopyCString(from: description)
}
