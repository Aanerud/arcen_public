import CoreFoundation
import Foundation
import Darwin

private let acfEchoMessagePortCallback: CFMessagePortCallBack = { _, _, data, _ in
    guard let data else { return nil }
    return Unmanaged.passRetained(data)
}

private let acfNoopFileDescriptorCallback: CFFileDescriptorCallBack = { _, _, _ in }

@_cdecl("cf_notification_center_get_type_id")
public func cf_notification_center_get_type_id() -> Int {
    Int(CFNotificationCenterGetTypeID())
}

@_cdecl("cf_notification_center_get_local")
public func cf_notification_center_get_local() -> UnsafeMutableRawPointer? {
    Unmanaged.passRetained(CFNotificationCenterGetLocalCenter()).toOpaque()
}

@_cdecl("cf_notification_center_get_distributed")
public func cf_notification_center_get_distributed() -> UnsafeMutableRawPointer? {
    Unmanaged.passRetained(CFNotificationCenterGetDistributedCenter()).toOpaque()
}

@_cdecl("cf_notification_center_get_darwin")
public func cf_notification_center_get_darwin() -> UnsafeMutableRawPointer? {
    Unmanaged.passRetained(CFNotificationCenterGetDarwinNotifyCenter()).toOpaque()
}

@_cdecl("cf_notification_center_post_notification")
public func cf_notification_center_post_notification(
    _ center: UnsafeMutableRawPointer,
    _ name: UnsafeMutableRawPointer,
    _ userInfo: UnsafeMutableRawPointer?,
    _ deliverImmediately: Bool
) {
    let center = Unmanaged<CFNotificationCenter>.fromOpaque(center).takeUnretainedValue()
    let name = Unmanaged<CFString>.fromOpaque(name).takeUnretainedValue()
    let notificationName = CFNotificationName(rawValue: name)
    let userInfo = userInfo.map { Unmanaged<CFDictionary>.fromOpaque($0).takeUnretainedValue() }
    CFNotificationCenterPostNotification(center, notificationName, nil, userInfo, deliverImmediately)
}

@_cdecl("cf_run_loop_get_type_id")
public func cf_run_loop_get_type_id() -> Int {
    Int(CFRunLoopGetTypeID())
}

@_cdecl("cf_run_loop_get_current")
public func cf_run_loop_get_current() -> UnsafeMutableRawPointer? {
    Unmanaged.passRetained(CFRunLoopGetCurrent()).toOpaque()
}

@_cdecl("cf_run_loop_get_main")
public func cf_run_loop_get_main() -> UnsafeMutableRawPointer? {
    Unmanaged.passRetained(CFRunLoopGetMain()).toOpaque()
}

@_cdecl("cf_run_loop_run_in_default_mode")
public func cf_run_loop_run_in_default_mode(_ seconds: Double, _ returnAfterSourceHandled: Bool) -> Int32 {
    Int32(CFRunLoopRunInMode(CFRunLoopMode.defaultMode!, seconds, returnAfterSourceHandled).rawValue)
}

@_cdecl("cf_run_loop_stop")
public func cf_run_loop_stop(_ value: UnsafeMutableRawPointer) {
    let runLoop = Unmanaged<CFRunLoop>.fromOpaque(value).takeUnretainedValue()
    CFRunLoopStop(runLoop)
}

@_cdecl("cf_run_loop_wake_up")
public func cf_run_loop_wake_up(_ value: UnsafeMutableRawPointer) {
    let runLoop = Unmanaged<CFRunLoop>.fromOpaque(value).takeUnretainedValue()
    CFRunLoopWakeUp(runLoop)
}

@_cdecl("cf_run_loop_add_timer")
public func cf_run_loop_add_timer(_ value: UnsafeMutableRawPointer, _ timer: UnsafeMutableRawPointer) {
    let runLoop = Unmanaged<CFRunLoop>.fromOpaque(value).takeUnretainedValue()
    let timer = Unmanaged<CFRunLoopTimer>.fromOpaque(timer).takeUnretainedValue()
    CFRunLoopAddTimer(runLoop, timer, CFRunLoopMode.defaultMode!)
}

@_cdecl("cf_run_loop_timer_get_type_id")
public func cf_run_loop_timer_get_type_id() -> Int {
    Int(CFRunLoopTimerGetTypeID())
}

@_cdecl("cf_run_loop_timer_create")
public func cf_run_loop_timer_create(_ intervalSeconds: Double, _ repeats: Bool) -> UnsafeMutableRawPointer? {
    let fireDate = CFAbsoluteTimeGetCurrent() + intervalSeconds
    let interval = repeats ? intervalSeconds : 0.0
    guard let timer = CFRunLoopTimerCreateWithHandler(nil, fireDate, interval, 0, 0, { _ in }) else {
        return nil
    }
    return Unmanaged.passRetained(timer).toOpaque()
}

@_cdecl("cf_run_loop_timer_is_valid")
public func cf_run_loop_timer_is_valid(_ value: UnsafeMutableRawPointer) -> Bool {
    let timer = Unmanaged<CFRunLoopTimer>.fromOpaque(value).takeUnretainedValue()
    return CFRunLoopTimerIsValid(timer)
}

@_cdecl("cf_run_loop_timer_invalidate")
public func cf_run_loop_timer_invalidate(_ value: UnsafeMutableRawPointer) {
    let timer = Unmanaged<CFRunLoopTimer>.fromOpaque(value).takeUnretainedValue()
    CFRunLoopTimerInvalidate(timer)
}

@_cdecl("cf_run_loop_timer_fire")
public func cf_run_loop_timer_fire(_ value: UnsafeMutableRawPointer) {
    let timer = Unmanaged<CFRunLoopTimer>.fromOpaque(value).takeUnretainedValue()
    CFRunLoopTimerSetNextFireDate(timer, CFAbsoluteTimeGetCurrent())
}

@_cdecl("cf_message_port_get_type_id")
public func cf_message_port_get_type_id() -> Int {
    Int(CFMessagePortGetTypeID())
}

@_cdecl("cf_message_port_create_echo_local")
public func cf_message_port_create_echo_local(_ name: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let name = acfCFString(from: name) else { return nil }
    var shouldFreeInfo: DarwinBoolean = false
    guard let port = CFMessagePortCreateLocal(nil, name, acfEchoMessagePortCallback, nil, &shouldFreeInfo) else {
        return nil
    }
    if let source = CFMessagePortCreateRunLoopSource(nil, port, 0) {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, CFRunLoopMode.defaultMode!)
    }
    return Unmanaged.passRetained(port).toOpaque()
}

@_cdecl("cf_message_port_create_remote")
public func cf_message_port_create_remote(_ name: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    guard let name = acfCFString(from: name), let port = CFMessagePortCreateRemote(nil, name) else {
        return nil
    }
    return Unmanaged.passRetained(port).toOpaque()
}

@_cdecl("cf_message_port_send_request")
public func cf_message_port_send_request(
    _ value: UnsafeMutableRawPointer,
    _ bytes: UnsafePointer<UInt8>?,
    _ len: Int,
    _ timeoutSeconds: Double,
    _ outBytes: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
    _ outLen: UnsafeMutablePointer<Int>
) -> Int32 {
    let port = Unmanaged<CFMessagePort>.fromOpaque(value).takeUnretainedValue()
    let data = bytes.flatMap { CFDataCreate(nil, $0, len) }
    var returnData: Unmanaged<CFData>?
    let status = CFMessagePortSendRequest(
        port,
        0,
        data,
        timeoutSeconds,
        timeoutSeconds,
        CFRunLoopMode.defaultMode?.rawValue,
        &returnData
    )
    guard status == kCFMessagePortSuccess, let returnData else {
        outBytes.pointee = nil
        outLen.pointee = 0
        return status
    }
    let dataRef = returnData.takeRetainedValue()
    let count = CFDataGetLength(dataRef)
    let buffer = UnsafeMutablePointer<UInt8>.allocate(capacity: count)
    if let bytePtr = CFDataGetBytePtr(dataRef), count > 0 {
        buffer.initialize(from: bytePtr, count: count)
    }
    outBytes.pointee = buffer
    outLen.pointee = count
    return status
}

@_cdecl("cf_message_port_free_bytes")
public func cf_message_port_free_bytes(_ bytes: UnsafeMutablePointer<UInt8>?, _ len: Int) {
    bytes?.deallocate()
}

@_cdecl("cf_message_port_invalidate")
public func cf_message_port_invalidate(_ value: UnsafeMutableRawPointer) {
    let port = Unmanaged<CFMessagePort>.fromOpaque(value).takeUnretainedValue()
    CFMessagePortInvalidate(port)
}

@_cdecl("cf_read_stream_get_type_id")
public func cf_read_stream_get_type_id() -> Int {
    Int(CFReadStreamGetTypeID())
}

@_cdecl("cf_write_stream_get_type_id")
public func cf_write_stream_get_type_id() -> Int {
    Int(CFWriteStreamGetTypeID())
}

@_cdecl("cf_stream_create_bound_pair")
public func cf_stream_create_bound_pair(
    _ transferBufferSize: Int,
    _ outRead: UnsafeMutablePointer<UnsafeMutableRawPointer?>,
    _ outWrite: UnsafeMutablePointer<UnsafeMutableRawPointer?>
) {
    var read: Unmanaged<CFReadStream>?
    var write: Unmanaged<CFWriteStream>?
    CFStreamCreateBoundPair(nil, &read, &write, transferBufferSize)
    outRead.pointee = read?.toOpaque()
    outWrite.pointee = write?.toOpaque()
}

@_cdecl("cf_read_stream_open")
public func cf_read_stream_open(_ value: UnsafeMutableRawPointer) -> Bool {
    let stream = Unmanaged<CFReadStream>.fromOpaque(value).takeUnretainedValue()
    return CFReadStreamOpen(stream)
}

@_cdecl("cf_read_stream_close")
public func cf_read_stream_close(_ value: UnsafeMutableRawPointer) {
    let stream = Unmanaged<CFReadStream>.fromOpaque(value).takeUnretainedValue()
    CFReadStreamClose(stream)
}

@_cdecl("cf_read_stream_read")
public func cf_read_stream_read(_ value: UnsafeMutableRawPointer, _ buffer: UnsafeMutablePointer<UInt8>, _ len: Int) -> Int {
    let stream = Unmanaged<CFReadStream>.fromOpaque(value).takeUnretainedValue()
    return CFReadStreamRead(stream, buffer, len)
}

@_cdecl("cf_write_stream_open")
public func cf_write_stream_open(_ value: UnsafeMutableRawPointer) -> Bool {
    let stream = Unmanaged<CFWriteStream>.fromOpaque(value).takeUnretainedValue()
    return CFWriteStreamOpen(stream)
}

@_cdecl("cf_write_stream_close")
public func cf_write_stream_close(_ value: UnsafeMutableRawPointer) {
    let stream = Unmanaged<CFWriteStream>.fromOpaque(value).takeUnretainedValue()
    CFWriteStreamClose(stream)
}

@_cdecl("cf_write_stream_write")
public func cf_write_stream_write(_ value: UnsafeMutableRawPointer, _ buffer: UnsafePointer<UInt8>, _ len: Int) -> Int {
    let stream = Unmanaged<CFWriteStream>.fromOpaque(value).takeUnretainedValue()
    return CFWriteStreamWrite(stream, buffer, len)
}

@_cdecl("cf_socket_get_type_id")
public func cf_socket_get_type_id() -> Int {
    Int(CFSocketGetTypeID())
}

@_cdecl("cf_socket_create_udp_ipv4")
public func cf_socket_create_udp_ipv4() -> UnsafeMutableRawPointer? {
    guard let socket = CFSocketCreate(nil, PF_INET, SOCK_DGRAM, IPPROTO_UDP, 0, nil, nil) else {
        return nil
    }
    return Unmanaged.passRetained(socket).toOpaque()
}

@_cdecl("cf_socket_get_native")
public func cf_socket_get_native(_ value: UnsafeMutableRawPointer) -> Int32 {
    let socket = Unmanaged<CFSocket>.fromOpaque(value).takeUnretainedValue()
    return CFSocketGetNative(socket)
}

@_cdecl("cf_socket_invalidate")
public func cf_socket_invalidate(_ value: UnsafeMutableRawPointer) {
    let socket = Unmanaged<CFSocket>.fromOpaque(value).takeUnretainedValue()
    CFSocketInvalidate(socket)
}

@_cdecl("cf_socket_is_valid")
public func cf_socket_is_valid(_ value: UnsafeMutableRawPointer) -> Bool {
    let socket = Unmanaged<CFSocket>.fromOpaque(value).takeUnretainedValue()
    return CFSocketIsValid(socket)
}

@_cdecl("cf_file_descriptor_get_type_id")
public func cf_file_descriptor_get_type_id() -> Int {
    Int(CFFileDescriptorGetTypeID())
}

@_cdecl("cf_file_descriptor_create")
public func cf_file_descriptor_create(_ nativeFD: Int32, _ closeOnInvalidate: Bool) -> UnsafeMutableRawPointer? {
    guard let descriptor = CFFileDescriptorCreate(nil, nativeFD, closeOnInvalidate, acfNoopFileDescriptorCallback, nil) else {
        return nil
    }
    return Unmanaged.passRetained(descriptor).toOpaque()
}

@_cdecl("cf_file_descriptor_get_native")
public func cf_file_descriptor_get_native(_ value: UnsafeMutableRawPointer) -> Int32 {
    let descriptor = Unmanaged<CFFileDescriptor>.fromOpaque(value).takeUnretainedValue()
    return CFFileDescriptorGetNativeDescriptor(descriptor)
}

@_cdecl("cf_file_descriptor_invalidate")
public func cf_file_descriptor_invalidate(_ value: UnsafeMutableRawPointer) {
    let descriptor = Unmanaged<CFFileDescriptor>.fromOpaque(value).takeUnretainedValue()
    CFFileDescriptorInvalidate(descriptor)
}
