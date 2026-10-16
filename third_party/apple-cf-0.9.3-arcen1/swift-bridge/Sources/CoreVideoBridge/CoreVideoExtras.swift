import CoreVideo
import Foundation
import Metal

@_cdecl("cv_metal_texture_cache_create_system_default")
public func cv_metal_texture_cache_create_system_default() -> UnsafeMutableRawPointer? {
    guard let device = MTLCreateSystemDefaultDevice() else { return nil }
    var cache: CVMetalTextureCache?
    let status = CVMetalTextureCacheCreate(nil, nil, device, nil, &cache)
    guard status == kCVReturnSuccess, let cache else { return nil }
    return Unmanaged.passRetained(cache).toOpaque()
}

@_cdecl("cv_metal_texture_cache_flush")
public func cv_metal_texture_cache_flush(_ cache: UnsafeMutableRawPointer) {
    let cache = Unmanaged<CVMetalTextureCache>.fromOpaque(cache).takeUnretainedValue()
    CVMetalTextureCacheFlush(cache, 0)
}
