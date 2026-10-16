use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

// Arcen patch: the upstream build ran `swift build` on a SwiftPM package
// declaring tools 5.9 and macOS 13. That excludes macOS 12, whose newest
// supported toolchain is Xcode 14.2 (Swift 5.7). The bridge sources are
// compiled here as one static module with `swiftc`, for the deployment target
// Cargo builds for (MACOSX_DEPLOYMENT_TARGET, default 12.0). The exported
// `@_cdecl` symbols are unchanged.
const DEFAULT_DEPLOYMENT_TARGET: &str = "12.0";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    if env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=IOSurface");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=CoreMedia");
    println!("cargo:rustc-link-lib=framework=CoreVideo");
    println!("cargo:rustc-link-lib=framework=Metal");

    let swift_dir = Path::new("swift-bridge");
    println!("cargo:rerun-if-changed={}", swift_dir.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let swift_build_dir = out_dir.join("swift-build");
    std::fs::create_dir_all(&swift_build_dir).expect("create Swift build directory");

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let swift_arch = match target_arch.as_str() {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        other => panic!("apple-cf: unsupported target arch '{other}'. Expected x86_64 or aarch64."),
    };
    let deployment_target = env::var("MACOSX_DEPLOYMENT_TARGET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DEPLOYMENT_TARGET.to_owned());
    let swift_target = format!("{swift_arch}-apple-macosx{}", deployment_target.trim());

    let mut sources = Vec::new();
    collect_swift_sources(&swift_dir.join("Sources"), &mut sources);
    sources.sort();
    assert!(!sources.is_empty(), "apple-cf: no Swift bridge sources found");

    let sdk_path = xcrun_output(&["--sdk", "macosx", "--show-sdk-path"]);
    let library = swift_build_dir.join("libAppleCFBridge.a");
    let output = Command::new("xcrun")
        .args(["swiftc", "-O", "-whole-module-optimization", "-parse-as-library"])
        // The bridge uses no regex APIs. Without this, Swift 5.7 auto-links
        // libswift_StringProcessing, which macOS 12 does not ship.
        .args([
            "-Xfrontend",
            "-disable-implicit-string-processing-module-import",
        ])
        .args(["-module-name", "AppleCFBridge"])
        .args(["-target", &swift_target])
        .args(["-sdk", &sdk_path])
        .args(["-emit-library", "-static"])
        .arg("-o")
        .arg(&library)
        .args(&sources)
        .output()
        .expect("Failed to run swiftc for the Swift bridge");

    if !output.status.success() {
        eprintln!(
            "Swift build STDOUT:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "Swift build STDERR:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        panic!(
            "Swift build failed with exit code: {:?}",
            output.status.code()
        );
    }

    link_swift_bridge(&swift_build_dir, &sdk_path);
}

// Only real directories and regular files belong to the bridge. The entry's
// own file type is used, which never follows a symbolic link, so a link cannot
// pull a directory or a Swift file from outside the governed tree into the
// build.
fn collect_swift_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("apple-cf: cannot read {}: {error}", directory.display()));
    for entry in entries {
        let entry = entry.expect("read Swift bridge directory entry");
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("apple-cf: cannot stat {}: {error}", path.display()));
        if file_type.is_symlink() {
            panic!(
                "apple-cf: refusing symbolic link in the Swift bridge sources: {}",
                path.display()
            );
        } else if file_type.is_dir() {
            collect_swift_sources(&path, sources);
        } else if file_type.is_file()
            && path.extension().is_some_and(|extension| extension == "swift")
        {
            sources.push(path);
        }
    }
}

fn xcrun_output(args: &[&str]) -> String {
    let output = Command::new("xcrun")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("apple-cf: xcrun {args:?} failed: {error}"));
    assert!(
        output.status.success(),
        "apple-cf: xcrun {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn link_swift_bridge(swift_build_dir: &Path, sdk_path: &str) {
    println!("cargo:rustc-link-search=native={}", swift_build_dir.display());
    println!("cargo:rustc-link-lib=static=AppleCFBridge");

    // The bridge auto-links the OS Swift runtime and overlays (libswiftCore
    // and friends); their link stubs live in the SDK, not on the default
    // linker search path.
    println!("cargo:rustc-link-search=native={sdk_path}/usr/lib/swift");

    println!("cargo:rustc-link-lib=framework=Foundation");

    // Swift back-deployment compatibility archives that the bridge objects
    // auto-link live beside the toolchain's swiftc.
    let swiftc = PathBuf::from(xcrun_output(&["--find", "swiftc"]));
    if let Some(toolchain_usr) = swiftc.parent().and_then(Path::parent) {
        let swift_lib = toolchain_usr.join("lib/swift/macosx");
        println!("cargo:rustc-link-search=native={}", swift_lib.display());
    }

    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    if let Ok(output) = Command::new("xcode-select").arg("-p").output() {
        if output.status.success() {
            let xcode_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let swift_lib_path =
                format!("{xcode_path}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{swift_lib_path}");
        }
    }
}
