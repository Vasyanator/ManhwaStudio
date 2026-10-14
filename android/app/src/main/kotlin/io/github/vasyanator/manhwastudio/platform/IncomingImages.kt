/*
 * File: IncomingImages.kt
 *
 * Purpose:
 * The incoming-images inbox: images other apps hand to ManhwaStudio through Android's Share
 * sheet (ACTION_SEND / ACTION_SEND_MULTIPLE) or "Open with" (ACTION_VIEW). The ONE owner of the
 * inbox layout; the Rust side (crates/ms-platform/src/incoming/) only takes published batches
 * over JNI and decodes the listing text this class returns.
 *
 * Key functions:
 * - receive(): copies the documents of one incoming intent into a new batch and publishes it
 *   (LaunchActivity's worker thread, while that Activity still holds the read grants).
 * - takePending(): claims every published batch, in batch-name (arrival) order.
 * - sweepClaimed(): deletes batches an earlier native session claimed, and unfinished leftovers.
 * - releaseBatch(): deletes one claimed batch once its images were opened.
 * - markNativeReady() / nativeOnIncoming(): the wake-up of the running Rust app.
 *
 * Layout (cacheDir/incoming/, NOT cache/saf_in: the SAF bridge purges that on its first use):
 *   <millis>-<n>/            one batch per incoming intent; the name sorts in arrival order
 *     NNNNN/<name>           one numbered folder per document, so equal display names never clash
 *     batch.tsv              ordered lines `f\t<absolute path>` or `s\t<name>\t<code>\t<detail>`
 *                            (code: copy_failed | no_stream); written to a temp file, then renamed
 *     ready                  published LAST (temp file + rename): the batch is complete
 *     claimed                `ready` renamed by takePending: a Rust session owns the batch
 * Names and details never contain a tab, CR or LF (sanitized here); paths are absolute.
 *
 * JNI contract (mirrored in crates/ms-platform/src/incoming/android.rs; the class binary name is
 * INCOMING_CLASS there): every @JvmStatic method below is called by Rust on a worker thread or on
 * the android_main thread before the window exists, and none of them throws into native code.
 * takePending answers one String per claimed batch: its absolute directory, "\n", then the
 * batch.tsv text. nativeOnIncoming is the Rust native (bound with RegisterNatives) and is called
 * after a publish only once markNativeReady() ran in this process: the batch itself always
 * travels through the inbox (pull), the native call is only the wake-up.
 */
package io.github.vasyanator.manhwastudio.platform

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.OpenableColumns
import android.util.Log
import android.webkit.MimeTypeMap
import java.io.File
import java.io.IOException
import java.util.concurrent.atomic.AtomicInteger

object IncomingImages {
    private const val TAG = "ManhwaStudio"

    /** Inbox root under cacheDir. */
    private const val INBOX_DIR = "incoming"
    private const val LISTING_FILE = "batch.tsv"
    private const val READY_MARKER = "ready"
    private const val CLAIMED_MARKER = "claimed"
    private const val TMP_SUFFIX = ".tmp"

    /** Skip codes of batch.tsv (`IncomingSkipKind` in Rust). */
    private const val CODE_COPY_FAILED = "copy_failed"
    private const val CODE_NO_STREAM = "no_stream"

    /** Longest file name (bytes) the app-storage file systems accept. */
    private const val MAX_NAME_BYTES = 255

    /** Local name of a document whose display name sanitizes to nothing. */
    private const val UNNAMED = "image"

    /** Distinguishes batches created within one millisecond. */
    private val batchCounter = AtomicInteger(0)

    private val lock = Any()

    /** Batches a receive() of this process is still writing; sweepClaimed must not touch them. Guarded by `lock`. */
    private val inProgress = HashSet<String>()

    /** True once the Rust app of this process bound nativeOnIncoming and wants wake-ups. */
    @Volatile
    private var nativeReady = false

    /** Whether `intent` carries images for the inbox (Share or "Open with"). */
    fun isIncoming(intent: Intent?): Boolean = when (intent?.action) {
        Intent.ACTION_VIEW, Intent.ACTION_SEND, Intent.ACTION_SEND_MULTIPLE -> true
        else -> false
    }

    /**
     * Copies the documents of `intent` into a new batch and publishes it. Blocking (worker thread
     * only); `activity` is the Activity that received the intent and must stay alive meanwhile,
     * because the read grants belong to it. A finishing `activity` aborts the receive and deletes
     * the partial batch. `progress(done, total)` runs on the calling thread after each document.
     * Returns true when a batch was published (even one whose every document was skipped, so the
     * user sees why); false when the intent carried no document or the batch could not be
     * written. Never throws.
     */
    fun receive(activity: Activity, intent: Intent, progress: (Int, Int) -> Unit): Boolean {
        val items = incomingUris(intent)
        if (items.isEmpty()) {
            Log.w(TAG, "IncomingImages: ${intent.action} carried no document")
            return false
        }
        val batchName = String.format(java.util.Locale.ROOT, "%013d-%06d", System.currentTimeMillis(), batchCounter.getAndIncrement())
        val batchDir = File(inboxRoot(activity), batchName)
        synchronized(lock) { inProgress.add(batchName) }
        try {
            if (!batchDir.mkdirs()) throw IOException("could not create the inbox folder ${batchDir.name}")
            val lines = StringBuilder()
            var copied = 0
            for ((index, uri) in items.withIndex()) {
                if (activity.isFinishing || activity.isDestroyed) {
                    Log.i(TAG, "IncomingImages: receiving was cancelled after $index of ${items.size} documents")
                    batchDir.deleteRecursively()
                    return false
                }
                if (uri == null) {
                    lines.append("s\t").append(index + 1).append('\t').append(CODE_NO_STREAM).append("\t\n")
                } else {
                    val name = localName(activity, uri)
                    val dest = File(File(batchDir, String.format(java.util.Locale.ROOT, "%05d", index)), name)
                    val error = SafBridge.copyToFile(activity, uri.toString(), dest.absolutePath)
                    if (error == null) {
                        lines.append("f\t").append(dest.absolutePath).append('\n')
                        copied++
                    } else {
                        dest.parentFile?.deleteRecursively()
                        lines.append("s\t").append(name).append('\t').append(CODE_COPY_FAILED).append('\t').append(tsvField(error)).append('\n')
                    }
                }
                progress(index + 1, items.size)
            }
            writeAtomically(File(batchDir, LISTING_FILE), lines.toString())
            writeAtomically(File(batchDir, READY_MARKER), "")
            Log.i(TAG, "IncomingImages: batch $batchName published (${intent.action}, $copied of ${items.size} documents copied)")
        } catch (t: Throwable) {
            Log.e(TAG, "IncomingImages: receiving ${items.size} documents failed", t)
            batchDir.deleteRecursively()
            return false
        } finally {
            synchronized(lock) { inProgress.remove(batchName) }
        }
        wakeNative()
        return true
    }

    /**
     * Rust native (incoming/android.rs): a batch was published. Only sets a flag and wakes the
     * GUI; the batch is taken later with takePending.
     */
    @JvmStatic
    external fun nativeOnIncoming()

    /** Called by Rust once nativeOnIncoming is bound: later publishes wake the running app. */
    @JvmStatic
    fun markNativeReady() {
        nativeReady = true
    }

    /**
     * Claims every published batch (renames `ready` to `claimed`) in batch-name order and returns
     * one entry per claimed batch: "<absolute dir>\n<batch.tsv text>". A batch whose listing
     * cannot be read is answered with a single copy_failed line naming the batch, so the user is
     * told instead of the images vanishing. Never throws.
     */
    @JvmStatic
    fun takePending(context: Context): Array<String> {
        val out = ArrayList<String>()
        try {
            val batches = inboxRoot(context).listFiles()?.filter { it.isDirectory }?.sortedBy { it.name } ?: emptyList()
            for (batch in batches) {
                val ready = File(batch, READY_MARKER)
                if (!ready.isFile || !ready.renameTo(File(batch, CLAIMED_MARKER))) continue
                val listing = try {
                    File(batch, LISTING_FILE).readText()
                } catch (e: IOException) {
                    Log.e(TAG, "IncomingImages: could not read the listing of batch ${batch.name}", e)
                    "s\t${batch.name}\t$CODE_COPY_FAILED\t${tsvField(e.toString())}\n"
                }
                out += batch.absolutePath + "\n" + listing
            }
        } catch (t: Throwable) {
            Log.e(TAG, "IncomingImages.takePending failed", t)
        }
        return out.toTypedArray()
    }

    /**
     * Deletes every batch an earlier native session claimed, and every unfinished batch no
     * receive() of this process is writing (a receive killed mid-copy). Called by Rust before
     * its first takePending. Never throws.
     */
    @JvmStatic
    fun sweepClaimed(context: Context) {
        try {
            val batches = inboxRoot(context).listFiles()?.filter { it.isDirectory } ?: return
            for (batch in batches) {
                val claimed = File(batch, CLAIMED_MARKER).exists()
                val unfinished = !claimed && !File(batch, READY_MARKER).exists() && synchronized(lock) { !inProgress.contains(batch.name) }
                if ((claimed || unfinished) && !batch.deleteRecursively()) {
                    Log.w(TAG, "IncomingImages: could not delete the old batch ${batch.name}")
                }
            }
        } catch (t: Throwable) {
            Log.w(TAG, "IncomingImages.sweepClaimed failed", t)
        }
    }

    /**
     * Deletes the claimed batch `dir` (an absolute path takePending returned). Refuses anything
     * that is not a direct child of the inbox root. Returns null on success, else the error text.
     * Never throws.
     */
    @JvmStatic
    fun releaseBatch(context: Context, dir: String): String? {
        return try {
            val root = inboxRoot(context).canonicalFile
            val batch = File(dir).canonicalFile
            when {
                batch.parentFile != root -> "not an inbox batch: ${batch.name}"
                !batch.exists() -> null
                batch.deleteRecursively() -> null
                else -> "could not delete the batch ${batch.name}"
            }
        } catch (t: Throwable) {
            Log.w(TAG, "IncomingImages.releaseBatch failed", t)
            t.toString()
        }
    }

    private fun inboxRoot(context: Context): File = File(context.cacheDir, INBOX_DIR)

    /** Calls nativeOnIncoming when the Rust app is listening; a cold start finds the batch in init. */
    private fun wakeNative() {
        if (!nativeReady) return
        try {
            nativeOnIncoming()
        } catch (t: Throwable) {
            // UnsatisfiedLinkError: the library is not loaded; the batch waits for the next init.
            Log.w(TAG, "IncomingImages: nativeOnIncoming failed", t)
        }
    }

    /**
     * The document URIs of `intent` in the order the sender gave them; null marks an item that
     * carried no stream (a ClipData text item). EXTRA_STREAM first, ClipData as the fallback
     * (some senders fill only ClipData, which is also what carries the grants).
     */
    private fun incomingUris(intent: Intent): List<Uri?> {
        val fromExtras: List<Uri?> = when (intent.action) {
            Intent.ACTION_VIEW -> listOfNotNull(intent.data)
            Intent.ACTION_SEND -> listOfNotNull(streamExtra(intent))
            Intent.ACTION_SEND_MULTIPLE -> streamListExtra(intent)
            else -> emptyList()
        }
        if (fromExtras.isNotEmpty()) return fromExtras
        val clip = intent.clipData ?: return emptyList()
        return (0 until clip.itemCount).map { clip.getItemAt(it).uri }
    }

    private fun streamExtra(intent: Intent): Uri? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }

    private fun streamListExtra(intent: Intent): List<Uri?> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java) ?: emptyList()
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM) ?: emptyList()
        }

    /**
     * The local file name of `uri`: the provider's display name (else the last path segment),
     * sanitized, with the extension of its MIME type added when the name has none (the Rust
     * decoder detects formats by content, except TGA, which needs the extension).
     */
    private fun localName(context: Context, uri: Uri): String {
        var name: String? = null
        try {
            context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                if (cursor.moveToFirst() && !cursor.isNull(0)) name = cursor.getString(0)
            }
        } catch (t: Throwable) {
            Log.w(TAG, "IncomingImages: could not query a document name", t)
        }
        var result = sanitizeName(name ?: uri.lastPathSegment ?: "")
        if (!result.contains('.')) {
            val type = try { context.contentResolver.getType(uri) } catch (t: Throwable) { null }
            val extension = type?.let { MimeTypeMap.getSingleton().getExtensionFromMimeType(it) }
            if (extension != null) result = sanitizeName("$result.$extension")
        }
        return result
    }

    /** A provider name made safe as one path component and as one TSV field. */
    private fun sanitizeName(raw: String): String {
        val cleaned = raw.map { c -> if (c == '/' || c == '\\' || c.isISOControl()) '_' else c }.joinToString("").trim()
        var name = if (cleaned.isEmpty() || cleaned == "." || cleaned == "..") UNNAMED else cleaned
        while (name.toByteArray(Charsets.UTF_8).size > MAX_NAME_BYTES) {
            // Shorten the stem, keeping a short extension intact.
            val dot = name.lastIndexOf('.')
            name = if (dot > 0 && name.length - dot <= 16) name.substring(0, dot - 1) + name.substring(dot) else name.dropLast(1)
        }
        return name
    }

    /** `text` as one TSV field: tabs and line breaks become spaces. */
    private fun tsvField(text: String): String = text.map { c -> if (c == '\t' || c == '\n' || c == '\r') ' ' else c }.joinToString("")

    /** Writes `text` to a temp file and renames it onto `target`, so readers see all or nothing. */
    private fun writeAtomically(target: File, text: String) {
        val tmp = File(target.parentFile, target.name + TMP_SUFFIX)
        tmp.writeText(text)
        if (!tmp.renameTo(target)) throw IOException("could not publish ${target.name}")
    }
}
