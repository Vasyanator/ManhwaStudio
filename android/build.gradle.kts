// Root build of the ManhwaStudio Android app: only pins the Android Gradle Plugin version.
// AGP 9 compiles Kotlin itself (built-in Kotlin), so no separate Kotlin plugin is applied.
plugins {
    id("com.android.application") version "9.4.1" apply false
}
