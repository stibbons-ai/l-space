fn main() {
    // The iOS 27 SDK's CoreMedia.tbd links `@rpath/libswiftCoreMedia.dylib`. Xcode adds
    // `/usr/lib/swift` to every app's runpath; dx doesn't, so dyld aborts at launch on the device.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    }
}
