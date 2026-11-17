#!/bin/sh
# Builds the mobile core for the iOS app (see crates/shellrs-mobile/README.md):
#
#   target/mobile/ios/ShellAsCore.xcframework  the library, for iPhones and for
#                                              the simulator on Apple silicon
#   target/mobile/ios/Sources/ShellAsCore.swift  its Swift interface
#
# Needs Xcode and `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`.
# IPHONEOS_DEPLOYMENT_TARGET sets the oldest iOS it runs on, 15.0 unless set.
set -eu
cd "$(dirname "$0")/.."
out=target/mobile/ios
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-15.0}"

for target in aarch64-apple-ios aarch64-apple-ios-sim; do
    cargo build --locked --release -p shellrs-mobile --lib --target "$target"
done

# The interface is read from what the library says about itself, in a build
# for this computer.
cargo build --locked -p shellrs-mobile --features bindgen
rm -rf "$out"
mkdir -p "$out/Sources" "$out/include/ShellAsCoreFFI"
target/debug/uniffi-bindgen generate --no-format --language swift \
    --library target/debug/libshellrs_mobile.dylib --out-dir "$out/generated"
mv "$out/generated/ShellAsCore.swift" "$out/Sources/"
mv "$out/generated/ShellAsCoreFFI.h" "$out/include/ShellAsCoreFFI/"
# In a folder of its own, so the module map does not clash with another
# library's in the app's build folder.
mv "$out/generated/ShellAsCoreFFI.modulemap" "$out/include/ShellAsCoreFFI/module.modulemap"
rm -r "$out/generated"

xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/libshellrs_mobile.a -headers "$out/include" \
    -library target/aarch64-apple-ios-sim/release/libshellrs_mobile.a -headers "$out/include" \
    -output "$out/ShellAsCore.xcframework"
