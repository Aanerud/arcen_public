fn main() {
    println!("cargo:rustc-link-lib=framework=IOKit");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    // The Swift 5.7 SDK of the last Xcode for macOS 12 links the Swift runtime
    // as `@rpath/libswiftCore.dylib`. A dependency's `rustc-link-arg` (apple-cf
    // sets this rpath) does not reach this package's binaries and test
    // executables, so a native Intel build would die in dyld without it. Newer
    // SDKs link `/usr/lib/swift` directly, where the extra rpath is harmless.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
