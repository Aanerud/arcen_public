// swift-tools-version:5.7
import PackageDescription

let package = Package(
    name: "AppleCFBridge",
    platforms: [
        .macOS(.v12)
    ],
    products: [
        .library(
            name: "AppleCFBridge",
            type: .static,
            targets: ["AppleCFBridge"])
    ],
    targets: [
        // Aggregator target. Pulls in all per-framework bridges so that
        // build.rs only has to link one static library, but the per-framework
        // sources stay isolated by directory.
        .target(
            name: "AppleCFBridge",
            dependencies: [
                "CoreGraphicsBridge",
                "IOSurfaceBridge",
                "DispatchBridge",
                "CoreMediaBridge",
                "CoreVideoBridge",
                "CoreFoundationBridge",
            ],
            path: "Sources/AppleCFBridge",
            publicHeadersPath: "include"),
        .target(
            name: "CoreGraphicsBridge",
            path: "Sources/CoreGraphicsBridge"),
        .target(
            name: "IOSurfaceBridge",
            path: "Sources/IOSurfaceBridge"),
        .target(
            name: "DispatchBridge",
            path: "Sources/DispatchBridge"),
        .target(
            name: "CoreMediaBridge",
            path: "Sources/CoreMediaBridge"),
        .target(
            name: "CoreVideoBridge",
            path: "Sources/CoreVideoBridge"),
        .target(
            name: "CoreFoundationBridge",
            path: "Sources/CoreFoundationBridge"),
    ]
)
