/*
 * File: LaunchActivity.kt
 *
 * Purpose:
 * Launcher entry of the app and the receiver of incoming images (Share sheet: ACTION_SEND /
 * ACTION_SEND_MULTIPLE; "Open with": ACTION_VIEW). Makes sure the bundled runtime files are
 * installed (AssetInstaller) before the Rust library starts, copies incoming images into the
 * inbox (platform/IncomingImages), then hands over to MainActivity.
 *
 * Notes:
 * The install runs on a worker thread (a first install copies ~20-150 MB; on the UI thread
 * that would be an ANR) while this Activity shows the progress. When the bundle is already
 * installed and nothing was shared it forwards to MainActivity at once, without drawing
 * anything. On an install failure it shows the error and stays; MainActivity is not started,
 * because the native side would refuse to run on an incomplete runtime root anyway.
 *
 * Incoming images: the read grants of a shared or opened document belong to THIS Activity, so
 * the copy into the inbox runs (on the same worker, after the install) while it is alive, and
 * finish() comes only afterwards. The batch reaches Rust through the inbox, never through the
 * MainActivity intent. Leaving this Activity during the copy (BACK) cancels the batch.
 *
 * MainActivity is singleTask and always started with FLAG_ACTIVITY_NEW_TASK: from a share sheet
 * this Activity runs inside the SENDING app's task, and the editor must come up in (or bring
 * forward) its own task instead of being stacked into the sender's.
 */
package io.github.vasyanator.manhwastudio

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.TextView
import io.github.vasyanator.manhwastudio.platform.IncomingImages
import java.io.IOException

class LaunchActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var bar: ProgressBar

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A re-creation restores the Activity with its old intent; the images of that intent
        // were already received (or dropped together with the process), so only a fresh
        // creation receives.
        val incoming = savedInstanceState == null && IncomingImages.isIncoming(intent)
        if (!incoming && AssetInstaller.isInstalled(this)) {
            startMain()
            return
        }
        buildProgressView()
        val received = if (incoming) intent else null
        Thread({ prepareThenStart(received) }, "ms-launch-prepare").start()
    }

    private fun buildProgressView() {
        val pad = (24 * resources.displayMetrics.density).toInt()
        status = TextView(this).apply { gravity = Gravity.CENTER }
        bar = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { isIndeterminate = true }
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(pad, pad, pad, pad)
            addView(status)
            addView(bar, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))
        }
        setContentView(layout)
    }

    /**
     * Worker-thread body: installs the bundle when needed, receives the images of `incoming`
     * (null = nothing was shared), then starts MainActivity or shows the install error on the UI
     * thread.
     */
    private fun prepareThenStart(incoming: Intent?) {
        if (!AssetInstaller.isInstalled(this) && !install()) return
        if (incoming != null) {
            runOnUiThread { bar.isIndeterminate = true; status.text = getString(R.string.launch_receiving_start) }
            val published = IncomingImages.receive(this, incoming) { done, total ->
                runOnUiThread { showProgress(done, total, getString(R.string.launch_receiving_status, done, total)) }
            }
            // Nothing to open (empty intent, cancelled, or the inbox failed; IncomingImages
            // logged why): the app still starts, without a batch.
            if (!published) Log.i(TAG, "LaunchActivity: ${incoming.action} produced no inbox batch")
        }
        runOnUiThread { if (!isFinishing && !isDestroyed) startMain() }
    }

    /** Installs the bundled assets; false (error shown) when it failed. Worker thread only. */
    private fun install(): Boolean {
        try {
            AssetInstaller.install(applicationContext) { done, total ->
                runOnUiThread { showProgress(done, total, getString(R.string.launch_installing_status, done, total)) }
            }
        } catch (e: IOException) {
            Log.e(TAG, "LaunchActivity: installing the bundled assets failed", e)
            runOnUiThread {
                bar.isIndeterminate = false
                status.text = getString(R.string.launch_install_error, e.message ?: e.toString())
            }
            return false
        }
        return true
    }

    /** UI thread: a determinate progress step with its status line. */
    private fun showProgress(done: Int, total: Int, text: String) {
        bar.isIndeterminate = false
        bar.max = total
        bar.progress = done
        status.text = text
    }

    /** Starts (or brings forward) MainActivity in its own task, then leaves. */
    private fun startMain() {
        startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        finish()
    }

    private companion object {
        const val TAG = "ManhwaStudio"
    }
}
