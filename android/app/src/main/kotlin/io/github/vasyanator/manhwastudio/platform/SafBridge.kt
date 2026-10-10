/*
 * File: SafBridge.kt
 *
 * Purpose:
 * The Kotlin half of the Storage Access Framework bridge of the Rust file-dialog seam
 * (crates/ms-platform/src/file_dialog/android.rs). The Rust side loads this class by its binary
 * name (`SAF_BRIDGE_CLASS` in crates/ms-platform/src/android/saf.rs, the single owner of the
 * name) and calls the static methods below over JNI.
 *
 * Key functions:
 * - launch(): starts SafRequestActivity on the UI thread and returns at once.
 * - nativeOnResult(): the Rust native that receives the one result of a launched request.
 * - copyToFile(), listTree(), treeHasEntries(), streamFileToUri(), copyDirIntoTree(),
 *   releaseUri(): blocking document I/O, called by Rust on its worker threads.
 *
 * Contract (mirrored in file_dialog/android.rs):
 * - `kind` codes 0..4 = OpenFile, OpenFiles, OpenFolder, SaveFile, SaveIntoFolder (the Rust
 *   `DialogKind` order); status codes 0 picked, 1 cancelled, 2 failed.
 * - A launch that returned null is answered by EXACTLY ONE nativeOnResult call for its id
 *   (picker result, cancel, start failure, or a start that never happened within
 *   START_TIMEOUT_MS), always from the background executor, never from the UI thread.
 * - Nothing here throws into native code: every entry point catches Throwable and answers with
 *   an error value (String = error text, null = ok; listTree ["err", text]; treeHasEntries -1).
 * - URI grants: the grants a picker result carries belong to SafRequestActivity, which finishes
 *   right after reporting, so the reported documents are first made persistable; Rust (or this
 *   class, when nativeOnResult answers false) releases them once the copy or commit is done.
 *   On its first use in a process the bridge releases every persisted grant left by an earlier
 *   process: the app keeps no document grant across processes.
 *
 * Notes:
 * The picker opens over the last resumed app Activity (tracked from the first launch on), so it
 * appears in the window that asked; before any Activity is tracked it starts from the
 * application context with FLAG_ACTIVITY_NEW_TASK, which places it in the existing app task.
 */
package io.github.vasyanator.manhwastudio.platform

import android.app.Activity
import android.app.Application
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.util.Log
import android.webkit.MimeTypeMap
import java.io.File
import java.io.FileInputStream
import java.io.FileNotFoundException
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.lang.ref.WeakReference
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

object SafBridge {
    private const val TAG = "ManhwaStudio"

    const val KIND_OPEN_FILE = 0
    const val KIND_OPEN_FILES = 1
    const val KIND_OPEN_FOLDER = 2
    const val KIND_SAVE_FILE = 3
    const val KIND_SAVE_INTO_FOLDER = 4

    const val STATUS_PICKED = 0
    const val STATUS_CANCELLED = 1
    const val STATUS_FAILED = 2

    /** How long a posted launch may take to create SafRequestActivity before it is failed. */
    private const val START_TIMEOUT_MS = 10_000L

    /** Copy buffer of the document streams. */
    private const val COPY_BUFFER_BYTES = 256 * 1024

    /** Upper bound of a listed tree; protects against a provider reporting a cycle. */
    private const val MAX_TREE_ENTRIES = 200_000

    /** Type of a created file whose extension has no well-known MIME type. */
    private const val OCTET_STREAM = "application/octet-stream"

    private val mainHandler = Handler(Looper.getMainLooper())

    /** Reports and their grant / name queries; one dialog is in flight at a time. */
    private val executor: ExecutorService = Executors.newSingleThreadExecutor { runnable ->
        Thread(runnable, "saf-bridge").apply { isDaemon = true }
    }

    private val lock = Any()

    // Guarded by `lock`.
    private var initialized = false
    private var lastResumed: WeakReference<Activity>? = null
    private val liveRequests = HashSet<Long>()
    private val awaitingStart = HashSet<Long>()

    /** Remembers the last resumed app Activity: the window a new picker should cover. */
    private val activityTracker = object : Application.ActivityLifecycleCallbacks {
        override fun onActivityResumed(activity: Activity) {
            if (activity !is SafRequestActivity) {
                synchronized(lock) { lastResumed = WeakReference(activity) }
            }
        }

        override fun onActivityDestroyed(activity: Activity) {
            synchronized(lock) {
                if (lastResumed?.get() === activity) lastResumed = null
            }
        }

        override fun onActivityCreated(activity: Activity, savedInstanceState: Bundle?) {}
        override fun onActivityStarted(activity: Activity) {}
        override fun onActivityPaused(activity: Activity) {}
        override fun onActivityStopped(activity: Activity) {}
        override fun onActivitySaveInstanceState(activity: Activity, outState: Bundle) {}
    }

    /**
     * Rust native (file_dialog/android.rs): delivers the one result of `requestId`. Returns
     * whether Rust took over the URI grants of `uris`; on false this class releases them.
     */
    @JvmStatic
    external fun nativeOnResult(requestId: Long, status: Int, uris: Array<String>?, names: Array<String>?, error: String?): Boolean

    /**
     * Shows the picker of `kind` for `requestId`. Returns at once: null = the request is posted
     * and will be answered through nativeOnResult; otherwise the error text (nothing posted).
     * `mimeTypes`: filter of the open kinds, the created type for SaveFile, empty for trees.
     * `suggestedName`: SaveFile only.
     */
    @JvmStatic
    fun launch(context: Context, requestId: Long, kind: Int, mimeTypes: Array<String>, suggestedName: String?): String? {
        return try {
            if (kind !in KIND_OPEN_FILE..KIND_SAVE_INTO_FOLDER) return "unsupported file dialog kind $kind"
            val app = context.applicationContext
            ensureInitialized(app)
            synchronized(lock) {
                liveRequests.add(requestId)
                awaitingStart.add(requestId)
            }
            val intent = SafRequestActivity.intent(app, requestId, kind, mimeTypes, suggestedName)
            if (mainHandler.post { startRequestActivity(app, requestId, intent) }) {
                null
            } else {
                forget(requestId)
                "the main thread is not running"
            }
        } catch (t: Throwable) {
            Log.e(TAG, "SafBridge.launch: request $requestId failed", t)
            forget(requestId)
            "could not launch the file picker: $t"
        }
    }

    /** UI thread: starts SafRequestActivity over the tracked window and arms the start watchdog. */
    private fun startRequestActivity(app: Context, requestId: Long, intent: Intent) {
        val host = synchronized(lock) { lastResumed?.get() }?.takeUnless { it.isFinishing || it.isDestroyed }
        try {
            if (host != null) {
                host.startActivity(intent)
            } else {
                app.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            }
        } catch (t: Throwable) {
            Log.e(TAG, "SafBridge: could not start the picker activity for request $requestId", t)
            synchronized(lock) { awaitingStart.remove(requestId) }
            reportAsync(app, requestId, STATUS_FAILED, null, null, "could not open the file picker: $t")
            return
        }
        // The system may drop a start silently (e.g. a background-start restriction); the Rust
        // worker would then wait forever, so an activity that never appears fails the request.
        mainHandler.postDelayed({
            val neverStarted = synchronized(lock) { awaitingStart.remove(requestId) }
            if (neverStarted) {
                Log.w(TAG, "SafBridge: the picker activity of request $requestId did not start")
                reportAsync(app, requestId, STATUS_FAILED, null, null, "the file picker did not open")
            }
        }, START_TIMEOUT_MS)
    }

    /** First use in this process: track Activities and drop grants an earlier process left. */
    private fun ensureInitialized(app: Context) {
        synchronized(lock) {
            if (initialized) return
            initialized = true
        }
        (app as? Application)?.registerActivityLifecycleCallbacks(activityTracker)
        try {
            for (permission in app.contentResolver.persistedUriPermissions) {
                app.contentResolver.releasePersistableUriPermission(permission.uri, grantFlags(permission.isReadPermission, permission.isWritePermission))
            }
        } catch (t: Throwable) {
            Log.w(TAG, "SafBridge: could not release leftover document grants", t)
        }
    }

    private fun forget(requestId: Long) {
        synchronized(lock) {
            liveRequests.remove(requestId)
            awaitingStart.remove(requestId)
        }
    }

    /** SafRequestActivity.onCreate: whether a Rust worker still waits for `requestId`. */
    internal fun onRequestActivityCreated(requestId: Long): Boolean = synchronized(lock) {
        awaitingStart.remove(requestId)
        liveRequests.contains(requestId)
    }

    /** Runs `task` on the bridge executor (never the UI thread). */
    internal fun runInBackground(task: () -> Unit) {
        executor.execute {
            try {
                task()
            } catch (t: Throwable) {
                Log.e(TAG, "SafBridge: background task failed", t)
            }
        }
    }

    /** [report] on the bridge executor. */
    internal fun reportAsync(context: Context, requestId: Long, status: Int, uris: Array<String>?, names: Array<String>?, error: String?) {
        runInBackground { report(context, requestId, status, uris, names, error) }
    }

    /**
     * Delivers the one result of `requestId` (later calls for the same id are dropped). Grants
     * of `uris` that Rust does not take over are released here. Bridge executor only.
     */
    internal fun report(context: Context, requestId: Long, status: Int, uris: Array<String>?, names: Array<String>?, error: String?) {
        val live = synchronized(lock) { liveRequests.remove(requestId) }
        val accepted = if (!live) {
            Log.i(TAG, "SafBridge: request $requestId was already answered; result dropped")
            false
        } else {
            try {
                nativeOnResult(requestId, status, uris, names, error)
            } catch (t: Throwable) {
                // UnsatisfiedLinkError: the native library is not loaded in this process.
                Log.e(TAG, "SafBridge: nativeOnResult failed for request $requestId", t)
                false
            }
        }
        if (!accepted) uris?.forEach { releaseUri(context, it) }
    }

    /** Takes a persistable grant for each picked document (see the file header). */
    internal fun persistGrants(context: Context, uris: List<Uri>, write: Boolean) {
        for (uri in uris) {
            try {
                context.contentResolver.takePersistableUriPermission(uri, grantFlags(true, write))
            } catch (t: Throwable) {
                // Not persistable (a provider that does not offer it): the transient grant of
                // the request activity is all there is.
                Log.w(TAG, "SafBridge: could not persist the grant of a picked document", t)
            }
        }
    }

    /** The name the picker showed for a document (or a tree's root). */
    internal fun displayName(context: Context, uri: Uri, isTree: Boolean): String {
        val documentUri = if (isTree) DocumentsContract.buildDocumentUriUsingTree(uri, DocumentsContract.getTreeDocumentId(uri)) else uri
        try {
            context.contentResolver.query(documentUri, arrayOf(Document.COLUMN_DISPLAY_NAME), null, null, null)?.use { cursor ->
                if (cursor.moveToFirst() && !cursor.isNull(0)) return cursor.getString(0)
            }
        } catch (t: Throwable) {
            Log.w(TAG, "SafBridge: could not query a document name", t)
        }
        // The provider exposes no display name: the document id is the only name there is.
        return uri.lastPathSegment ?: "document"
    }

    /** Copies the document `uri` into the local file `destPath`. Blocking; null = ok. */
    @JvmStatic
    fun copyToFile(context: Context, uri: String, destPath: String): String? = guarded("copyToFile") {
        val dest = File(destPath)
        dest.parentFile?.mkdirs()
        val input = context.contentResolver.openInputStream(Uri.parse(uri)) ?: throw IOException("the provider returned no data stream")
        input.use { source -> FileOutputStream(dest).use { sink -> copyStream(source, sink) } }
    }

    /** Streams the local file `srcPath` into the document `uri`, replacing its content. Blocking; null = ok. */
    @JvmStatic
    fun streamFileToUri(context: Context, srcPath: String, uri: String): String? = guarded("streamFileToUri") {
        writeFileToDocument(context, File(srcPath), Uri.parse(uri))
    }

    /**
     * Lists the tree `treeUri` breadth-first as ["ok", (kind, parent, name, uri)*]: kind "d" / "f",
     * parent the index of an earlier "d" entry or "-1" for the tree root. ["err", text] on failure.
     */
    @JvmStatic
    fun listTree(context: Context, treeUri: String): Array<String> {
        return try {
            val tree = Uri.parse(treeUri)
            val out = arrayListOf("ok")
            val queue = ArrayDeque<Pair<String, Int>>()
            queue.addLast(DocumentsContract.getTreeDocumentId(tree) to -1)
            var count = 0
            while (queue.isNotEmpty()) {
                val (parentId, parentIndex) = queue.removeFirst()
                for (child in children(context, tree, parentId)) {
                    if (count >= MAX_TREE_ENTRIES) throw IOException("the folder holds more than $MAX_TREE_ENTRIES entries")
                    val index = count++
                    out += if (child.isDir) "d" else "f"
                    out += parentIndex.toString()
                    out += child.name
                    out += DocumentsContract.buildDocumentUriUsingTree(tree, child.id).toString()
                    if (child.isDir) queue.addLast(child.id to index)
                }
            }
            out.toTypedArray()
        } catch (t: Throwable) {
            Log.w(TAG, "SafBridge.listTree failed", t)
            arrayOf("err", describe(t))
        }
    }

    /** 1 when the tree `treeUri` has any entry, 0 when it is empty, -1 when it cannot be listed. */
    @JvmStatic
    fun treeHasEntries(context: Context, treeUri: String): Int {
        return try {
            val tree = Uri.parse(treeUri)
            if (children(context, tree, DocumentsContract.getTreeDocumentId(tree)).isEmpty()) 0 else 1
        } catch (t: Throwable) {
            Log.w(TAG, "SafBridge.treeHasEntries failed", t)
            -1
        }
    }

    /**
     * Copies the local folder `srcDir` into the tree `treeUri`: missing folders and files are
     * created, a same-named file is overwritten, a same-named folder is merged into. Blocking;
     * null = ok.
     */
    @JvmStatic
    fun copyDirIntoTree(context: Context, srcDir: String, treeUri: String): String? = guarded("copyDirIntoTree") {
        val tree = Uri.parse(treeUri)
        copyDirInto(context, tree, DocumentsContract.getTreeDocumentId(tree), File(srcDir))
    }

    /** Releases the persisted grant of `uri`, if any. Never throws. */
    @JvmStatic
    fun releaseUri(context: Context, uri: String) {
        try {
            val parsed = Uri.parse(uri)
            for (permission in context.contentResolver.persistedUriPermissions) {
                if (permission.uri == parsed) {
                    context.contentResolver.releasePersistableUriPermission(parsed, grantFlags(permission.isReadPermission, permission.isWritePermission))
                }
            }
        } catch (t: Throwable) {
            Log.w(TAG, "SafBridge.releaseUri failed", t)
        }
    }

    private class Child(val id: String, val name: String, val isDir: Boolean)

    /** The direct children of the document `parentId` inside `tree`. */
    private fun children(context: Context, tree: Uri, parentId: String): List<Child> {
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(tree, parentId)
        val projection = arrayOf(Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE)
        val cursor = context.contentResolver.query(childrenUri, projection, null, null, null) ?: throw IOException("the provider did not list the folder")
        return cursor.use {
            val result = ArrayList<Child>(it.count)
            while (it.moveToNext()) {
                val id = it.getString(0) ?: continue
                val name = it.getString(1) ?: id
                result += Child(id, name, it.getString(2) == Document.MIME_TYPE_DIR)
            }
            result
        }
    }

    private fun copyDirInto(context: Context, tree: Uri, parentId: String, dir: File) {
        val existing = children(context, tree, parentId).associateBy { it.name }
        val entries = dir.listFiles() ?: throw IOException("could not list the staged folder ${dir.name}")
        for (entry in entries.sortedBy { it.name }) {
            val found = existing[entry.name]
            if (entry.isDirectory) {
                val childId = when {
                    found == null -> createDocument(context, tree, parentId, Document.MIME_TYPE_DIR, entry.name)
                    found.isDir -> found.id
                    else -> throw IOException("'${entry.name}' already exists in the destination as a file")
                }
                copyDirInto(context, tree, childId, entry)
            } else {
                val childId = when {
                    found == null -> createDocument(context, tree, parentId, mimeTypeOf(entry.name), entry.name)
                    found.isDir -> throw IOException("'${entry.name}' already exists in the destination as a folder")
                    else -> found.id
                }
                writeFileToDocument(context, entry, DocumentsContract.buildDocumentUriUsingTree(tree, childId))
            }
        }
    }

    /** Creates a child document and returns its id. */
    private fun createDocument(context: Context, tree: Uri, parentId: String, mimeType: String, name: String): String {
        val parentUri = DocumentsContract.buildDocumentUriUsingTree(tree, parentId)
        val created = DocumentsContract.createDocument(context.contentResolver, parentUri, mimeType, name) ?: throw IOException("the provider could not create '$name'")
        return DocumentsContract.getDocumentId(created)
    }

    /** Replaces the content of the document `target` with the file `source`. */
    private fun writeFileToDocument(context: Context, source: File, target: Uri) {
        openTruncating(context, target).use { sink -> FileInputStream(source).use { input -> copyStream(input, sink) } }
    }

    /**
     * Opens `uri` for writing from offset 0, dropping old content ("wt"). A provider without
     * "wt" support gets "w": safe for the documents this bridge writes, which are either newly
     * created (empty) or overwritten in providers that honour "wt".
     */
    private fun openTruncating(context: Context, uri: Uri): OutputStream {
        val truncating = try {
            context.contentResolver.openOutputStream(uri, "wt")
        } catch (unsupported: IllegalArgumentException) {
            null
        } catch (unsupported: UnsupportedOperationException) {
            null
        } catch (unsupported: FileNotFoundException) {
            Log.i(TAG, "SafBridge: mode \"wt\" refused, retrying with \"w\"", unsupported)
            null
        }
        return truncating ?: context.contentResolver.openOutputStream(uri, "w") ?: throw IOException("the provider returned no output stream")
    }

    private fun copyStream(input: InputStream, output: OutputStream) {
        val buffer = ByteArray(COPY_BUFFER_BYTES)
        while (true) {
            val read = input.read(buffer)
            if (read < 0) break
            output.write(buffer, 0, read)
        }
        output.flush()
    }

    private fun mimeTypeOf(name: String): String {
        val extension = name.substringAfterLast('.', "").lowercase()
        if (extension.isEmpty()) return OCTET_STREAM
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension) ?: OCTET_STREAM
    }

    private fun grantFlags(read: Boolean, write: Boolean): Int =
        (if (read) Intent.FLAG_GRANT_READ_URI_PERMISSION else 0) or (if (write) Intent.FLAG_GRANT_WRITE_URI_PERMISSION else 0)

    /** Runs `body`; null when it succeeded, else the logged error text. Never throws. */
    private inline fun guarded(operation: String, body: () -> Unit): String? {
        return try {
            body()
            null
        } catch (t: Throwable) {
            Log.w(TAG, "SafBridge.$operation failed", t)
            describe(t)
        }
    }

    private fun describe(t: Throwable): String = t.message?.let { "${t.javaClass.simpleName}: $it" } ?: t.javaClass.simpleName
}
