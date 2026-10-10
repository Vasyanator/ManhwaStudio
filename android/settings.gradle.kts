// Gradle settings of the ManhwaStudio Android app: the `:app` module plus the
// `:winit-secondary-window` library module that lives in the winit fork's source.
//
// The native library, the bundled assets and the launcher icon are NOT produced by Gradle:
// `build-android.sh` in the repository root stages them into `app/src/main/` before it runs
// Gradle (see android/README.md). All repositories are declared here, centrally.

pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

// Rust crates that ship an Android (Gradle) component inside their source are located through
// `cargo metadata`, so the component always matches the crate Cargo resolved: a crates.io
// release, a git checkout or a local `[patch]` path alike. One query serves every lookup.
val cargoPackages: List<Map<String, Any?>> by lazy {
    val repoRoot = settingsDir.parentFile
    val metadata = providers.exec {
        workingDir = repoRoot
        commandLine(
            "cargo", "metadata", "--format-version", "1",
            "--filter-platform", "aarch64-linux-android",
            "--manifest-path", File(repoRoot, "Cargo.toml").path,
        )
    }.standardOutput.asText.get()
    @Suppress("UNCHECKED_CAST")
    (groovy.json.JsonSlurper().parseText(metadata) as Map<String, Any?>)["packages"] as List<Map<String, Any?>>
}

/** Source directory of the resolved Cargo package [name]; [purpose] names it in the error. */
fun cargoPackageDir(name: String, purpose: String): File {
    val manifestPaths = cargoPackages.filter { it["name"] == name }.map { it["manifest_path"] as String }.distinct()
    val manifestPath = manifestPaths.singleOrNull()
        ?: throw GradleException(
            "cargo metadata lists ${manifestPaths.size} $name packages (expected exactly one); " +
                "$purpose cannot be found",
        )
    return File(manifestPath).parentFile
}

// The Kotlin half of `rustls-platform-verifier` ships as a local Maven repository INSIDE the
// `rustls-platform-verifier-android` crate source, exactly as that crate's README prescribes.
fun rustlsPlatformVerifierMavenDir(): File =
    File(cargoPackageDir("rustls-platform-verifier-android", "the Android TLS verifier component"), "maven")

// The Kotlin half of winit's secondary windows (`rs.winit.android.SecondaryWindowActivity`) is a
// Gradle library module INSIDE the winit fork's source (`android/winit-secondary-window`).
fun winitSecondaryWindowModuleDir(): File =
    File(cargoPackageDir("winit", "the winit secondary-window Android module"), "android/winit-secondary-window")

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
        maven {
            url = uri(rustlsPlatformVerifierMavenDir())
            metadataSources.artifact()
        }
    }
}

rootProject.name = "ManhwaStudio"
include(":app")
include(":winit-secondary-window")
project(":winit-secondary-window").projectDir = winitSecondaryWindowModuleDir()
