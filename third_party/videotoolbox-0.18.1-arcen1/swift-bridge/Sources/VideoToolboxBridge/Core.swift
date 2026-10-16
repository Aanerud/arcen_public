// Core memory-management + FFI utility helpers for the VideoToolbox
// Swift bridge. Mirrors the pattern used by screencapturekit-rs.

import Foundation
import VideoToolbox

// MARK: - Status codes

/// `kVTPropertyNotSupportedErr`
public let VTB_NOT_SUPPORTED: Int32 = -11800

// MARK: - Retained-pointer helpers

/// Take a Swift reference and hand a +1 retained, opaque `void*` to Rust.
@inline(__always)
public func vtb_retain<T: AnyObject>(_ object: T) -> UnsafeMutableRawPointer {
    Unmanaged.passRetained(object).toOpaque()
}

/// Borrow a Swift reference from an opaque pointer without changing
/// the retain count.
@inline(__always)
public func vtb_borrow<T: AnyObject>(_ ptr: UnsafeMutableRawPointer) -> T {
    Unmanaged<T>.fromOpaque(ptr).takeUnretainedValue()
}

/// Drop a +1 retained Swift reference that Rust no longer owns.
@inline(__always)
public func vtb_release<T: AnyObject>(_ ptr: UnsafeMutableRawPointer, as _: T.Type) {
    Unmanaged<T>.fromOpaque(ptr).release()
}

/// Convert an `NSError` thrown from a Swift call to an `OSStatus`-style
/// `Int32` Rust expects.
@inline(__always)
public func vtb_status(from error: Error) -> Int32 {
    Int32((error as NSError).code)
}

/// Synchronously block the calling thread on an async Swift call,
/// returning a single result via an out-parameter handler.
///
/// Used to bridge `async throws` Swift APIs (motion estimation,
/// RAW processing, frame processor) into the synchronous C ABI Rust
/// expects.
public func vtb_block_on_async<T>(
    timeoutSeconds: Int = 30,
    work: @escaping () async throws -> T,
    onSuccess: @escaping (T) -> Void
) -> Int32 {
    let sem = DispatchSemaphore(value: 0)
    var status: Int32 = 0
    Task {
        do {
            let result = try await work()
            onSuccess(result)
        } catch {
            status = vtb_status(from: error)
        }
        sem.signal()
    }
    _ = sem.wait(timeout: .now() + .seconds(timeoutSeconds))
    return status
}
