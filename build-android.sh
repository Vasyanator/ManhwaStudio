#!/usr/bin/env bash
# build-android.sh — builds the ManhwaStudio Android APK (arm64-v8a) with one command.
#
# Steps: Rust cdylib via cargo-ndk -> stripped into android/app/src/main/jniLibs, bundled
# runtime files (tracked fonts and launcher backgrounds ui_new/menu_imgs; --full adds the
# extended fonts and the wiki) staged into
# android/app/src/main/assets, launcher icon staged, then the Gradle wrapper builds the APK
# (debug variant, signed with AGP's debug keystore). Everything staged is generated and
# gitignored. Details and prerequisites: android/README.md.
#
# Usage: ./build-android.sh [--release|--debug] [--full] [--install] [--logcat]
#   --release   Rust release profile for the native library (default)
#   --debug     Rust dev profile (debug info stripped, symbols kept)
#   --full      also bundle fonts/ui/ext (~77 MB) and the wiki (~41 MB)
#   --install   adb install -r the APK and launch it (uses the `adb` on PATH)
#   --logcat    after launching, follow the app's logcat (implies --install)
#
# Environment (each falls back to android/local.properties: sdk.dir, ndk.dir, java.home):
#   ANDROID_HOME (or ANDROID_SDK_ROOT)  Android SDK with platforms;android-35
#   ANDROID_NDK_HOME                     NDK (default: the newest one under $ANDROID_HOME/ndk)
#   JAVA_HOME                            JDK 17+ for Gradle (default: `java` on PATH)
set -euo pipefail

readonly APP_ID="io.github.vasyanator.manhwastudio"
readonly ABI="arm64-v8a"
readonly RUST_TARGET="aarch64-linux-android"
readonly API_LEVEL=31            # = minSdk in android/app/build.gradle.kts
readonly LIB_NAME="libmanhwastudio_rs.so"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ANDROID_DIR="$REPO/android"
MAIN_DIR="$ANDROID_DIR/app/src/main"

die() { echo "build-android.sh: error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

profile="release"; full=0; install=0; logcat=0
for arg in "$@"; do
  case "$arg" in
    --release) profile="release" ;;
    --debug) profile="debug" ;;
    --full) full=1 ;;
    --install) install=1 ;;
    --logcat) install=1; logcat=1 ;;
    -h|--help) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$arg' (see --help)" ;;
  esac
done

# --- toolchain -------------------------------------------------------------------------------
local_prop() {  # local_prop <key>: value from android/local.properties, empty when absent
  local file="$ANDROID_DIR/local.properties"
  [ -f "$file" ] || return 0
  sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" "$file" | tail -n 1 | sed 's/\\:/:/g; s/\\\\/\\/g'
}

ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$(local_prop sdk.dir)}}"
[ -n "$ANDROID_HOME" ] || die "set ANDROID_HOME or sdk.dir in android/local.properties"
[ -d "$ANDROID_HOME/platforms" ] || die "ANDROID_HOME='$ANDROID_HOME' is not an Android SDK (no platforms/)"
export ANDROID_HOME

if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  ANDROID_NDK_HOME="$(local_prop ndk.dir)"
  if [ -z "$ANDROID_NDK_HOME" ] && [ -d "$ANDROID_HOME/ndk" ]; then
    ANDROID_NDK_HOME="$(find "$ANDROID_HOME/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -n 1)"
  fi
fi
[ -n "$ANDROID_NDK_HOME" ] && [ -d "$ANDROID_NDK_HOME/toolchains/llvm" ] \
  || die "no NDK found: set ANDROID_NDK_HOME, ndk.dir in android/local.properties, or install one under \$ANDROID_HOME/ndk"
export ANDROID_NDK_HOME

if [ -z "${JAVA_HOME:-}" ]; then
  JAVA_HOME="$(local_prop java.home)"
fi
if [ -n "$JAVA_HOME" ]; then
  [ -x "$JAVA_HOME/bin/java" ] || die "JAVA_HOME='$JAVA_HOME' has no bin/java"
  export JAVA_HOME
else
  command -v java >/dev/null || die "no JDK: set JAVA_HOME (JDK 17+) or java.home in android/local.properties"
  echo "build-android.sh: warning: JAVA_HOME not set, Gradle uses '$(command -v java)'" >&2
fi

command -v cargo >/dev/null || die "cargo not found"
cargo ndk --version >/dev/null 2>&1 || die "cargo-ndk missing: cargo install cargo-ndk"
if command -v rustup >/dev/null && ! rustup target list --installed | grep -qx "$RUST_TARGET"; then
  die "Rust target missing: rustup target add $RUST_TARGET"
fi
command -v git >/dev/null && git -C "$REPO" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
  || die "the bundled-file list comes from 'git ls-files'; run this from a git checkout"

LLVM_BIN="$(find "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" -mindepth 2 -maxdepth 2 -type d -name bin | head -n 1)"
[ -x "$LLVM_BIN/llvm-strip" ] && [ -x "$LLVM_BIN/llvm-nm" ] || die "llvm-strip / llvm-nm not found in the NDK ($ANDROID_NDK_HOME)"

if command -v sha256sum >/dev/null; then sha256() { sha256sum "$@"; }; else sha256() { shasum -a 256 "$@"; }; fi

# --- native library --------------------------------------------------------------------------
# `cargo rustc --crate-type cdylib` builds the cdylib only here: the root library target stays
# an rlib, so desktop builds never link a cdylib.
info "Rust $profile build of the native library ($RUST_TARGET, API $API_LEVEL)"
cargo_profile_args=()
[ "$profile" = "release" ] && cargo_profile_args=(--release)
(cd "$REPO" && cargo ndk -t "$ABI" --platform "$API_LEVEL" rustc -p manhwastudio_rs --lib --crate-type cdylib "${cargo_profile_args[@]}")

target_dir="$(cd "$REPO" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$target_dir" ] || die "cannot read the cargo target directory from 'cargo metadata'"
built_lib="$target_dir/$RUST_TARGET/$profile/$LIB_NAME"
[ -f "$built_lib" ] || die "the build produced no '$built_lib'"

jni_dir="$MAIN_DIR/jniLibs/$ABI"
rm -rf "$MAIN_DIR/jniLibs"
mkdir -p "$jni_dir"
if [ "$profile" = "release" ]; then
  "$LLVM_BIN/llvm-strip" --strip-unneeded -o "$jni_dir/$LIB_NAME" "$built_lib"
else
  "$LLVM_BIN/llvm-strip" --strip-debug -o "$jni_dir/$LIB_NAME" "$built_lib"
fi
# The two entry points the Java side resolves by name: GameActivity's native glue and the
# android-activity application entry.
exports="$("$LLVM_BIN/llvm-nm" -D --defined-only "$jni_dir/$LIB_NAME")"
for symbol in android_main GameActivity_onCreate; do
  grep -qE "[[:space:]]$symbol\$" <<<"$exports" || die "$LIB_NAME does not export '$symbol'"
done
info "native library: $(du -h "$jni_dir/$LIB_NAME" | cut -f1) (exports android_main, GameActivity_onCreate)"

# --- bundled runtime files -------------------------------------------------------------------
# Only tracked files ship (local font groups and other private files never do), except the
# launcher backgrounds staged below. The list is what AssetInstaller copies; user documents must
# never be part of it.
assets_dir="$MAIN_DIR/assets"
rm -rf "$assets_dir"
mkdir -p "$assets_dir/ms_root"
files_list="$assets_dir/ms_root.files"
: > "$files_list"
bundle_paths=(fonts ui_new/menu_imgs)
[ "$full" = 1 ] && bundle_paths+=(wiki)
while IFS= read -r -d '' path; do
  case "$path" in
    *.md) continue ;;
    fonts/ui/ext/*) [ "$full" = 1 ] || continue ;;
  esac
  case "${path##*/}" in
    user_config.*|fonts_data.*|presets.*) die "refusing to bundle user document '$path'" ;;
  esac
  [ -f "$REPO/$path" ] || die "tracked file '$path' is missing from the working tree"
  mkdir -p "$assets_dir/ms_root/$(dirname "$path")"
  cp "$REPO/$path" "$assets_dir/ms_root/$path"
  printf '%s\n' "$path" >> "$files_list"
done < <(git -C "$REPO" ls-files -z -- "${bundle_paths[@]}")

LC_ALL=C sort -o "$files_list" "$files_list"
file_count="$(wc -l < "$files_list" | tr -d ' ')"
[ "$file_count" -gt 0 ] || die "no bundled files found"
# Stamp = content hash of the bundle: a new APK re-installs the files only when they changed.
bundle_hash="$(cd "$assets_dir/ms_root" && while IFS= read -r path; do sha256 "$path"; done < "$files_list" | sha256 | cut -c1-16)"
printf 'assets-%s\n' "$bundle_hash" > "$assets_dir/ms_root.stamp"
info "bundled files: $file_count ($(du -sh "$assets_dir/ms_root" | cut -f1)), stamp assets-$bundle_hash"

icon_dir="$MAIN_DIR/res/mipmap-xxxhdpi"
mkdir -p "$icon_dir"
cp "$REPO/app_icon_512.png" "$icon_dir/ic_launcher.png"

# --- APK -------------------------------------------------------------------------------------
info "Gradle assembleDebug"
(cd "$ANDROID_DIR" && ./gradlew --console=plain assembleDebug)
apk="$ANDROID_DIR/app/build/outputs/apk/debug/app-debug.apk"
[ -f "$apk" ] || die "Gradle produced no '$apk'"
info "APK: $apk ($(du -h "$apk" | cut -f1))"

[ "$install" = 1 ] || exit 0
command -v adb >/dev/null || die "adb not found on PATH"
info "installing on $(adb get-serialno)"
adb install -r "$apk"
[ "$logcat" = 1 ] && adb logcat -c
adb shell am start -n "$APP_ID/.LaunchActivity"
[ "$logcat" = 1 ] || exit 0
pid=""
for _ in $(seq 1 40); do
  pid="$(adb shell pidof "$APP_ID" | tr -d '\r')" && [ -n "$pid" ] && break
  sleep 0.25
done
[ -n "$pid" ] || die "the app process did not start (see 'adb logcat')"
info "logcat of pid $pid (Ctrl+C to stop)"
exec adb logcat --pid="$pid"
