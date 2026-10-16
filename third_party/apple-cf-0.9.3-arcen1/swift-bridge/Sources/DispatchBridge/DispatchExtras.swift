import Foundation

@_cdecl("acf_dispatch_async_f")
public func acf_dispatch_async_f(
    _ queue: UnsafeMutableRawPointer,
    _ context: UnsafeMutableRawPointer?,
    _ work: (@convention(c) (UnsafeMutableRawPointer?) -> Void)?
) {
    guard let work else { return }
    let queue = Unmanaged<DispatchQueue>.fromOpaque(queue).takeUnretainedValue()
    queue.async {
        work(context)
    }
}

@_cdecl("acf_dispatch_async_and_wait_f")
public func acf_dispatch_async_and_wait_f(
    _ queue: UnsafeMutableRawPointer,
    _ context: UnsafeMutableRawPointer?,
    _ work: (@convention(c) (UnsafeMutableRawPointer?) -> Void)?
) {
    guard let work else { return }
    let queue = Unmanaged<DispatchQueue>.fromOpaque(queue).takeUnretainedValue()
    queue.asyncAndWait(execute: DispatchWorkItem {
        work(context)
    })
}

@_cdecl("acf_dispatch_apply_f")
public func acf_dispatch_apply_f(
    _ iterations: Int,
    _ queue: UnsafeMutableRawPointer,
    _ context: UnsafeMutableRawPointer?,
    _ work: (@convention(c) (Int, UnsafeMutableRawPointer?) -> Void)?
) {
    guard let work, iterations > 0 else { return }
    let queue = Unmanaged<DispatchQueue>.fromOpaque(queue).takeUnretainedValue()
    let group = DispatchGroup()
    for iteration in 0..<iterations {
        group.enter()
        queue.async {
            work(iteration, context)
            group.leave()
        }
    }
    group.wait()
}

private func dispatchDeadline(timeoutMs: Int64) -> DispatchTime {
    if timeoutMs < 0 {
        return .distantFuture
    }
    return .now() + .milliseconds(Int(timeoutMs))
}

final class DispatchSourceTimerHolder {
    let source: DispatchSourceTimer
    private(set) var fireCount: UInt64 = 0

    init(intervalMs: UInt64, leewayMs: UInt64) {
        let queue = DispatchQueue(label: "com.doomfish.apple-cf.dispatch-source")
        source = DispatchSource.makeTimerSource(queue: queue)
        source.setEventHandler { [weak self] in
            self?.fireCount += 1
        }
        source.schedule(
            deadline: .now() + .milliseconds(Int(intervalMs)),
            repeating: .milliseconds(Int(intervalMs)),
            leeway: .milliseconds(Int(leewayMs))
        )
    }
}

@_cdecl("acf_dispatch_group_create")
public func acf_dispatch_group_create() -> UnsafeMutableRawPointer {
    return Unmanaged.passRetained(DispatchGroup()).toOpaque()
}

@_cdecl("acf_dispatch_group_enter")
public func acf_dispatch_group_enter(_ group: UnsafeMutableRawPointer) {
    let group = Unmanaged<DispatchGroup>.fromOpaque(group).takeUnretainedValue()
    group.enter()
}

@_cdecl("acf_dispatch_group_leave")
public func acf_dispatch_group_leave(_ group: UnsafeMutableRawPointer) {
    let group = Unmanaged<DispatchGroup>.fromOpaque(group).takeUnretainedValue()
    group.leave()
}

@_cdecl("acf_dispatch_group_wait")
public func acf_dispatch_group_wait(_ group: UnsafeMutableRawPointer, _ timeoutMs: Int64) -> Bool {
    let group = Unmanaged<DispatchGroup>.fromOpaque(group).takeUnretainedValue()
    return group.wait(timeout: dispatchDeadline(timeoutMs: timeoutMs)) == .success
}

@_cdecl("acf_dispatch_semaphore_create")
public func acf_dispatch_semaphore_create(_ value: Int64) -> UnsafeMutableRawPointer {
    return Unmanaged.passRetained(DispatchSemaphore(value: Int(value))).toOpaque()
}

@_cdecl("acf_dispatch_semaphore_signal")
public func acf_dispatch_semaphore_signal(_ semaphore: UnsafeMutableRawPointer) -> Int64 {
    let semaphore = Unmanaged<DispatchSemaphore>.fromOpaque(semaphore).takeUnretainedValue()
    return Int64(semaphore.signal())
}

@_cdecl("acf_dispatch_semaphore_wait")
public func acf_dispatch_semaphore_wait(_ semaphore: UnsafeMutableRawPointer, _ timeoutMs: Int64) -> Bool {
    let semaphore = Unmanaged<DispatchSemaphore>.fromOpaque(semaphore).takeUnretainedValue()
    return semaphore.wait(timeout: dispatchDeadline(timeoutMs: timeoutMs)) == .success
}

@_cdecl("acf_dispatch_source_timer_create")
public func acf_dispatch_source_timer_create(_ intervalMs: UInt64, _ leewayMs: UInt64) -> UnsafeMutableRawPointer {
    return Unmanaged.passRetained(DispatchSourceTimerHolder(intervalMs: intervalMs, leewayMs: leewayMs)).toOpaque()
}

@_cdecl("acf_dispatch_source_timer_resume")
public func acf_dispatch_source_timer_resume(_ source: UnsafeMutableRawPointer) {
    let source = Unmanaged<DispatchSourceTimerHolder>.fromOpaque(source).takeUnretainedValue()
    source.source.resume()
}

@_cdecl("acf_dispatch_source_timer_cancel")
public func acf_dispatch_source_timer_cancel(_ source: UnsafeMutableRawPointer) {
    let source = Unmanaged<DispatchSourceTimerHolder>.fromOpaque(source).takeUnretainedValue()
    source.source.cancel()
}

@_cdecl("acf_dispatch_source_timer_fire_count")
public func acf_dispatch_source_timer_fire_count(_ source: UnsafeMutableRawPointer) -> UInt64 {
    let source = Unmanaged<DispatchSourceTimerHolder>.fromOpaque(source).takeUnretainedValue()
    return source.fireCount
}
