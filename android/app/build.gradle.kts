// The ManhwaStudio Android application module.
//
// Inputs staged by `build-android.sh` (generated, gitignored, never edited by hand):
// - src/main/jniLibs/arm64-v8a/libmanhwastudio_rs.so — the Rust `cdylib`;
// - src/main/assets/ms_root/**, ms_root.files, ms_root.stamp — the bundled runtime files that
//   `AssetInstaller` copies into `filesDir/ms_root` (the Rust runtime root);
// - src/main/res/mipmap-xxxhdpi/ic_launcher.png — the launcher icon (from app_icon_512.png).
//
// Version: versionName is the root Cargo.toml `[package] version` (the single source of the
// program version); versionCode is derived from it as major * 1_000_000 + minor * 1_000 + patch.

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

/** Deterministic versionCode of an `X.Y.Z` version; minor and patch must stay below 1000. */
fun versionCodeOf(version: String): Int {
    val (major, minor, patch) = version.split('.').map(String::toInt)
    if (minor >= 1000 || patch >= 1000 || major >= 2000) {
        throw GradleException("version $version does not fit the versionCode scheme major*1e6 + minor*1e3 + patch")
    }
    return major * 1_000_000 + minor * 1_000 + patch
}

val msVersion = cargoPackageVersion()

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
        versionName = msVersion
        versionCode = versionCodeOf(msVersion)
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        // Only the debug variant is built for now (debug keystore of AGP). Release signing is a
        // later step: keystore outside the repository, credentials from the environment.
        getByName("debug") {
            isMinifyEnabled = false
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
}
