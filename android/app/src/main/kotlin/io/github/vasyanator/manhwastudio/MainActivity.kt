/*
 * File: MainActivity.kt
 *
 * Purpose:
 * The GameActivity hosting the Rust library (manifest meta-data android.app.lib_name). Its
 * super.onCreate loads libmanhwastudio_rs.so and starts `android_main` on a native thread
 * (android-activity, src/android_entry.rs).
 *
 * Notes:
 * Normally started by LaunchActivity after the bundled assets are installed. Android can also
 * re-create it directly (Recents after an APK update, process death), skipping LaunchActivity.
 * super.onCreate cannot be skipped, so the native side checks the asset stamp itself and stops
 * without touching the runtime root; this Activity then finishes and hands over to
 * LaunchActivity, which installs the assets and starts a fresh MainActivity.
 */
package io.github.vasyanator.manhwastudio

import android.content.Intent
import android.os.Bundle
import android.util.Log
import com.google.androidgamesdk.GameActivity

class MainActivity : GameActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Checked before super.onCreate starts the native thread, so the decision matches the
        // one android_main makes (nothing installs assets in between: only LaunchActivity does).
        val assetsReady = AssetInstaller.isInstalled(this)
        super.onCreate(savedInstanceState)
        if (!assetsReady) {
            Log.w(TAG, "MainActivity: bundled assets are not installed; handing over to LaunchActivity")
            startActivity(Intent(this, LaunchActivity::class.java))
            finish()
        }
    }

    private companion object {
        const val TAG = "ManhwaStudio"
    }
}
