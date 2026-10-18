// The ManhwaStudio Android application module.
//
// Inputs staged by `build-android.sh` (generated, gitignored, never edited by hand):
// - src/main/jniLibs/arm64-v8a/libmanhwastudio_rs.so — the Rust `cdylib`;
// - src/main/assets/ms_root/**, ms_root.files, ms_root.stamp — the bundled runtime files that
//   `AssetInstaller` copies into `filesDir/ms_root` (the Rust runtime root);
// - src/main/res/mipmap-xxxhdpi/ic_launcher.png — the launcher icon (from app_icon_512.png).
//
// Version: the root Cargo.toml `[package] version` is the single source of the program version.
// A release writes it as `N.0.0`, N = the commit count of the release commit, so versionName is
// the plain `N` for such a version and the full `X.Y.Z` otherwise; versionCode is
// major * 10_000 + minor * 100 + patch (minor and patch below 100), monotonic in the version.
//
// Signing: the debug variant uses AGP's debug keystore. The release variant is signed with the
// release keystore named by the environment (MS_ANDROID_KEYSTORE, MS_ANDROID_KEYSTORE_PASSWORD,
// MS_ANDROID_KEY_ALIAS, MS_ANDROID_KEY_PASSWORD; build-android.sh --release exports them).
// Credentials never come from a tracked file and are never logged; when one is missing, only
// release tasks fail (`checkReleaseSigning`), debug builds are unaffected.

plugins {
    id("com.android.application")
}

/** `[package] version` of the root Cargo.toml, without a pre-release / build suffix. */
fun cargoPackageVersion(): String {
    val manifest = rootProject.file("../Cargo.toml").readText()
    val packageSection = manifest.substringAfter("[package]").substringBefore("\n[")
    val match = Regex("""(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)[^"]*"""").find(packageSection)
        ?: throw GradleException("no [package] version = \"X.Y.Z\" in the root Cargo.toml")
    return match.groupValues.drop(1).joinToString(".")
}

/** The three numeric parts of an `X.Y.Z` version from [cargoPackageVersion]. */
fun versionParts(version: String): Triple<Int, Int, Int> {
    val (major, minor, patch) = version.split('.').map(String::toInt)
    return Triple(major, minor, patch)
}

/**
 * Deterministic versionCode of an `X.Y.Z` version: major * 10_000 + minor * 100 + patch.
 * Minor and patch must stay below 100 (otherwise two versions could share a code) and the code
 * must stay within Google Play's 2_100_000_000 limit; a version outside the scheme fails the build.
 */
fun versionCodeOf(version: String): Int {
    val (major, minor, patch) = versionParts(version)
    if (minor >= 100 || patch >= 100 || major >= 210_000) {
        throw GradleException(
            "version $version does not fit the versionCode scheme major * 10000 + minor * 100 + patch " +
                "(minor and patch must be below 100, major below 210000)"
        )
    }
    return major * 10_000 + minor * 100 + patch
}

/** versionName shown to the user: `N` for a release version `N.0.0`, else the full `X.Y.Z`. */
fun versionNameOf(version: String): String {
    val (major, minor, patch) = versionParts(version)
    return if (minor == 0 && patch == 0) major.toString() else version
}

val msVersion = cargoPackageVersion()

/** Environment variables that carry the release signing credentials (values never logged). */
val releaseSigningVariables = listOf(
    "MS_ANDROID_KEYSTORE",
    "MS_ANDROID_KEYSTORE_PASSWORD",
    "MS_ANDROID_KEY_ALIAS",
    "MS_ANDROID_KEY_PASSWORD",
)
// An unset or empty variable counts as missing.
val releaseSigningValues: Map<String, String> = releaseSigningVariables
    .mapNotNull { name -> providers.environmentVariable(name).orNull?.takeIf(String::isNotEmpty)?.let { name to it } }
    .toMap()
val missingReleaseSigningVariables = releaseSigningVariables.filterNot(releaseSigningValues::containsKey)
val releaseKeystoreFile: File? = releaseSigningValues["MS_ANDROID_KEYSTORE"]?.let(::file)

android {
    namespace = "io.github.vasyanator.manhwastudio"
    // The newest platform the toolchain setup installs; targetSdk follows it (a targetSdk above
    // compileSdk would promise behaviour the build cannot compile against).
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.vasyanator.manhwastudio"
        // Android 12: the floor the Rust library is built for (`cargo ndk --platform 31`).
        minSdk = 31
        targetSdk = 35
        versionName = versionNameOf(msVersion)
        versionCode = versionCodeOf(msVersion)
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    signingConfigs {
        // Created only when every credential is present; otherwise the release variant stays
        // unsigned and `checkReleaseSigning` stops any release build before it gets that far.
        if (missingReleaseSigningVariables.isEmpty()) {
            create("release") {
                storeFile = releaseKeystoreFile
                storePassword = releaseSigningValues.getValue("MS_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = releaseSigningValues.getValue("MS_ANDROID_KEY_ALIAS")
                keyPassword = releaseSigningValues.getValue("MS_ANDROID_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        // Debug variant: AGP's debug keystore.
        getByName("debug") {
            isMinifyEnabled = false
        }
        // Release variant: the distributed APK. No minification or resource shrinking: the Rust
        // side reaches GameActivity, the winit fork's Kotlin classes and the platform bridges
        // through JNI by name, which R8 cannot see.
        getByName("release") {
            isMinifyEnabled = false
            isShrinkResources = false
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs {
            // Compressed in the APK and extracted on install: the library is ~100 MB raw.
            useLegacyPackaging = true
            // build-android.sh strips the library itself; AGP would otherwise need the NDK.
            keepDebugSymbols += "**/*.so"
            // The onnxruntime AAR's Java API binding: the Rust side loads libonnxruntime.so by
            // path and never uses the Java API, so its JNI library is dead weight.
            excludes += "**/libonnxruntime4j_jni.so"
        }
    }

    androidResources {
        // AGP's default pattern drops dot-files and `_`-prefixed directories from assets; the
        // bundled runtime tree must arrive complete, so only VCS / OS junk is ignored.
        ignoreAssetsPattern = "!.svn:!.git:!.ds_store:!*.scc:!CVS:!thumbs.db:!picasa.ini:!*~"
    }
}

dependencies {
    // The Java half of android-activity's GameActivity backend: android-activity 0.6.1 is
    // written against exactly this release (its README, "Game Activity Library Version").
    implementation("androidx.games:games-activity:4.4.0")
    // GameActivity extends AppCompatActivity but its POM declares no dependencies.
    implementation("androidx.appcompat:appcompat:1.7.1")
    // Kotlin component of rustls-platform-verifier (local Maven repo, see settings.gradle.kts).
    implementation("rustls:rustls-platform-verifier:latest.release")
    // Kotlin half of winit's secondary windows (module in the winit fork, see settings.gradle.kts).
    implementation(project(":winit-secondary-window"))
    // The official ONNX Runtime for Android (CPU + XNNPACK EPs): its jni/arm64-v8a/
    // libonnxruntime.so lands in the package's nativeLibraryDir, where the native ONNX runtime
    // loads it by path (the `bundled` rows of crates/ms-onnx-runtime/src/ort_manifest.json).
    // The version must equal those rows' `version` (a host test in ms-onnx-runtime checks it)
    // and stay >= 1.18, the floor of the pinned `ort` crate.
    implementation("com.microsoft.onnxruntime:onnxruntime-android:1.27.0")
}

// Fails a release build early, with the names of the missing signing variables (never their
// values), or when the keystore file does not exist. Wired only into the release variant's first
// task, so debug builds never run it.
val checkReleaseSigning = tasks.register("checkReleaseSigning") {
    val missing = missingReleaseSigningVariables
    val keystorePath = releaseKeystoreFile?.path
    val keystoreExists = releaseKeystoreFile?.isFile ?: false
    doLast {
        if (missing.isNotEmpty()) {
            throw GradleException(
                "release signing is not configured: set ${missing.joinToString(", ")} " +
                    "(see android/README.md, \"Release builds\")"
            )
        }
        if (!keystoreExists) {
            throw GradleException("release keystore MS_ANDROID_KEYSTORE='$keystorePath' does not exist")
        }
    }
}
tasks.matching { it.name == "preReleaseBuild" }.configureEach {
    dependsOn(checkReleaseSigning)
}
