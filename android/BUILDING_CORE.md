# Rebuilding the Rust core for Android

The repository ships prebuilt `libseams_core.so` binaries for `arm64-v8a` (real devices)
and `x86_64` (emulator) under `android/app/src/main/jniLibs/`. The app works without a
Rust toolchain as long as you only change Kotlin code.

Rebuild the core when you change anything under `crates/`:

1. Install Rust and the Android targets:

   ```sh
   rustup target add aarch64-linux-android x86_64-linux-android
   ```

2. Install the Android NDK (Android Studio → SDK Manager → NDK (side by side), or
   download from https://developer.android.com/ndk).

3. Build and copy the libraries (adjust NDK path and min-API wrapper to your setup):

   ```sh
   NDK="$ANDROID_HOME/ndk/<version>/toolchains/llvm/prebuilt/linux-x86_64"
   MIN_API=26

   CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/bin/aarch64-linux-android$MIN_API-clang" \
   CC_aarch64_linux_android="$NDK/bin/aarch64-linux-android$MIN_API-clang" \
   AR_aarch64_linux_android="$NDK/bin/llvm-ar" \
   cargo build --release -p seams-core --lib --target aarch64-linux-android

   CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK/bin/x86_64-linux-android$MIN_API-clang" \
   CC_x86_64_linux_android="$NDK/bin/x86_64-linux-android$MIN_API-clang" \
   AR_x86_64_linux_android="$NDK/bin/llvm-ar" \
   cargo build --release -p seams-core --lib --target x86_64-linux-android

   mkdir -p android/app/src/main/jniLibs/arm64-v8a android/app/src/main/jniLibs/x86_64
   cp target/aarch64-linux-android/release/libseams_core.so android/app/src/main/jniLibs/arm64-v8a/
   cp target/x86_64-linux-android/release/libseams_core.so android/app/src/main/jniLibs/x86_64/
   "$NDK/bin/llvm-strip" android/app/src/main/jniLibs/*/libseams_core.so
   ```

4. If the exported engine API changed, regenerate the Kotlin bindings first:

   ```sh
   cargo build -p seams-core
   cargo run --bin uniffi-bindgen -- generate \
       --library target/debug/libseams_core.so --language kotlin \
       --out-dir /tmp/seams-bindings
   cp /tmp/seams-bindings/uniffi/seams_core/seams_core.kt \
       android/app/src/main/java/uniffi/seams_core/seams_core.kt
   ```

Then build the app in Android Studio as usual (open the `android/` directory).
