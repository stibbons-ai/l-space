fn main() {
    // The iOS 27 SDK's system libraries link `@rpath/libswift*.dylib`. Xcode adds
    // `/usr/lib/swift` to every app's runpath; dx doesn't, so dyld aborts at launch on a device
    // (ADR 0007).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    }
}
