import CoreFoundation
import Foundation

private func acfArrayFromRawPointers(_ pointers: [UnsafeRawPointer?]) -> CFArray? {
    var callbacks = kCFTypeArrayCallBacks
    var mutablePointers = pointers
    return mutablePointers.withUnsafeMutableBufferPointer { buffer in
        CFArrayCreate(nil, buffer.baseAddress, buffer.count, &callbacks)
    }
}

private func acfDictionaryFromRawPointers(
    keys: [UnsafeRawPointer?],
    values: [UnsafeRawPointer?]
) -> CFDictionary? {
    precondition(keys.count == values.count)
    let count = keys.count
    var keys = keys
    var values = values
    var keyCallbacks = kCFTypeDictionaryKeyCallBacks
    var valueCallbacks = kCFTypeDictionaryValueCallBacks
    return keys.withUnsafeMutableBufferPointer { keyBuffer in
        values.withUnsafeMutableBufferPointer { valueBuffer in
            CFDictionaryCreate(
                nil,
                keyBuffer.baseAddress,
                valueBuffer.baseAddress,
                count,
                &keyCallbacks,
                &valueCallbacks
            )
        }
    }
}

private func acfBagFromRawPointers(_ pointers: [UnsafeRawPointer?]) -> CFBag? {
    var callbacks = kCFTypeBagCallBacks
    var pointers = pointers
    return pointers.withUnsafeMutableBufferPointer { buffer in
        CFBagCreate(nil, buffer.baseAddress, buffer.count, &callbacks)
    }
}

private func acfSetCallbacks(_ kind: Int32) -> CFSetCallBacks {
    switch kind {
    case 1:
        return kCFCopyStringSetCallBacks
    default:
        return kCFTypeSetCallBacks
    }
}

private func acfSetFromRawPointers(_ pointers: [UnsafeRawPointer?], callbacksKind: Int32) -> CFSet? {
    var callbacks = acfSetCallbacks(callbacksKind)
    var pointers = pointers
    return pointers.withUnsafeMutableBufferPointer { buffer in
        CFSetCreate(nil, buffer.baseAddress, buffer.count, &callbacks)
    }
}

private func acfPropertyListFormat(_ rawValue: Int) -> CFPropertyListFormat {
    switch rawValue {
    case 1:
        return .openStepFormat
    case 100:
        return .xmlFormat_v1_0
    default:
        return .binaryFormat_v1_0
    }
}

private func acfRetainedOpaque(_ value: AnyObject?) -> UnsafeMutableRawPointer? {
    guard let value else { return nil }
    return Unmanaged.passRetained(value).toOpaque()
}

private func acfStoreRetainedCFError(
    _ outError: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ error: Unmanaged<CFError>?
) {
    outError?.pointee = error.map { unmanaged in
        let value = unmanaged.takeRetainedValue()
        return Unmanaged.passRetained(value).toOpaque()
    }
}

@_cdecl("cf_array_get_type_id")
public func cf_array_get_type_id() -> Int {
    Int(CFArrayGetTypeID())
}

@_cdecl("cf_array_create")
public func cf_array_create(_ values: UnsafePointer<UnsafeMutableRawPointer?>?, _ count: Int) -> UnsafeMutableRawPointer? {
    let pointers = UnsafeBufferPointer(start: values, count: count).map { raw -> UnsafeRawPointer? in
        raw.map(UnsafeRawPointer.init)
    }
    guard let array = acfArrayFromRawPointers(pointers) else { return nil }
    return Unmanaged.passRetained(array).toOpaque()
}

@_cdecl("cf_array_get_count")
public func cf_array_get_count(_ value: UnsafeMutableRawPointer) -> Int {
    let array = Unmanaged<CFArray>.fromOpaque(value).takeUnretainedValue()
    return CFArrayGetCount(array)
}

@_cdecl("cf_array_get_value_at_index")
public func cf_array_get_value_at_index(_ value: UnsafeMutableRawPointer, _ index: Int) -> UnsafeMutableRawPointer? {
    let array = Unmanaged<CFArray>.fromOpaque(value).takeUnretainedValue()
    guard index >= 0, index < CFArrayGetCount(array) else { return nil }
    let raw = CFArrayGetValueAtIndex(array, index)
    return acfRetainedCFType(UnsafeMutableRawPointer(mutating: raw))
}

@_cdecl("cf_dictionary_get_type_id")
public func cf_dictionary_get_type_id() -> Int {
    Int(CFDictionaryGetTypeID())
}

@_cdecl("cf_dictionary_create")
public func cf_dictionary_create(
    _ keys: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ values: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ count: Int
) -> UnsafeMutableRawPointer? {
    let keyPointers = UnsafeBufferPointer(start: keys, count: count).map { $0.map(UnsafeRawPointer.init) }
    let valuePointers = UnsafeBufferPointer(start: values, count: count).map { $0.map(UnsafeRawPointer.init) }
    guard let dictionary = acfDictionaryFromRawPointers(keys: keyPointers, values: valuePointers) else {
        return nil
    }
    return Unmanaged.passRetained(dictionary).toOpaque()
}

@_cdecl("cf_dictionary_get_count")
public func cf_dictionary_get_count(_ value: UnsafeMutableRawPointer) -> Int {
    let dictionary = Unmanaged<CFDictionary>.fromOpaque(value).takeUnretainedValue()
    return CFDictionaryGetCount(dictionary)
}

@_cdecl("cf_dictionary_contains_key")
public func cf_dictionary_contains_key(_ value: UnsafeMutableRawPointer, _ key: UnsafeMutableRawPointer) -> Bool {
    let dictionary = Unmanaged<CFDictionary>.fromOpaque(value).takeUnretainedValue()
    return CFDictionaryContainsKey(dictionary, UnsafeRawPointer(key))
}

@_cdecl("cf_dictionary_get_value")
public func cf_dictionary_get_value(_ value: UnsafeMutableRawPointer, _ key: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let dictionary = Unmanaged<CFDictionary>.fromOpaque(value).takeUnretainedValue()
    let raw = CFDictionaryGetValue(dictionary, UnsafeRawPointer(key))
    return acfRetainedCFType(UnsafeMutableRawPointer(mutating: raw))
}

@_cdecl("cf_dictionary_copy_keys")
public func cf_dictionary_copy_keys(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let dictionary = Unmanaged<CFDictionary>.fromOpaque(value).takeUnretainedValue()
    let count = CFDictionaryGetCount(dictionary)
    var keys = Array<UnsafeRawPointer?>(repeating: nil, count: count)
    CFDictionaryGetKeysAndValues(dictionary, &keys, nil)
    guard let array = acfArrayFromRawPointers(keys) else { return nil }
    return Unmanaged.passRetained(array).toOpaque()
}

@_cdecl("cf_dictionary_copy_values")
public func cf_dictionary_copy_values(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let dictionary = Unmanaged<CFDictionary>.fromOpaque(value).takeUnretainedValue()
    let count = CFDictionaryGetCount(dictionary)
    var values = Array<UnsafeRawPointer?>(repeating: nil, count: count)
    CFDictionaryGetKeysAndValues(dictionary, nil, &values)
    guard let array = acfArrayFromRawPointers(values) else { return nil }
    return Unmanaged.passRetained(array).toOpaque()
}

@_cdecl("cf_bag_get_type_id")
public func cf_bag_get_type_id() -> Int {
    Int(CFBagGetTypeID())
}

@_cdecl("cf_bag_create")
public func cf_bag_create(_ values: UnsafePointer<UnsafeMutableRawPointer?>?, _ count: Int) -> UnsafeMutableRawPointer? {
    let pointers = UnsafeBufferPointer(start: values, count: count).map { $0.map(UnsafeRawPointer.init) }
    guard let bag = acfBagFromRawPointers(pointers) else { return nil }
    return Unmanaged.passRetained(bag).toOpaque()
}

@_cdecl("cf_bag_get_count")
public func cf_bag_get_count(_ value: UnsafeMutableRawPointer) -> Int {
    let bag = Unmanaged<CFBag>.fromOpaque(value).takeUnretainedValue()
    return CFBagGetCount(bag)
}

@_cdecl("cf_bag_contains_value")
public func cf_bag_contains_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) -> Bool {
    let bag = Unmanaged<CFBag>.fromOpaque(value).takeUnretainedValue()
    return CFBagContainsValue(bag, UnsafeRawPointer(candidate))
}

@_cdecl("cf_bag_get_count_of_value")
public func cf_bag_get_count_of_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) -> Int {
    let bag = Unmanaged<CFBag>.fromOpaque(value).takeUnretainedValue()
    return CFBagGetCountOfValue(bag, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_get_type_id")
public func cf_set_get_type_id() -> Int {
    Int(CFSetGetTypeID())
}

@_cdecl("cf_set_create")
public func cf_set_create(
    _ values: UnsafePointer<UnsafeMutableRawPointer?>?,
    _ count: Int,
    _ callbacksKind: Int32
) -> UnsafeMutableRawPointer? {
    let pointers = UnsafeBufferPointer(start: values, count: count).map { $0.map(UnsafeRawPointer.init) }
    guard let set = acfSetFromRawPointers(pointers, callbacksKind: callbacksKind) else { return nil }
    return Unmanaged.passRetained(set).toOpaque()
}

@_cdecl("cf_set_create_copy")
public func cf_set_create_copy(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    guard let copy = CFSetCreateCopy(nil, set) else { return nil }
    return Unmanaged.passRetained(copy).toOpaque()
}

@_cdecl("cf_set_create_mutable")
public func cf_set_create_mutable(_ capacity: Int, _ callbacksKind: Int32) -> UnsafeMutableRawPointer? {
    var callbacks = acfSetCallbacks(callbacksKind)
    guard let set = CFSetCreateMutable(nil, capacity, &callbacks) else { return nil }
    return Unmanaged.passRetained(set).toOpaque()
}

@_cdecl("cf_set_create_mutable_copy")
public func cf_set_create_mutable_copy(_ value: UnsafeMutableRawPointer, _ capacity: Int) -> UnsafeMutableRawPointer? {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    guard let copy = CFSetCreateMutableCopy(nil, capacity, set) else { return nil }
    return Unmanaged.passRetained(copy).toOpaque()
}

@_cdecl("cf_set_get_count")
public func cf_set_get_count(_ value: UnsafeMutableRawPointer) -> Int {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    return CFSetGetCount(set)
}

@_cdecl("cf_set_get_count_of_value")
public func cf_set_get_count_of_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) -> Int {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    return CFSetGetCountOfValue(set, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_contains_value")
public func cf_set_contains_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) -> Bool {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    return CFSetContainsValue(set, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_get_value")
public func cf_set_get_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    let raw = CFSetGetValue(set, UnsafeRawPointer(candidate))
    return acfRetainedCFType(UnsafeMutableRawPointer(mutating: raw))
}

@_cdecl("cf_set_get_value_if_present")
public func cf_set_get_value_if_present(
    _ value: UnsafeMutableRawPointer,
    _ candidate: UnsafeMutableRawPointer,
    _ outValue: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) -> Bool {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    var raw: UnsafeRawPointer?
    let present = CFSetGetValueIfPresent(set, UnsafeRawPointer(candidate), &raw)
    outValue.pointee = acfRetainedCFType(UnsafeMutableRawPointer(mutating: raw))
    return present
}

@_cdecl("cf_set_get_values")
public func cf_set_get_values(_ value: UnsafeMutableRawPointer, _ outValues: UnsafeMutablePointer<UnsafeMutableRawPointer?>) {
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    let count = CFSetGetCount(set)
    var values = Array<UnsafeRawPointer?>(repeating: nil, count: count)
    if count > 0 {
        CFSetGetValues(set, &values)
    }
    for (index, raw) in values.enumerated() {
        outValues.advanced(by: index).pointee = acfRetainedCFType(UnsafeMutableRawPointer(mutating: raw))
    }
}

@_cdecl("cf_set_apply_function")
public func cf_set_apply_function(
    _ value: UnsafeMutableRawPointer,
    _ context: UnsafeMutableRawPointer?,
    _ callback: (@convention(c) (UnsafeMutableRawPointer?, UnsafeMutableRawPointer?) -> Void)?
) {
    guard let callback else { return }
    let set = Unmanaged<CFSet>.fromOpaque(value).takeUnretainedValue()
    let count = CFSetGetCount(set)
    var values = Array<UnsafeRawPointer?>(repeating: nil, count: count)
    if count > 0 {
        CFSetGetValues(set, &values)
    }
    for raw in values {
        callback(acfRetainedCFType(UnsafeMutableRawPointer(mutating: raw)), context)
    }
}

@_cdecl("cf_set_add_value")
public func cf_set_add_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) {
    let set = Unmanaged<CFMutableSet>.fromOpaque(value).takeUnretainedValue()
    CFSetAddValue(set, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_replace_value")
public func cf_set_replace_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) {
    let set = Unmanaged<CFMutableSet>.fromOpaque(value).takeUnretainedValue()
    CFSetReplaceValue(set, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_set_value")
public func cf_set_set_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) {
    let set = Unmanaged<CFMutableSet>.fromOpaque(value).takeUnretainedValue()
    CFSetSetValue(set, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_remove_value")
public func cf_set_remove_value(_ value: UnsafeMutableRawPointer, _ candidate: UnsafeMutableRawPointer) {
    let set = Unmanaged<CFMutableSet>.fromOpaque(value).takeUnretainedValue()
    CFSetRemoveValue(set, UnsafeRawPointer(candidate))
}

@_cdecl("cf_set_remove_all_values")
public func cf_set_remove_all_values(_ value: UnsafeMutableRawPointer) {
    let set = Unmanaged<CFMutableSet>.fromOpaque(value).takeUnretainedValue()
    CFSetRemoveAllValues(set)
}

@_cdecl("cf_property_list_create_deep_copy")
public func cf_property_list_create_deep_copy(_ value: UnsafeMutableRawPointer, _ options: UInt64) -> UnsafeMutableRawPointer? {
    let propertyList = acfBorrowedAnyObject(value)
    guard let copy = CFPropertyListCreateDeepCopy(nil, propertyList, CFOptionFlags(options)) else {
        return nil
    }
    return Unmanaged.passRetained(copy).toOpaque()
}

@_cdecl("cf_property_list_create_with_data")
public func cf_property_list_create_with_data(
    _ data: UnsafeMutableRawPointer,
    _ options: UInt64,
    _ outFormat: UnsafeMutablePointer<Int>?,
    _ outError: UnsafeMutablePointer<UnsafeMutableRawPointer?>?
) -> UnsafeMutableRawPointer? {
    let data = Unmanaged<CFData>.fromOpaque(data).takeUnretainedValue()
    var format: CFPropertyListFormat = .openStepFormat
    var error: Unmanaged<CFError>?
    let propertyList = CFPropertyListCreateWithData(nil, data, CFOptionFlags(options), &format, &error)?.takeRetainedValue()
    outFormat?.pointee = format.rawValue
    acfStoreRetainedCFError(outError, error)
    return acfRetainedOpaque(propertyList)
}

@_cdecl("cf_property_list_create_with_stream")
public func cf_property_list_create_with_stream(
    _ stream: UnsafeMutableRawPointer,
    _ streamLength: Int,
    _ options: UInt64,
    _ outFormat: UnsafeMutablePointer<Int>?,
    _ outError: UnsafeMutablePointer<UnsafeMutableRawPointer?>?
) -> UnsafeMutableRawPointer? {
    let stream = Unmanaged<CFReadStream>.fromOpaque(stream).takeUnretainedValue()
    var format: CFPropertyListFormat = .openStepFormat
    var error: Unmanaged<CFError>?
    let propertyList = CFPropertyListCreateWithStream(nil, stream, streamLength, CFOptionFlags(options), &format, &error)?.takeRetainedValue()
    outFormat?.pointee = format.rawValue
    acfStoreRetainedCFError(outError, error)
    return acfRetainedOpaque(propertyList)
}

@_cdecl("cf_property_list_write")
public func cf_property_list_write(
    _ value: UnsafeMutableRawPointer,
    _ stream: UnsafeMutableRawPointer,
    _ format: Int,
    _ options: UInt64,
    _ outError: UnsafeMutablePointer<UnsafeMutableRawPointer?>?
) -> Int {
    let propertyList = acfBorrowedAnyObject(value)
    let stream = Unmanaged<CFWriteStream>.fromOpaque(stream).takeUnretainedValue()
    var error: Unmanaged<CFError>?
    let written = CFPropertyListWrite(propertyList, stream, acfPropertyListFormat(format), CFOptionFlags(options), &error)
    acfStoreRetainedCFError(outError, error)
    return written
}

@_cdecl("cf_property_list_create_data")
public func cf_property_list_create_data(
    _ value: UnsafeMutableRawPointer,
    _ format: Int,
    _ options: UInt64,
    _ outError: UnsafeMutablePointer<UnsafeMutableRawPointer?>?
) -> UnsafeMutableRawPointer? {
    let propertyList = acfBorrowedAnyObject(value)
    var error: Unmanaged<CFError>?
    let data = CFPropertyListCreateData(nil, propertyList, acfPropertyListFormat(format), CFOptionFlags(options), &error)?.takeRetainedValue()
    acfStoreRetainedCFError(outError, error)
    return acfRetainedOpaque(data)
}

@_cdecl("cf_property_list_is_valid")
public func cf_property_list_is_valid(_ value: UnsafeMutableRawPointer, _ format: Int) -> Bool {
    let propertyList = acfBorrowedAnyObject(value)
    return CFPropertyListIsValid(propertyList, acfPropertyListFormat(format))
}

@_cdecl("cf_attributed_string_get_type_id")
public func cf_attributed_string_get_type_id() -> Int {
    Int(CFAttributedStringGetTypeID())
}

@_cdecl("cf_attributed_string_create")
public func cf_attributed_string_create(_ string: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let string = Unmanaged<CFString>.fromOpaque(string).takeUnretainedValue()
    guard let attributed = CFAttributedStringCreate(nil, string, nil) else { return nil }
    return Unmanaged.passRetained(attributed).toOpaque()
}

@_cdecl("cf_attributed_string_get_string")
public func cf_attributed_string_get_string(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let attributed = Unmanaged<CFAttributedString>.fromOpaque(value).takeUnretainedValue()
    return Unmanaged.passRetained(CFAttributedStringGetString(attributed)).toOpaque()
}

@_cdecl("cf_attributed_string_get_length")
public func cf_attributed_string_get_length(_ value: UnsafeMutableRawPointer) -> Int {
    let attributed = Unmanaged<CFAttributedString>.fromOpaque(value).takeUnretainedValue()
    return CFAttributedStringGetLength(attributed)
}

final class ACFTreeNode {
    let value: AnyObject?
    var children: [ACFTreeNode] = []

    init(value: AnyObject?) {
        self.value = value
    }
}

@_cdecl("cf_tree_create")
public func cf_tree_create(_ value: UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer? {
    let object = value.map(acfBorrowedAnyObject)
    return Unmanaged.passRetained(ACFTreeNode(value: object)).toOpaque()
}

@_cdecl("cf_tree_append_child")
public func cf_tree_append_child(_ parent: UnsafeMutableRawPointer, _ child: UnsafeMutableRawPointer) {
    let parentNode = Unmanaged<ACFTreeNode>.fromOpaque(parent).takeUnretainedValue()
    let childNode = Unmanaged<ACFTreeNode>.fromOpaque(child).takeUnretainedValue()
    parentNode.children.append(childNode)
}

@_cdecl("cf_tree_get_child_count")
public func cf_tree_get_child_count(_ value: UnsafeMutableRawPointer) -> Int {
    let node = Unmanaged<ACFTreeNode>.fromOpaque(value).takeUnretainedValue()
    return node.children.count
}

@_cdecl("cf_tree_get_child_at_index")
public func cf_tree_get_child_at_index(_ value: UnsafeMutableRawPointer, _ index: Int) -> UnsafeMutableRawPointer? {
    let node = Unmanaged<ACFTreeNode>.fromOpaque(value).takeUnretainedValue()
    guard index >= 0, index < node.children.count else { return nil }
    return Unmanaged.passRetained(node.children[index]).toOpaque()
}

@_cdecl("cf_tree_copy_value")
public func cf_tree_copy_value(_ value: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer? {
    let node = Unmanaged<ACFTreeNode>.fromOpaque(value).takeUnretainedValue()
    guard let object = node.value else { return nil }
    return Unmanaged.passRetained(object).toOpaque()
}
