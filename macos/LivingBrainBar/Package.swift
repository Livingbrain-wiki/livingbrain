// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "LivingBrainBar",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "LivingBrainBar", targets: ["LivingBrainBar"]),
        .library(name: "LivingBrainKit", targets: ["LivingBrainKit"]),
    ],
    targets: [
        // Everything that can be tested without a window server: the CLI's
        // JSON, the menu's state machine, the CLI locator and the subprocess
        // runner.
        .target(name: "LivingBrainKit"),
        // The SwiftUI shell: the menu bar extra and the AppKit glue.
        .executableTarget(name: "LivingBrainBar", dependencies: ["LivingBrainKit"]),
        .testTarget(
            name: "LivingBrainKitTests",
            dependencies: ["LivingBrainKit"],
            resources: [.copy("Fixtures")]
        ),
    ]
)
