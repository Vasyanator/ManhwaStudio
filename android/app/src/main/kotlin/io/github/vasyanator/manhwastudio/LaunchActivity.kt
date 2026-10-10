/*
 * File: LaunchActivity.kt
 *
 * Purpose:
 * Launcher entry of the app. Makes sure the bundled runtime files are installed
 * (AssetInstaller) before the Rust library starts, then hands over to MainActivity.
 *
 * Notes:
 * The install runs on a worker thread (a first install copies ~20-150 MB; on the UI thread
 * that would be an ANR) while this Activity shows the progress. When the bundle is already
 * installed it forwards to MainActivity at once, without drawing anything. On failure it shows
 * the error and stays; MainActivity is not started, because the native side would refuse to
 * run on an incomplete runtime root anyway.
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
import java.io.IOException

class LaunchActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var bar: ProgressBar

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (AssetInstaller.isInstalled(this)) {
            startMain()
            return
        }
        buildProgressView()
        Thread({ installThenStart() }, "ms-asset-install").start()
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

    /** Worker-thread body: installs, then starts MainActivity or shows the error on the UI thread. */
    private fun installThenStart() {
        try {
            AssetInstaller.install(applicationContext) { done, total ->
                runOnUiThread {
                    bar.isIndeterminate = false
                    bar.max = total
                    bar.progress = done
                    status.text = getString(R.string.launch_installing_status, done, total)
                }
            }
        } catch (e: IOException) {
            Log.e(TAG, "LaunchActivity: installing the bundled assets failed", e)
            runOnUiThread {
                bar.isIndeterminate = false
                status.text = getString(R.string.launch_install_error, e.message ?: e.toString())
            }
            return
        }
        runOnUiThread { if (!isFinishing && !isDestroyed) startMain() }
    }

    private fun startMain() {
        startActivity(Intent(this, MainActivity::class.java))
        finish()
    }

    private companion object {
        const val TAG = "ManhwaStudio"
    }
}
