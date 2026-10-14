/*
 * File: MainActivity.kt
 *
 * Purpose:
 * The GameActivity hosting the Rust library (manifest meta-data android.app.lib_name). Its
 * super.onCreate loads libmanhwastudio_rs.so and starts `android_main` on a native thread
 * (android-activity, src/android_entry.rs).
 *
 * Key functions:
 * - onNewIntent(): records the new intent only (see "Incoming images").
 * - dispatchKeyEvent(): sends physical-keyboard keys straight to the native key path.
 * - isPhysicalKeyboardKey(): which key events take that route.
 *
 * Notes:
 * Normally started by LaunchActivity after the bundled assets are installed. Android can also
 * re-create it directly (Recents after an APK update, process death), skipping LaunchActivity.
 * super.onCreate cannot be skipped, so the native side checks the asset stamp itself and stops
 * without touching the runtime root; this Activity then finishes and hands over to
 * LaunchActivity, which installs the assets and starts a fresh MainActivity.
 *
 * Incoming images: the manifest makes this Activity singleTask, so a share or "Open with" that
 * arrives while the app runs reuses this instance (onNewIntent) in its own task instead of
 * starting a second GameActivity (android-activity supports one per process). Activities above
 * it in its task (an open SAF picker) are cleared; the picker then reports a cancel. The images
 * never travel in this Activity's intent: LaunchActivity copies them into the inbox first and
 * Rust takes them from there (platform/IncomingImages, crates/ms-platform/src/incoming/).
 * winit's secondary-window Activities live in their own tasks (taskAffinity "") and are not
 * affected by singleTask.
 *
 * Keyboard routing: GameActivity's GameTextInput InputConnection is the OnKeyListener of the
 * surface view. While the soft input is active (a focused text field) it consumes every
 * printable key into its own Java text buffer, which Rust only receives as IME text snapshots.
 * Physical keys must instead reach Rust as key events (layout-aware characters, repeat,
 * modifiers, shortcuts), exactly as when no field is focused, so dispatchKeyEvent hands them to
 * onKeyDown / onKeyUp (-> GameActivity.onKeyDownNative / onKeyUpNative) without passing the view
 * hierarchy. Soft-keyboard text keeps going through GameTextInput (the winit fork turns its
 * snapshots into IME events), so no key is delivered twice. Contract:
 * crates/winit/src/platform_impl/android/MODULE_README.md (the "Soft keyboard (IME)" contract).
 */
package io.github.vasyanator.manhwastudio

import android.content.Intent
import android.os.Bundle
import android.util.Log
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
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

    /**
     * A singleTask re-launch (LaunchActivity after a share or "Open with"): only remembers the
     * intent. The batch is already in the inbox and the running Rust app was woken through
     * IncomingImages.nativeOnIncoming.
     */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
    }

    /**
     * Physical-keyboard keys go to [onKeyDown] / [onKeyUp] (GameActivity's native key path)
     * directly, the way Activity.dispatchKeyEvent does after the view hierarchy declined them,
     * so GameTextInput's key listener never sees them. Every other key event (soft keyboard,
     * injected or other virtual devices, ACTION_MULTIPLE) takes the normal route.
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (!isPhysicalKeyboardKey(event)) {
            return super.dispatchKeyEvent(event)
        }
        onUserInteraction()
        return event.dispatch(this, window.decorView.keyDispatcherState, this)
    }

    private companion object {
        const val TAG = "ManhwaStudio"

        /**
         * A DOWN or UP key event from a real (non-virtual) keyboard device. Soft-keyboard events
         * come from KeyCharacterMap.VIRTUAL_KEYBOARD and/or carry FLAG_SOFT_KEYBOARD; a mouse's
         * BACK / FORWARD buttons have no keyboard source and keep the default route.
         */
        fun isPhysicalKeyboardKey(event: KeyEvent): Boolean {
            if (event.action != KeyEvent.ACTION_DOWN && event.action != KeyEvent.ACTION_UP) {
                return false
            }
            if (event.deviceId == KeyCharacterMap.VIRTUAL_KEYBOARD ||
                (event.flags and KeyEvent.FLAG_SOFT_KEYBOARD) != 0 ||
                !event.isFromSource(InputDevice.SOURCE_KEYBOARD)
            ) {
                return false
            }
            val device = event.device ?: return false
            return !device.isVirtual
        }
    }
}
