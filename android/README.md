# EffectCraft for Android

The Gradle project that packages `apps/effectcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/effectcraft-android/src/lib.rs`: `android_main`, the session wiring (engine, media,
  export to Downloads, settings in the private files dir), the pickers and the JNI bridge to
  `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker (several files at
  once), saving into `Downloads/EffectCraft/`, and the full-screen window.
- `cargo ndk` drops `libeffectcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p effectcraft-android
cd android && ./gradlew assembleDebug
```

## Known limits (first version)

- No audio output: the desktop app plays previews through cpal, which is not built into this
  shell, so previews run silently (audio footage still imports and exports).
- File › Open / Import copy the picked files into the app's private `Imports/` folder first
  (the engine reads footage from the file system); large videos take their time and space.
- Save writes the project to the private `Projects/` folder under the suggested name (no
  dialog); Render Queue outputs go to `Downloads/EffectCraft/<name>` through MediaStore, and
  the same name again in one session overwrites it.
- No TCP control server / MCP, no Help ▸ Enable Logging file (logs go to logcat, tag
  `effectcraft`), no folder picker (Settings paths), no Media Browser on the file system.
- The desktop layout needs a tablet-sized screen; on a phone's cover screen it is cramped.
