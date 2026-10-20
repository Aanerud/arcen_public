// SPDX-License-Identifier: AGPL-3.0-only
//
// Arcen real-screen driver (macOS). Drives the real Arcen Deck window with
// real CGEvent input and watches the result with ScreenCaptureKit.
//
// It writes three files into --out:
//   meta.json    window/capture geometry, clock anchors, permission status
//   sends.jsonl  every posted input event: monotonic time, global point, phase
//   frames.jsonl every captured frame: display time, decoded page code strip
//                (page frame counter + last pointer position the host page
//                had seen), and black/white cursor blobs found in the frame
// Analysis lives in realscreen.py. Build: see docs/testing/realscreen.md.

import AppKit
import ApplicationServices
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import QuartzCore
import ScreenCaptureKit

// MARK: - arguments

struct Options {
    var pid: pid_t = 0
    var out = "realscreen-out"
    var phases = ["focus", "calib", "jump", "hover", "draw", "stress"]
    var device = "mouse"  // mouse | pen
    var rate = 120.0
    var jumps = 16
    var hoverSeconds = 5.0
    var drawSeconds = 8.0
    var stressSeconds = 8.0
    var waitStream = 120.0
    var lockPath = "/tmp/arcen-mac-screen.lock"
    var createLock = false
    var snapshots = true
    var escapeFirst = false
    var tilt = false  // pen: vary tiltX/tiltY (normalized -1...1) during strokes
    var stripWait: Double? = nil  // seconds to wait for the latency page's strip (default --wait-stream)
    var region = (0.10, 0.45, 0.90, 0.75)  // stylus phase: canvas box as window fractions x0,y0,x1,y1
    var findCanvas = false  // stylus phase: locate (and scroll to) the page's dark canvas first
}

func parseOptions() -> Options {
    var o = Options()
    var it = CommandLine.arguments.dropFirst().makeIterator()
    while let a = it.next() {
        func v() -> String { it.next() ?? "" }
        switch a {
        case "--pid": o.pid = pid_t(v()) ?? 0
        case "--out": o.out = v()
        case "--phases": o.phases = v().split(separator: ",").map(String.init)
        case "--device": o.device = v()
        case "--rate": o.rate = Double(v()) ?? 120
        case "--jumps": o.jumps = Int(v()) ?? 16
        case "--hover-seconds": o.hoverSeconds = Double(v()) ?? 5
        case "--draw-seconds": o.drawSeconds = Double(v()) ?? 8
        case "--stress-seconds": o.stressSeconds = Double(v()) ?? 8
        case "--wait-stream": o.waitStream = Double(v()) ?? 120
        case "--lock": o.lockPath = v()
        case "--create-lock": o.createLock = true
        case "--no-snapshots": o.snapshots = false
        case "--escape-first": o.escapeFirst = true
        case "--tilt": o.tilt = true
        case "--find-canvas": o.findCanvas = true
        case "--strip-wait": o.stripWait = Double(v())
        case "--region":
            let f = v().split(separator: ",").compactMap { Double($0) }
            if f.count == 4 { o.region = (f[0], f[1], f[2], f[3]) }
        case "--probe":
            print(
                "{\"accessibility\":\(AXIsProcessTrusted()),\"post_event\":\(CGPreflightPostEventAccess()),\"screen_capture\":\(CGPreflightScreenCaptureAccess())}"
            )
            exit(0)
        default:
            FileHandle.standardError.write("unknown argument \(a)\n".data(using: .utf8)!)
            exit(2)
        }
    }
    return o
}

var opts = parseOptions()
guard opts.pid > 0 else {
    FileHandle.standardError.write("usage: realscreen-driver --pid <deck pid> --out <dir> [...]\n".data(using: .utf8)!)
    exit(2)
}
try? FileManager.default.createDirectory(atPath: opts.out, withIntermediateDirectories: true)

func log(_ s: String) {
    FileHandle.standardError.write("[driver \(String(format: "%.3f", CACurrentMediaTime()))] \(s)\n".data(using: .utf8)!)
}

/// Blocking sleep; the driver is a straight-line script and wants real time.
func pause(_ seconds: Double) { Thread.sleep(forTimeInterval: seconds) }

// MARK: - clocks

var timebase = mach_timebase_info_data_t()
mach_timebase_info(&timebase)
func machToSeconds(_ t: UInt64) -> Double { Double(t) * Double(timebase.numer) / Double(timebase.denom) / 1e9 }

/// (monotonic seconds, unix seconds) with the tightest bracket of 50 samples.
func clockAnchor() -> (Double, Double, Double) {
    var best = (0.0, 0.0, 1e9)
    for _ in 0..<50 {
        let a = CACurrentMediaTime()
        let w = Date().timeIntervalSince1970
        let b = CACurrentMediaTime()
        if b - a < best.2 { best = ((a + b) / 2, w, b - a) }
    }
    return best
}

// MARK: - screen lock (shared with other agents/operators)

var lockFd: Int32 = -1
var lockState = "absent"
if FileManager.default.fileExists(atPath: opts.lockPath) {
    lockFd = open(opts.lockPath, O_RDONLY)
    if lockFd >= 0 {
        let start = Date()
        while flock(lockFd, LOCK_EX | LOCK_NB) != 0 {
            if Date().timeIntervalSince(start) > 900 { log("screen lock busy for 15 min; giving up"); exit(5) }
            pause(1)
        }
        lockState = "held"
    }
} else if opts.createLock {
    lockFd = open(opts.lockPath, O_RDONLY | O_CREAT, 0o666)
    if lockFd >= 0 && flock(lockFd, LOCK_EX | LOCK_NB) == 0 { lockState = "created+held" }
}
log("screen lock \(opts.lockPath): \(lockState)")

// MARK: - Deck window

struct WindowInfo { let id: CGWindowID; let frame: CGRect }

func deckWindow() -> WindowInfo? {
    guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]]
    else { return nil }
    var best: WindowInfo?
    for w in list {
        guard (w[kCGWindowOwnerPID as String] as? Int).map({ pid_t($0) }) == opts.pid,
            (w[kCGWindowLayer as String] as? Int) == 0,
            let b = w[kCGWindowBounds as String] as? [String: Double],
            let id = w[kCGWindowNumber as String] as? Int
        else { continue }
        let r = CGRect(x: b["X"]!, y: b["Y"]!, width: b["Width"]!, height: b["Height"]!)
        if r.width < 200 || r.height < 200 { continue }
        if best == nil || r.width * r.height > best!.frame.width * best!.frame.height {
            best = WindowInfo(id: CGWindowID(id), frame: r)
        }
    }
    return best
}

// MARK: - frame analysis

struct Blob: Codable { var x0: Int; var y0: Int; var x1: Int; var y1: Int; var n: Int; var white: Int }
struct FrameRecord: Codable {
    var t: Double  // display time (monotonic seconds) or arrival time if absent
    var arrival: Double
    var ok: Bool
    var counter: Int
    var x: Int
    var y: Int
    var blobs: [Blob]
}

final class Analyzer {
    var stripBox: (Int, Int, Int, Int)? = nil  // left, top, right, bottom in capture px
    var stripMisses = 0

    @inline(__always) func px(_ base: UnsafePointer<UInt8>, _ bpr: Int, _ x: Int, _ y: Int) -> (Int, Int, Int) {
        let p = base + y * bpr + x * 4
        return (Int(p[2]), Int(p[1]), Int(p[0]))  // BGRA -> r,g,b
    }

    func isMagenta(_ c: (Int, Int, Int)) -> Bool { c.0 > 190 && c.2 > 190 && c.1 < 80 }

    func findStrip(_ base: UnsafePointer<UInt8>, _ bpr: Int, _ w: Int, _ h: Int) -> (Int, Int, Int, Int)? {
        var minX = Int.max, minY = Int.max, maxX = -1, maxY = -1
        let sw = min(w, w * 6 / 10), sh = min(h, max(60, h / 6))
        for y in 0..<sh {
            for x in 0..<sw where isMagenta(px(base, bpr, x, y)) {
                minX = min(minX, x); maxX = max(maxX, x); minY = min(minY, y); maxY = max(maxY, y)
            }
        }
        if maxX < 0 || maxX - minX < 27 * 3 || maxY - minY < 4 { return nil }
        return (minX, minY, maxX, maxY)
    }

    func decode(_ base: UnsafePointer<UInt8>, _ bpr: Int, _ box: (Int, Int, Int, Int)) -> (Int, Int, Int)? {
        let (l, t, r, b) = box
        let cw = Double(r - l + 1) / 27.0, ch = Double(b - t + 1) / 2.0
        var bits: [Int] = []
        bits.reserveCapacity(50)
        for row in 0..<2 {
            // The end cells must still be magenta, or the box moved.
            let cy = Int(Double(t) + (Double(row) + 0.5) * ch)
            if !isMagenta(px(base, bpr, Int(Double(l) + 0.5 * cw), cy)) { return nil }
            if !isMagenta(px(base, bpr, Int(Double(l) + 26.5 * cw), cy)) { return nil }
            for c in 0..<25 {
                let cx = Int(Double(l) + (Double(c) + 1.5) * cw)
                var sum = 0
                for dy in -1...1 { for dx in -1...1 { let p = px(base, bpr, cx + dx, cy + dy); sum += p.0 + p.1 + p.2 } }
                bits.append(sum / 27 > 128 ? 1 : 0)
            }
        }
        func word(_ from: Int, _ n: Int) -> Int { bits[from..<(from + n)].reduce(0) { $0 << 1 | $1 } }
        let f = word(0, 16), x = word(16, 13), y = word(29, 13), chk = word(42, 8)
        if (f + x * 3 + y * 7) & 0xff != chk { return nil }
        return (f, x, y)
    }

    func blobs(_ base: UnsafePointer<UInt8>, _ bpr: Int, _ w: Int, _ h: Int) -> [Blob] {
        let g = 6
        let gw = (w + g - 1) / g, gh = (h + g - 1) / g
        var cells = [Int32](repeating: 0, count: gw * gh)
        var whites = [Int32](repeating: 0, count: gw * gh)
        // Exact extremes per cell, so blob bounds are pixel-precise.
        var cminX = [Int32](repeating: Int32.max, count: gw * gh)
        var cminY = [Int32](repeating: Int32.max, count: gw * gh)
        var cmaxX = [Int32](repeating: -1, count: gw * gh)
        var cmaxY = [Int32](repeating: -1, count: gw * gh)
        var excl = (0, 0, -1, -1)
        if let s = stripBox { excl = (s.0 - 6, s.1 - 6, s.2 + 6, s.3 + 6) }
        var y = 0
        while y < h {
            var x = 0
            while x < w {
                if !(x >= excl.0 && x <= excl.2 && y >= excl.1 && y <= excl.3) {
                    let p = px(base, bpr, x, y)
                    let white = p.0 > 215 && p.1 > 215 && p.2 > 215
                    let black = p.0 < 40 && p.1 < 40 && p.2 < 40
                    if white || black {
                        let i = (y / g) * gw + x / g
                        cells[i] += 1
                        if white { whites[i] += 1 }
                        cminX[i] = min(cminX[i], Int32(x)); cmaxX[i] = max(cmaxX[i], Int32(x))
                        cminY[i] = min(cminY[i], Int32(y)); cmaxY[i] = max(cmaxY[i], Int32(y))
                    }
                }
                x += 2
            }
            y += 2
        }
        var seen = [Bool](repeating: false, count: gw * gh)
        var out: [Blob] = []
        for start in 0..<(gw * gh) where cells[start] > 0 && !seen[start] {
            var stack = [start]
            seen[start] = true
            var b = Blob(x0: Int.max, y0: Int.max, x1: -1, y1: -1, n: 0, white: 0)
            while let i = stack.popLast() {
                let cx = i % gw, cy = i / gw
                b.x0 = min(b.x0, Int(cminX[i])); b.y0 = min(b.y0, Int(cminY[i]))
                b.x1 = max(b.x1, Int(cmaxX[i])); b.y1 = max(b.y1, Int(cmaxY[i]))
                b.n += Int(cells[i]); b.white += Int(whites[i])
                for dy in -1...1 {
                    for dx in -1...1 {
                        let nx = cx + dx, ny = cy + dy
                        if nx < 0 || ny < 0 || nx >= gw || ny >= gh { continue }
                        let j = ny * gw + nx
                        if cells[j] > 0 && !seen[j] { seen[j] = true; stack.append(j) }
                    }
                }
            }
            // A cursor is small and mixes both colours; big areas are content.
            let bw = b.x1 - b.x0, bh = b.y1 - b.y0
            if b.n >= 5 && bw < 120 && bh < 120 { out.append(b) }
        }
        out.sort { $0.n > $1.n }
        return Array(out.prefix(4))
    }

    func analyze(_ pb: CVPixelBuffer) -> (Bool, Int, Int, Int, [Blob]) {
        CVPixelBufferLockBaseAddress(pb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let raw = CVPixelBufferGetBaseAddress(pb) else { return (false, 0, 0, 0, []) }
        let base = UnsafePointer<UInt8>(raw.assumingMemoryBound(to: UInt8.self))
        let bpr = CVPixelBufferGetBytesPerRow(pb), w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
        var decoded: (Int, Int, Int)? = nil
        if let box = stripBox { decoded = decode(base, bpr, box) }
        if decoded == nil {
            stripBox = findStrip(base, bpr, w, h)
            if let box = stripBox { decoded = decode(base, bpr, box) }
        }
        let bl = blobs(base, bpr, w, h)
        if let d = decoded { return (true, d.0, d.1, d.2, bl) }
        return (false, 0, 0, 0, bl)
    }
}

// MARK: - capture

final class Capture: NSObject, SCStreamOutput, SCStreamDelegate {
    let analyzer = Analyzer()
    let lock = NSLock()
    var frames: [FrameRecord] = []
    var firstOk: Double? = nil
    var snapshotRequests: [String] = []
    var captureSize = (0, 0)
    var stopped: Error? = nil
    var latest: CVPixelBuffer? = nil

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, let pb = CMSampleBufferGetImageBuffer(sb) else { return }
        let arrival = CACurrentMediaTime()
        var t = arrival
        if let atts = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
            let a = atts.first
        {
            if let s = a[.status] as? Int, s != SCFrameStatus.complete.rawValue { return }
            if let d = a[.displayTime] as? UInt64 { t = machToSeconds(d) }
        }
        let (ok, counter, x, y, blobs) = analyzer.analyze(pb)
        lock.lock()
        frames.append(FrameRecord(t: t, arrival: arrival, ok: ok, counter: counter, x: x, y: y, blobs: blobs))
        if ok && firstOk == nil { firstOk = t }
        latest = pb
        let snap = snapshotRequests.popLast()
        lock.unlock()
        if let name = snap { saveSnapshot(pb, name) }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        log("capture stopped: \(error)")
        stopped = error
    }

    func allFrames() -> [FrameRecord] { lock.withLock { frames } }

    /// The largest near-black rectangle in the newest frame (a drawing
    /// canvas on a light page), in capture pixels: rows count when 35–97% of
    /// their pixels are near-black, and the longest run of such rows wins.
    func darkBox() -> CGRect? {
        guard let pb = lock.withLock({ latest }) else { return nil }
        CVPixelBufferLockBaseAddress(pb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let raw = CVPixelBufferGetBaseAddress(pb) else { return nil }
        let base = raw.assumingMemoryBound(to: UInt8.self)
        let bpr = CVPixelBufferGetBytesPerRow(pb), w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
        func dark(_ x: Int, _ y: Int) -> Bool {
            let p = base + y * bpr + x * 4
            return p[0] < 24 && p[1] < 24 && p[2] < 24
        }
        var best = (0, 0), run = (-1, 0)
        for y in stride(from: 0, to: h, by: 2) {
            var n = 0
            for x in stride(from: 0, to: w, by: 4) where dark(x, y) { n += 1 }
            let share = Double(n) / Double(w / 4)
            if share > 0.35 && share < 0.97 {
                if run.0 < 0 { run = (y, y) } else { run.1 = y }
                if run.1 - run.0 > best.1 - best.0 { best = run }
            } else {
                run = (-1, 0)
            }
        }
        if best.1 - best.0 < 40 { return nil }
        let mid = (best.0 + best.1) / 2
        var x0 = w, x1 = 0
        for x in 0..<w where dark(x, mid) { x0 = min(x0, x); x1 = max(x1, x) }
        return x1 > x0 ? CGRect(x: x0, y: best.0, width: x1 - x0, height: best.1 - best.0) : nil
    }

    func requestSnapshot(_ name: String) {
        guard opts.snapshots else { return }
        lock.lock(); snapshotRequests.append(name); lock.unlock()
    }

    func saveSnapshot(_ pb: CVPixelBuffer, _ name: String) {
        let ci = CIImage(cvPixelBuffer: pb)
        let ctx = CIContext()
        guard let cg = ctx.createCGImage(ci, from: ci.extent) else { return }
        let rep = NSBitmapImageRep(cgImage: cg)
        try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: "\(opts.out)/\(name).png"))
    }
}

// MARK: - input

let source = CGEventSource(stateID: .hidSystemState)
struct Send: Codable { var t: Double; var t1: Double; var phase: String; var kind: String; var x: Double; var y: Double; var p: Double }
var sends: [Send] = []

func post(_ e: CGEvent, _ phase: String, _ kind: String, _ pt: CGPoint, _ pressure: Double = 0) {
    let t = CACurrentMediaTime()
    e.post(tap: .cghidEventTap)
    sends.append(Send(t: t, t1: CACurrentMediaTime(), phase: phase, kind: kind, x: pt.x, y: pt.y, p: pressure))
}

func mouse(_ type: CGEventType, _ pt: CGPoint, _ phase: String, _ kind: String) {
    guard let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: pt, mouseButton: .left) else { return }
    post(e, phase, kind, pt)
}

let penDevice: Int64 = 0x4152  // arbitrary synthetic tablet device id

func pen(
    _ type: CGEventType, _ pt: CGPoint, _ pressure: Double, _ phase: String, _ kind: String,
    tilt: (Double, Double) = (0, 0), tip: Bool? = nil
) {
    guard let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: pt, mouseButton: .left) else { return }
    // The subtype selects the tablet-point layout and clears its fields, so
    // every tablet field (tilt included) is set after it.
    e.setIntegerValueField(.mouseEventSubtype, value: Int64(CGEventMouseSubtype.tabletPoint.rawValue))
    e.setDoubleValueField(.tabletEventTiltX, value: tilt.0)
    e.setDoubleValueField(.tabletEventTiltY, value: tilt.1)
    e.setDoubleValueField(.mouseEventPressure, value: pressure)
    e.setDoubleValueField(.tabletEventPointPressure, value: pressure)
    e.setIntegerValueField(.tabletEventDeviceID, value: penDevice)
    e.setIntegerValueField(.tabletEventPointX, value: Int64(pt.x))
    e.setIntegerValueField(.tabletEventPointY, value: Int64(pt.y))
    // The tip bit is contact; a tip down may carry no pressure at all.
    e.setIntegerValueField(.tabletEventPointButtons, value: (tip ?? (pressure > 0)) ? 1 : 0)
    post(e, phase, kind, pt, pressure)
}

func penProximity(_ enter: Bool, _ pt: CGPoint, _ phase: String) {
    func fill(_ e: CGEvent) {
        e.setIntegerValueField(.tabletProximityEventEnterProximity, value: enter ? 1 : 0)
        e.setIntegerValueField(.tabletProximityEventPointerType, value: 1)  // pen
        e.setIntegerValueField(.tabletProximityEventDeviceID, value: penDevice)
        e.setIntegerValueField(.tabletProximityEventSystemTabletID, value: 1)
        e.setIntegerValueField(.tabletProximityEventPointerID, value: 1)
        e.setIntegerValueField(.tabletProximityEventCapabilityMask, value: 0x0647)  // xy, buttons, pressure, tilt
    }
    if let e = CGEvent(mouseEventSource: source, mouseType: .mouseMoved, mouseCursorPosition: pt, mouseButton: .left) {
        e.setIntegerValueField(.mouseEventSubtype, value: Int64(CGEventMouseSubtype.tabletProximity.rawValue))
        fill(e)
        post(e, phase, enter ? "prox_in" : "prox_out", pt)
    }
    if let e = CGEvent(source: source) {
        e.type = .tabletProximity
        e.location = pt
        fill(e)
        e.post(tap: .cghidEventTap)
    }
}

func key(_ code: CGKeyCode, _ phase: String) {
    for down in [true, false] {
        if let e = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down) {
            post(e, phase, down ? "key_down" : "key_up", .zero)
        }
        pause(0.04)
    }
}

func sleepUntil(_ t: Double) {
    let d = t - CACurrentMediaTime()
    if d > 0 { pause(d) }
}

// MARK: - main

let anchorStart = clockAnchor()
log("waiting for Deck window of pid \(opts.pid)")
var win: WindowInfo? = nil
let windowDeadline = CACurrentMediaTime() + opts.waitStream
while win == nil && CACurrentMediaTime() < windowDeadline {
    win = deckWindow()
    if win == nil { pause(0.5) }
}
guard var window = win else { log("no Deck window"); exit(3) }
NSRunningApplication(processIdentifier: opts.pid)?.activate(options: [])
pause(1.0)
window = deckWindow() ?? window
log("Deck window \(window.id) frame \(window.frame)")

let capture = Capture()
let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
let centre = CGPoint(x: window.frame.midX, y: window.frame.midY)
guard let display = content.displays.first(where: { $0.frame.contains(centre) }) ?? content.displays.first else {
    log("no display"); exit(4)
}
let mode = CGDisplayCopyDisplayMode(display.displayID)
let refresh = mode?.refreshRate ?? 0
let filter = SCContentFilter(display: display, excludingWindows: [])
let config = SCStreamConfiguration()
let local = CGRect(
    x: window.frame.minX - display.frame.minX, y: window.frame.minY - display.frame.minY,
    width: window.frame.width, height: window.frame.height)
config.sourceRect = local
config.width = Int(window.frame.width)
config.height = Int(window.frame.height)
config.showsCursor = true
config.pixelFormat = kCVPixelFormatType_32BGRA
config.minimumFrameInterval = CMTime(value: 1, timescale: 240)
config.queueDepth = 8
let stream = SCStream(filter: filter, configuration: config, delegate: capture)
try stream.addStreamOutput(capture, type: .screen, sampleHandlerQueue: DispatchQueue(label: "capture", qos: .userInteractive))
try await stream.startCapture()
log("capturing \(config.width)x\(config.height) from display \(display.displayID) @\(refresh) Hz")

// Wait for the host page's code strip to show up in the Deck window.
let streamDeadline = CACurrentMediaTime() + (opts.stripWait ?? opts.waitStream)
while capture.firstOk == nil && CACurrentMediaTime() < streamDeadline { pause(0.2) }
if capture.firstOk == nil {
    log("page code strip never decoded")
    capture.requestSnapshot("no-strip")
    pause(0.5)
}
capture.requestSnapshot("start")

let F = window.frame
func at(_ fx: Double, _ fy: Double) -> CGPoint { CGPoint(x: F.minX + F.width * fx, y: F.minY + F.height * fy) }
let period = 1.0 / opts.rate
var phaseMarks: [[String: Double]] = []
func mark(_ name: String) -> Double { let t = CACurrentMediaTime(); phaseMarks.append([name: t]); return t }

func circle(_ phase: String, seconds: Double, button: Bool) {
    let c = at(0.36, 0.58)
    let r = F.height * 0.20
    let t0 = mark(phase)
    let usePen = opts.device == "pen"
    let start = CGPoint(x: c.x + r, y: c.y)
    if usePen { penProximity(true, start, phase) }
    if button {
        if usePen { pen(.leftMouseDown, start, 0.6, phase, "down") } else { mouse(.leftMouseDown, start, phase, "down") }
    } else {
        if usePen { pen(.mouseMoved, start, 0, phase, "move") } else { mouse(.mouseMoved, start, phase, "move") }
    }
    let n = Int(seconds * opts.rate)
    for i in 1...n {
        sleepUntil(t0 + Double(i) * period)
        let a = Double(i) * period / 1.6 * 2 * Double.pi
        let p = CGPoint(x: c.x + r * cos(a), y: c.y + r * sin(a))
        let pressure = button ? 0.5 + 0.3 * sin(a * 3) : 0
        let tilt = opts.tilt ? (0.5 * sin(a), 0.4 * cos(a)) : (0.0, 0.0)
        if usePen {
            pen(button ? .leftMouseDragged : .mouseMoved, p, pressure, phase, "move", tilt: tilt)
        } else {
            mouse(button ? .leftMouseDragged : .mouseMoved, p, phase, "move")
        }
        if i == n / 2 { capture.requestSnapshot(phase) }
    }
    let end = sends.last.map { CGPoint(x: $0.x, y: $0.y) } ?? start
    if button {
        if usePen { pen(.leftMouseUp, end, 0, phase, "up") } else { mouse(.leftMouseUp, end, phase, "up") }
    }
    if usePen { penProximity(false, end, phase) }
    pause(0.6)
}

/// One stroke from `a` to `b` (fractions of the stylus region) at `opts.rate`,
/// with pressure and tilt given per progress `u` in 0...1. `wave` adds a
/// vertical sine of that amplitude (region fraction) for a fast wavy stroke.
func stroke(
    _ phase: String, _ a: (Double, Double), _ b: (Double, Double), seconds: Double, touch: Bool,
    wave: Double = 0, snapshot: String? = nil,
    pressure: (Double) -> Double, tilt: (Double) -> (Double, Double)
) {
    let (x0, y0, x1, y1) = opts.region
    func pt(_ fx: Double, _ fy: Double) -> CGPoint { at(x0 + (x1 - x0) * fx, y0 + (y1 - y0) * fy) }
    let usePen = opts.device == "pen"
    let start = pt(a.0, a.1)
    if usePen { penProximity(true, start, phase) }
    if usePen { pen(.mouseMoved, start, 0, phase, "move", tilt: tilt(0)) } else { mouse(.mouseMoved, start, phase, "move") }
    pause(0.05)
    if touch {
        if usePen { pen(.leftMouseDown, start, pressure(0), phase, "down", tilt: tilt(0), tip: true) } else { mouse(.leftMouseDown, start, phase, "down") }
    }
    let n = max(2, Int(seconds * opts.rate))
    let t0 = CACurrentMediaTime()
    var last = start
    for i in 1...n {
        sleepUntil(t0 + Double(i) / opts.rate)
        let u = Double(i) / Double(n)
        let fy = a.1 + (b.1 - a.1) * u + wave * sin(u * 2 * Double.pi * 6)
        last = pt(a.0 + (b.0 - a.0) * u, fy)
        let type: CGEventType = touch ? .leftMouseDragged : .mouseMoved
        if usePen { pen(type, last, touch ? pressure(u) : 0, phase, "move", tilt: tilt(u), tip: touch) } else { mouse(type, last, phase, "move") }
        if i == n / 2, let s = snapshot { capture.requestSnapshot(s) }
    }
    if touch {
        if usePen { pen(.leftMouseUp, last, 0, phase, "up", tilt: tilt(1), tip: false) } else { mouse(.leftMouseUp, last, phase, "up") }
    }
    pause(0.15)
    if usePen { penProximity(false, last, phase) }
    pause(0.4)
}

for phase in opts.phases {
    log("phase \(phase)")
    switch phase {
    case "focus":
        _ = mark(phase)
        let p = at(0.40, 0.60)
        mouse(.mouseMoved, p, phase, "move")
        pause(0.2)
        if opts.escapeFirst { key(53, phase); pause(0.6) }
        mouse(.leftMouseDown, p, phase, "down")
        pause(0.05)
        mouse(.leftMouseUp, p, phase, "up")
        pause(0.8)
    case "calib":
        _ = mark(phase)
        for fy in [0.35, 0.58, 0.82] {
            for fx in [0.12, 0.34, 0.58] {
                mouse(.mouseMoved, at(fx, fy), phase, "move")
                pause(0.45)
            }
        }
    case "jump":
        _ = mark(phase)
        // With --device pen the steps are pen hover moves, so host-side
        // smoothing shows as several samples converging on the target.
        let usePen = opts.device == "pen"
        if usePen { penProximity(true, at(0.56, 0.62), phase); pause(0.3) }
        for i in 0..<opts.jumps {
            let target = at(i % 2 == 0 ? 0.14 : 0.56, 0.62)
            if usePen { pen(.mouseMoved, target, 0, phase, "jump") } else { mouse(.mouseMoved, target, phase, "jump") }
            pause(0.5)
        }
        if usePen { penProximity(false, at(0.56, 0.62), phase); pause(0.3) }
    case "hover":
        circle(phase, seconds: opts.hoverSeconds, button: false)
    case "draw":
        circle(phase, seconds: opts.drawSeconds, button: true)
    case "stylus":
        // A drawing session on a real stylus test page: hover, a pressure ramp,
        // a tilted stroke, then a fast wavy stroke with pressure and tilt varying.
        _ = mark(phase)
        let noTilt: (Double) -> (Double, Double) = { _ in (0, 0) }
        if opts.findCanvas {
            // Find the page's canvas (a large near-black box), hover it once so
            // the page shows its readouts, then scroll until it is in full view.
            let cw = Double(config.width), chh = Double(config.height)
            func toRegion(_ b: CGRect) -> (Double, Double, Double, Double) {
                (b.minX / cw + 0.03, b.minY / chh + 0.02, b.maxX / cw - 0.03, b.maxY / chh - 0.02)
            }
            if opts.escapeFirst { key(53, phase); pause(0.8) }  // a fresh GNOME login opens the overview
            // Keyboard focus for the arrow keys: click the page's empty left margin.
            let margin = at(0.03, 0.55)
            mouse(.mouseMoved, margin, phase, "move")
            pause(0.2)
            mouse(.leftMouseDown, margin, phase, "down")
            pause(0.05)
            mouse(.leftMouseUp, margin, phase, "up")
            pause(0.4)
            func locate(hover: Bool) {
                for attempt in 0..<10 {
                    pause(0.3)
                    let box = capture.darkBox()
                    log("canvas search \(attempt): \(String(describing: box))")
                    if let b = box {
                        if (hover && attempt == 0) || b.height < 0.25 * chh {
                            opts.region = toRegion(b)
                            stroke(phase, (0.1, 0.5), (0.9, 0.5), seconds: 0.6, touch: false, pressure: { _ in 0 }, tilt: noTilt)
                        }
                        if b.maxY < chh - 8 && b.height > 0.25 * chh {
                            opts.region = toRegion(b)
                            return
                        }
                    }
                    // Arrow-down scrolls the page a line at a time in every browser.
                    for _ in 0..<3 { key(125, phase) }
                    pause(0.6)
                }
            }
            locate(hover: true)
            // A page may change its layout on the first contact (a "pen detected"
            // banner replacing a hint): tap once in a corner, then locate again.
            stroke(phase, (0.0, 0.0), (0.02, 0.0), seconds: 0.1, touch: true, pressure: { _ in 0.5 }, tilt: noTilt)
            pause(0.6)
            locate(hover: false)
            log("stylus region \(opts.region)")
            capture.requestSnapshot("stylus-located")
            pause(0.5)
        }
        stroke(phase, (0.05, 0.08), (0.95, 0.08), seconds: 1.5, touch: false, snapshot: "stylus-hover",
               pressure: { _ in 0 }, tilt: noTilt)
        // Taps: the tip down for 60 ms with no pressure at all, then a light one.
        for (i, p) in [0.0, 0.0, 0.0, 0.01, 0.01].enumerated() {
            let fx = 0.05 + Double(i) * 0.05
            stroke(phase, (fx, 0.18), (fx, 0.18), seconds: 0.06, touch: true, pressure: { _ in p }, tilt: noTilt)
        }
        capture.requestSnapshot("stylus-taps")
        // A light stroke: pressure 1 %, under libinput's default contact threshold.
        stroke(phase, (0.35, 0.18), (0.95, 0.18), seconds: 1.5, touch: true, snapshot: "stylus-light",
               pressure: { _ in 0.01 }, tilt: noTilt)
        stroke(phase, (0.05, 0.32), (0.95, 0.32), seconds: 2.5, touch: true, snapshot: "stylus-ramp",
               pressure: { u in 0.02 + 0.98 * u }, tilt: noTilt)
        stroke(phase, (0.05, 0.50), (0.95, 0.50), seconds: 2.0, touch: true, snapshot: "stylus-tilt",
               pressure: { _ in 0.5 }, tilt: { u in opts.tilt ? (0.6 - 1.2 * u, -0.4 + 0.8 * u) : (0, 0) })
        stroke(phase, (0.05, 0.78), (0.95, 0.78), seconds: 2.0, touch: true, wave: 0.12, snapshot: "stylus-wave",
               pressure: { u in 0.3 + 0.6 * abs(sin(u * 9)) },
               tilt: { u in opts.tilt ? (0.5 * sin(u * 7), 0.5 * cos(u * 5)) : (0, 0) })
        pause(1.0)
        capture.requestSnapshot("stylus-final")
        pause(0.5)
    case "stress":
        _ = mark(phase)
        key(1, phase)  // 'S' toggles the page's moving band
        let c = at(0.30, 0.55)
        let t0 = CACurrentMediaTime()
        // Slow hover so the cursor stays visible while the band moves.
        var i = 0
        while CACurrentMediaTime() - t0 < opts.stressSeconds {
            i += 1
            sleepUntil(t0 + Double(i) * (1.0 / 60))
            let a = Double(i) / 60 / 3 * 2 * Double.pi
            mouse(.mouseMoved, CGPoint(x: c.x + 80 * cos(a), y: c.y + 80 * sin(a)), phase, "move")
            if i == 120 { capture.requestSnapshot("stress") }
        }
        key(1, phase)
        pause(0.5)
    default:
        log("unknown phase \(phase)")
    }
}
_ = mark("end")
pause(1.0)
try? await stream.stopCapture()
let anchorEnd = clockAnchor()

// MARK: - output

let enc = JSONEncoder()
func writeLines<T: Encodable>(_ items: [T], _ name: String) {
    var data = Data()
    for i in items { data.append(try! enc.encode(i)); data.append(0x0a) }
    try? data.write(to: URL(fileURLWithPath: "\(opts.out)/\(name)"))
}
let frames = capture.allFrames()
writeLines(sends, "sends.jsonl")
writeLines(frames, "frames.jsonl")
let meta: [String: Any] = [
    "deck_pid": Int(opts.pid),
    "window": ["x": F.minX, "y": F.minY, "w": F.width, "h": F.height, "id": Int(window.id)],
    "capture": ["w": config.width, "h": config.height, "display": Int(display.displayID), "refresh_hz": refresh,
                "display_frame": ["x": display.frame.minX, "y": display.frame.minY, "w": display.frame.width, "h": display.frame.height]],
    "strip_box": capture.analyzer.stripBox.map { [$0.0, $0.1, $0.2, $0.3] } ?? [],
    "device": opts.device, "rate_hz": opts.rate, "phases": phaseMarks,
    "clock_anchor_start": ["mono": anchorStart.0, "unix": anchorStart.1, "err": anchorStart.2],
    "clock_anchor_end": ["mono": anchorEnd.0, "unix": anchorEnd.1, "err": anchorEnd.2],
    "permissions": ["accessibility": AXIsProcessTrusted(), "post_event": CGPreflightPostEventAccess(),
                    "screen_capture": CGPreflightScreenCaptureAccess()],
    "screen_lock": ["path": opts.lockPath, "state": lockState],
    "frames": frames.count, "frames_decoded": frames.filter { $0.ok }.count, "sends": sends.count,
]
let metaData = try JSONSerialization.data(withJSONObject: meta, options: [.prettyPrinted, .sortedKeys])
try metaData.write(to: URL(fileURLWithPath: "\(opts.out)/meta.json"))
log("done: \(frames.count) frames (\(frames.filter { $0.ok }.count) decoded), \(sends.count) sends")
exit(0)
