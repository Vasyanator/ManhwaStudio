// Gradle settings of the ManhwaStudio Android app (one module, `:app`).
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

// The Kotlin half of `rustls-platform-verifier` ships as a local Maven repository INSIDE the
// `rustls-platform-verifier-android` crate source. Its location is asked from Cargo, exactly as
// that crate's README prescribes, so the AAR always matches the crate version in Cargo.lock.
fun rustlsPlatformVerifierMavenDir(): File {
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
    val packages = (groovy.json.JsonSlurper().parseText(metadata) as Map<String, Any?>)["packages"] as List<Map<String, Any?>>
    val manifestPath = packages.firstOrNull { it["name"] == "rustls-platform-verifier-android" }?.get("manifest_path") as String?
        ?: throw GradleException(
            "cargo metadata lists no rustls-platform-verifier-android package; " +
                "the Android TLS verifier component cannot be found",
        )
    return File(File(manifestPath).parentFile, "maven")
}

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
