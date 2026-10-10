/*
 * File: SafRequestActivity.kt
 *
 * Purpose:
 * Invisible (translucent) helper Activity that shows ONE Storage Access Framework picker for
 * SafBridge and reports its result. Started by SafBridge.launch on the UI thread over the window
 * that asked; finishes itself as soon as the result is reported.
 *
 * Key functions:
 * - intent(): the start intent of one request (id, kind, MIME types, suggested name).
 * - onCreate(): registers the AndroidX Activity Result contract of the kind and launches it.
 *
 * Why a helper Activity instead of registering on the requesting Activity: the request comes
 * from a native thread at any moment, the requesting window may be the GameActivity or a
 * winit-fork secondary window Activity, and the Activity Result API must be registered in
 * onCreate to survive a re-creation. A dedicated ComponentActivity gets all three for free:
 * its registration in onCreate is restored by the ActivityResultRegistry when the system
 * destroys it while the picker is open ("Don't keep activities", memory pressure), and no
 * other Activity needs bridge code. configChanges keeps it alive across window resizes.
 *
 * Notes:
 * Exactly one report per request (SafBridge contract): the picker result, or a cancel when the
 * Activity finishes without one. Display-name queries and grant persistence run on the bridge
 * executor; the Activity stays alive (it holds the transient URI grants) until they are done.
 * After a process death the re-created Activity finds no live request and finishes silently.
 */
package io.github.vasyanator.manhwastudio.platform

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts

class SafRequestActivity : ComponentActivity() {
    private var requestId = 0L
    private var kind = -1

    /** True once this request was reported (or needs no report): blocks a second report. */
    private var settled = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        requestId = intent.getLongExtra(EXTRA_REQUEST_ID, 0L)
        kind = intent.getIntExtra(EXTRA_KIND, -1)
        if (!SafBridge.onRequestActivityCreated(requestId)) {
            // Re-created after a process death (no Rust worker waits) or already answered.
            settled = true
            finish()
            return
        }
        val mimeTypes = intent.getStringArrayExtra(EXTRA_MIME_TYPES)?.takeIf { it.isNotEmpty() } ?: arrayOf(ANY_TYPE)
        // Registered on every creation: on a re-creation the registry hands the pending
        // result to this new callback. Launched only on the first creation.
        val launch: () -> Unit = when (kind) {
            SafBridge.KIND_OPEN_FILE -> {
                val launcher = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri -> onPicked(listOfNotNull(uri)) }
                ({ launcher.launch(mimeTypes) })
            }
            SafBridge.KIND_OPEN_FILES -> {
                val launcher = registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris -> onPicked(uris) }
                ({ launcher.launch(mimeTypes) })
            }
            SafBridge.KIND_OPEN_FOLDER, SafBridge.KIND_SAVE_INTO_FOLDER -> {
                val launcher = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri -> onPicked(listOfNotNull(uri)) }
                ({ launcher.launch(null) })
            }
            SafBridge.KIND_SAVE_FILE -> {
                val createType = mimeTypes.first().takeUnless { it == ANY_TYPE } ?: OCTET_STREAM
                val launcher = registerForActivityResult(ActivityResultContracts.CreateDocument(createType)) { uri -> onPicked(listOfNotNull(uri)) }
                val suggestedName = intent.getStringExtra(EXTRA_SUGGESTED_NAME).orEmpty()
                ({ launcher.launch(suggestedName) })
            }
            else -> {
                settle(SafBridge.STATUS_FAILED, null, null, "unsupported file dialog kind $kind")
                finish()
                return
            }
        }
        if (savedInstanceState == null) {
            try {
                launch()
            } catch (t: Throwable) {
                // ActivityNotFoundException: no document picker on this device.
                Log.e(TAG, "SafRequestActivity: could not show the picker", t)
                settle(SafBridge.STATUS_FAILED, null, null, "could not show the file picker: $t")
                finish()
            }
        }
    }

    override fun onDestroy() {
        // Finished without a picker result (e.g. the picker activity itself failed): a cancel.
        // A destroy that is not a finish is a re-creation; the new instance reports instead.
        if (!settled && isFinishing) settle(SafBridge.STATUS_CANCELLED, null, null, null)
        super.onDestroy()
    }

    /** The picker returned `uris` (empty = cancelled). UI thread. */
    private fun onPicked(uris: List<Uri>) {
        if (settled) return
        if (uris.isEmpty()) {
            settle(SafBridge.STATUS_CANCELLED, null, null, null)
            finish()
            return
        }
        settled = true
        val app = applicationContext
        val isTree = kind == SafBridge.KIND_OPEN_FOLDER || kind == SafBridge.KIND_SAVE_INTO_FOLDER
        val write = kind == SafBridge.KIND_SAVE_FILE || kind == SafBridge.KIND_SAVE_INTO_FOLDER
        SafBridge.runInBackground {
            try {
                SafBridge.persistGrants(app, uris, write)
                val names = uris.map { SafBridge.displayName(app, it, isTree) }.toTypedArray()
                SafBridge.report(app, requestId, SafBridge.STATUS_PICKED, uris.map(Uri::toString).toTypedArray(), names, null)
            } catch (t: Throwable) {
                Log.e(TAG, "SafRequestActivity: could not report the picked documents", t)
                SafBridge.report(app, requestId, SafBridge.STATUS_FAILED, null, null, "could not read the picked documents: $t")
            } finally {
                runOnUiThread { finish() }
            }
        }
    }

    /** Reports a result without documents (on the bridge executor) and blocks further reports. */
    private fun settle(status: Int, uris: Array<String>?, names: Array<String>?, error: String?) {
        settled = true
        SafBridge.reportAsync(applicationContext, requestId, status, uris, names, error)
    }

    companion object {
        private const val TAG = "ManhwaStudio"
        private const val EXTRA_REQUEST_ID = "io.github.vasyanator.manhwastudio.saf.REQUEST_ID"
        private const val EXTRA_KIND = "io.github.vasyanator.manhwastudio.saf.KIND"
        private const val EXTRA_MIME_TYPES = "io.github.vasyanator.manhwastudio.saf.MIME_TYPES"
        private const val EXTRA_SUGGESTED_NAME = "io.github.vasyanator.manhwastudio.saf.SUGGESTED_NAME"
        private const val ANY_TYPE = "*/*"
        private const val OCTET_STREAM = "application/octet-stream"

        /** The start intent of request `requestId`. */
        fun intent(context: Context, requestId: Long, kind: Int, mimeTypes: Array<String>, suggestedName: String?): Intent =
            Intent(context, SafRequestActivity::class.java)
                .putExtra(EXTRA_REQUEST_ID, requestId)
                .putExtra(EXTRA_KIND, kind)
                .putExtra(EXTRA_MIME_TYPES, mimeTypes)
                .putExtra(EXTRA_SUGGESTED_NAME, suggestedName)
    }
}
