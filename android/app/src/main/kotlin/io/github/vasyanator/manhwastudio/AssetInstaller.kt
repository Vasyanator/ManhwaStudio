/*
 * File: AssetInstaller.kt
 *
 * Purpose:
 * Installs the runtime files bundled in the APK (the assets/ms_root tree: UI and typing fonts,
 * optionally the extended fonts and the wiki) into filesDir/ms_root, the directory the Rust
 * library makes its working directory and runtime root (src/android_entry.rs).
 *
 * Key functions:
 * - isInstalled(): cheap check that ms_root holds exactly this APK's bundle.
 * - install(): (re)installs the bundle; blocking, call it on a worker thread.
 *
 * Contract with the Rust side (src/android_entry.rs `check_bundled_assets`): the installed
 * stamp file is written LAST and atomically, only after every bundled file is in place, and is
 * deleted FIRST when an install starts. The native library refuses to start unless the
 * installed stamp equals the bundled one. The stamp names below are mirrored there.
 *
 * Data safety: a file is overwritten or deleted only when this installer put it there itself
 * (it is listed in the previous install's file list). Anything the app or the user created in
 * ms_root (user_config, fonts_data, presets, imported fonts, projects) is never touched, even
 * when its path collides with a bundled one.
 */
package io.github.vasyanator.manhwastudio

import android.content.Context
import android.util.Log
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException

object AssetInstaller {
    private const val TAG = "ManhwaStudio"

    /** Asset directory mirrored into the runtime root. */
    private const val ASSET_ROOT = "ms_root"

    /** Runtime root under filesDir; equals RUNTIME_ROOT_DIR_NAME in src/android_entry.rs. */
    private const val RUNTIME_ROOT_DIR = "ms_root"

    /** Asset with the bundle stamp (build-android.sh); BUNDLED_STAMP_ASSET in Rust. */
    private const val BUNDLED_STAMP_ASSET = "ms_root.stamp"

    /** Asset listing every bundled file, one ms_root-relative path per line (build-android.sh). */
    private const val BUNDLED_FILES_ASSET = "ms_root.files"

    /** Stamp written last inside the runtime root; INSTALLED_STAMP_FILE in Rust. */
    private const val INSTALLED_STAMP_FILE = ".ms_assets_stamp"

    /** Paths this installer owns in the runtime root (files it has written). */
    private const val INSTALLED_FILES_FILE = ".ms_assets_files"

    /** Suffix of the temporary file a copy goes through before its atomic rename. */
    private const val TMP_SUFFIX = ".ms_install_tmp"

    /** Progress callback: files done, files total. Called on the installing thread. */
    fun interface Progress {
        fun onProgress(done: Int, total: Int)
    }

    private fun runtimeRoot(context: Context) = File(context.filesDir, RUNTIME_ROOT_DIR)

    private fun readAssetText(context: Context, name: String): String =
        context.assets.open(name).use { it.readBytes().toString(Charsets.UTF_8) }

    /**
     * True when the runtime root holds exactly the bundle of this APK. Small file reads only,
     * cheap enough for Activity.onCreate. A missing bundled stamp (APK not built by
     * build-android.sh) reads as "not installed"; install() then reports why.
     */
    fun isInstalled(context: Context): Boolean {
        val bundled = try {
            readAssetText(context, BUNDLED_STAMP_ASSET).trim()
        } catch (e: IOException) {
            Log.e(TAG, "AssetInstaller: bundled stamp '$BUNDLED_STAMP_ASSET' unreadable: $e")
            return false
        }
        val installed = File(runtimeRoot(context), INSTALLED_STAMP_FILE)
        return try {
            bundled.isNotEmpty() && installed.readText(Charsets.UTF_8).trim() == bundled
        } catch (e: FileNotFoundException) {
            false
        } catch (e: IOException) {
            Log.w(TAG, "AssetInstaller: installed stamp '$installed' unreadable: $e")
            false
        }
    }

    /**
     * Installs the bundle of this APK into the runtime root. Blocking (copies up to ~150 MB):
     * never call it on the UI thread. Serialized process-wide; returns at once when the bundle
     * is already installed.
     *
     * @throws IOException when the APK carries no bundle or a file cannot be read or written;
     *   the installed stamp is then absent, so the native side keeps refusing to start.
     */
    @Synchronized
    @Throws(IOException::class)
    fun install(context: Context, progress: Progress) {
        if (isInstalled(context)) return
        val started = System.nanoTime()
        val root = runtimeRoot(context)
        if (!root.isDirectory && !root.mkdirs()) throw IOException("cannot create '$root'")
        val stampFile = File(root, INSTALLED_STAMP_FILE)
        // Stamp first out: an install interrupted from here on is detected on the next start.
        if (stampFile.exists() && !stampFile.delete()) throw IOException("cannot delete '$stampFile'")

        val stamp = readAssetText(context, BUNDLED_STAMP_ASSET).trim()
        if (stamp.isEmpty()) throw IOException("bundled stamp '$BUNDLED_STAMP_ASSET' is empty")
        val bundled = readAssetText(context, BUNDLED_FILES_ASSET).lines().map(String::trim).filter(String::isNotEmpty)
        bundled.forEach(::requireSafeRelativePath)

        val ownedFile = File(root, INSTALLED_FILES_FILE)
        val previouslyOwned = readOwned(ownedFile)
        // Record ownership of the new files BEFORE copying them, so files of an interrupted
        // install still count as ours on the retry and get overwritten instead of kept stale.
        writeAtomically(ownedFile, (previouslyOwned + bundled).sorted().joinToString("\n"))

        var copied = 0
        var kept = 0
        bundled.forEachIndexed { index, path ->
            val target = File(root, path)
            if (target.exists() && path !in previouslyOwned) {
                // Not ours (created by the app or the user): never overwrite it.
                Log.w(TAG, "AssetInstaller: keeping existing non-bundled file '$path'")
                kept++
            } else {
                copyAsset(context, "$ASSET_ROOT/$path", target)
                copied++
            }
            progress.onProgress(index + 1, bundled.size)
        }

        val bundledSet = bundled.toSet()
        var removed = 0
        for (path in previouslyOwned - bundledSet) {
            // A file a previous bundle installed and this one no longer ships.
            val stale = File(root, path)
            if (stale.isFile) {
                if (!stale.delete()) throw IOException("cannot delete stale bundled file '$stale'")
                removed++
            }
        }
        writeAtomically(ownedFile, bundled.sorted().joinToString("\n"))
        writeAtomically(stampFile, stamp)
        val ms = (System.nanoTime() - started) / 1_000_000
        Log.i(TAG, "AssetInstaller: installed bundle '$stamp' into '$root': copied=$copied kept=$kept removed=$removed in ${ms}ms")
    }

    /** Paths of the previous install's file list; empty when there was none. */
    private fun readOwned(file: File): Set<String> = try {
        file.readText(Charsets.UTF_8).lines().map(String::trim).filter(String::isNotEmpty).onEach(::requireSafeRelativePath).toSet()
    } catch (e: FileNotFoundException) {
        emptySet()
    }

    /** Rejects a listed path that could escape the runtime root. */
    private fun requireSafeRelativePath(path: String) {
        if (path.startsWith("/") || path.split('/').any { it == ".." || it.isEmpty() }) {
            throw IOException("unsafe bundled path '$path'")
        }
    }

    /** Copies one asset to `target` through a temporary sibling and an atomic rename. */
    private fun copyAsset(context: Context, asset: String, target: File) {
        val parent = target.parentFile ?: throw IOException("no parent directory for '$target'")
        if (!parent.isDirectory && !parent.mkdirs()) throw IOException("cannot create '$parent'")
        val tmp = File(parent, target.name + TMP_SUFFIX)
        context.assets.open(asset).use { input -> tmp.outputStream().use { output -> input.copyTo(output) } }
        if (!tmp.renameTo(target)) throw IOException("cannot move '$tmp' to '$target'")
    }

    /** Writes `text` to `target` through a temporary sibling and an atomic rename. */
    private fun writeAtomically(target: File, text: String) {
        val tmp = File(target.parentFile, target.name + TMP_SUFFIX)
        tmp.writeText(text, Charsets.UTF_8)
        if (!tmp.renameTo(target)) throw IOException("cannot move '$tmp' to '$target'")
    }
}
