// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "VideoToolboxBridge",
    platforms: [
        .macOS(.v13)
    ],
    products: [
        .library(
            name: "VideoToolboxBridge",
            type: .static,
            targets: ["VideoToolboxBridge"])
    ],
    targets: [
        .target(
            name: "VideoToolboxBridge",
            path: "Sources/VideoToolboxBridge",
            publicHeadersPath: "include")
    ]
)
