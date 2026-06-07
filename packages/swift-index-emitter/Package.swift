// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "swift-index-emitter",
    platforms: [.macOS(.v10_15)],
    dependencies: [
        .package(
            url: "https://github.com/apple/swift-syntax.git",
            from: "510.0.0"
        ),
    ],
    targets: [
        .executableTarget(
            name: "swift-index-emitter",
            dependencies: [
                .product(name: "SwiftSyntax", package: "swift-syntax"),
                .product(name: "SwiftParser", package: "swift-syntax"),
            ],
            path: "Sources"
        ),
    ]
)
