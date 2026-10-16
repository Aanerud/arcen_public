// Aggregator stub. The AppleCFBridge target depends on the per-framework
// bridges; this file exists only so Swift PM has at least one source file
// to compile in the aggregator target.

import Foundation

/// Free a NUL-terminated heap-allocated string previously returned by any
/// bridge function in this crate. Centralised here so every per-framework
/// bridge can call into it without needing its own copy.
@_cdecl("acf_free_string")
public func acf_free_string(_ str: UnsafeMutablePointer<CChar>?) {
    guard let str = str else { return }
    free(str)
}
