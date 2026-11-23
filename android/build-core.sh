#!/usr/bin/env bash
# Builds libseams_core.so for arm64-v8a and x86_64 and copies it into the app's
# jniLibs. See BUILDING_CORE.md for prerequisites.
set -euo pipefail

MIN_API="${MIN_API:-26}"
ABIS="${ABIS:-aarch64-linux-android x86_64-linux-android}"
declare -A JNI_DIR=( [aarch64-linux-android]=arm64-v8a [x86_64-linux-android]=x86_64 )

NDK="${NDK:?Set NDK to the prebuilt toolchains directory, e.g. \$ANDROID_HOME/ndk/<version>/toolchains/llvm/prebuilt/linux-x86_64}"

rustup target add $ABIS

for target in $ABIS; do
    CARGO_TARGET_${target^^}_LINKER="$NDK/bin/${target}-${MIN_API}-clang" \
    CC_${target}="$NDK/bin/${target}-${MIN_API}-clang" \
    AR_${target}="$NDK/bin/llvm-ar" \
    cargo build --release -p seams-core --lib --target "$target"

    dir="android/app/src/main/jniLibs/${JNI_DIR[$target]}"
    mkdir -p "$dir"
    cp "target/${target}/release/libseams_core.so" "$dir/"
    "$NDK/bin/llvm-strip" "$dir/libseams_core.so"
    echo "built ${JNI_DIR[$target]}"
done

echo "Done. Open android/ in Android Studio and build the app."
