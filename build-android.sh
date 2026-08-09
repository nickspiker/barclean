#!/bin/bash
# Build the Rust core for arm64, then assemble and install the APK.
#
# ANDROID_HOME defaults to ~/android-sdk, which is where this machine keeps it — NOT the macOS
# default ~/Library/Android/sdk. Override the env var if that ever changes.
set -e

export ANDROID_HOME="${ANDROID_HOME:-$HOME/android-sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/25.2.9519653}"
export PATH="$ANDROID_HOME/platform-tools:$PATH"

# 16 KB LOAD-segment alignment. Android 15+ devices (the Pixel 8 Pro this is tested on) use 16 KB
# pages and reject or warn on 4 KB-aligned libs. cargo-ndk sets its own RUSTFLAGS and overrides
# .cargo/config, so the flag has to ride this env var to survive.
ALIGN="-C link-arg=-Wl,-z,max-page-size=16384"

echo "==> building Rust core (arm64-v8a)"
RUSTFLAGS="-A warnings $ALIGN" cargo ndk -t arm64-v8a -o android/app/libs build --release

echo "==> assembling APK"
cd android
./gradlew assembleDebug

APK=app/build/outputs/apk/debug/app-debug.apk
echo "==> APK at android/$APK"

if [ "$1" = "install" ]; then
    echo "==> installing"
    adb install -r "$APK"
    adb shell am start -n com.barclean/.BarcleanActivity
fi
