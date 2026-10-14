/*
 * File: CredentialBridge.kt
 *
 * Purpose:
 * The Android credential store of ManhwaStudio: API keys and access tokens (AI API keys, the
 * Hugging Face token) encrypted with a key that lives in the AndroidKeyStore. The Rust seam
 * crates/ms-platform/src/credentials/ calls it over JNI; Rust never sees the Keystore key.
 *
 * Key functions:
 * - read(): one entry, decrypted.
 * - write(): encrypts and stores one entry, replacing any previous value (also an unreadable one:
 *   an explicit save is how the user repairs it).
 * - delete(): removes one entry; a missing entry is success.
 *
 * Storage:
 * - One AES-256-GCM key, alias KEY_ALIAS, generated in the AndroidKeyStore on the first write:
 *   non-exportable, no user-authentication requirement (keys are read by background workers).
 * - Entries live in the app-private SharedPreferences file PREFS_NAME (MODE_PRIVATE). Cloud
 *   backup is off in the manifest; a device-to-device transfer may still copy the file, but the
 *   Keystore key never leaves the device, so such entries read as "unreadable", never as a secret.
 *   Value = "1:" + Base64(IV) + ":" + Base64(ciphertext + GCM tag), a fresh Keystore-generated
 *   IV per write. The entry name is the GCM additional authenticated data, so a blob copied
 *   under another name fails to decrypt instead of answering the wrong secret.
 * - Entry names are built by Rust (`credentials::entry_name`: "v1." + hex(service) + "." +
 *   hex(account)) and are opaque here.
 *
 * JNI contract (mirrored in crates/ms-platform/src/credentials/android.rs, class binary name
 * CREDENTIAL_CLASS there; wire decoding in credentials/mod.rs):
 * - read(Context, String): String[] = {"ok", secret} | {"missing"} | {"unreadable", detail} |
 *   {"failed", detail}. "unreadable" = an entry exists but cannot be decrypted (Keystore key
 *   lost or invalidated, malformed or tampered value); it is NEVER reported as "missing" and is
 *   never deleted or overwritten here except by an explicit write() or delete().
 * - write(Context, String, String): String? and delete(Context, String): String?; null = done,
 *   otherwise the failure detail.
 * Every entry point catches Throwable and answers with a value: nothing throws into native code.
 * Secrets never appear in a log line or in a detail text; details carry only exception classes
 * and messages of the crypto / storage layers.
 * Callers are Rust worker threads; every method is blocking (Keystore binder calls, a
 * synchronous SharedPreferences commit) and serialized by `lock`, so two first writes cannot
 * generate two Keystore keys.
 */
package io.github.vasyanator.manhwastudio.platform

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import java.security.UnrecoverableKeyException
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

object CredentialBridge {
    private const val TAG = "ManhwaStudio"

    private const val KEYSTORE_PROVIDER = "AndroidKeyStore"

    /** Alias of the one Keystore key. A persistence contract: renaming it orphans every entry. */
    private const val KEY_ALIAS = "io.github.vasyanator.manhwastudio.credentials.v1"

    /** App-private SharedPreferences file of the encrypted entries. A persistence contract. */
    private const val PREFS_NAME = "ms_credentials"

    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_TAG_BITS = 128
    private const val KEY_BITS = 256

    /** Version prefix of a stored value; any other prefix reads as "unreadable". */
    private const val VALUE_VERSION = "1"

    /** Answer status codes of read() (`decode_read_answer` in Rust). */
    private const val STATUS_OK = "ok"
    private const val STATUS_MISSING = "missing"
    private const val STATUS_UNREADABLE = "unreadable"
    private const val STATUS_FAILED = "failed"

    private val lock = Any()

    /** A stored value that exists but cannot be turned back into its secret. */
    private class UnreadableEntry(message: String) : Exception(message)

    /** Reads and decrypts entry `name`. See the JNI contract in the file header. */
    @JvmStatic
    fun read(context: Context, name: String): Array<String> {
        return try {
            synchronized(lock) {
                val stored = prefs(context).getString(name, null) ?: return arrayOf(STATUS_MISSING)
                arrayOf(STATUS_OK, decrypt(name, stored))
            }
        } catch (t: Throwable) {
            val detail = describe(t)
            if (isUnreadable(t)) {
                Log.w(TAG, "credential entry cannot be decrypted: $detail")
                arrayOf(STATUS_UNREADABLE, detail)
            } else {
                Log.w(TAG, "credential read failed: $detail")
                arrayOf(STATUS_FAILED, detail)
            }
        }
    }

    /** Encrypts `secret` and stores it as entry `name`, replacing any previous value. */
    @JvmStatic
    fun write(context: Context, name: String, secret: String): String? = guarded("write") {
        synchronized(lock) {
            val value = encrypt(name, secret)
            if (!prefs(context).edit().putString(name, value).commit()) {
                throw IllegalStateException("the credential preferences file could not be written")
            }
        }
    }

    /** Removes entry `name`; a missing entry is success. */
    @JvmStatic
    fun delete(context: Context, name: String): String? = guarded("delete") {
        synchronized(lock) {
            val prefs = prefs(context)
            if (prefs.contains(name) && !prefs.edit().remove(name).commit()) {
                throw IllegalStateException("the credential preferences file could not be written")
            }
        }
    }

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    private fun keyStore(): KeyStore = KeyStore.getInstance(KEYSTORE_PROVIDER).apply { load(null) }

    /** The Keystore key, or null when it does not exist (never written, or lost). */
    private fun existingKey(): SecretKey? = keyStore().getKey(KEY_ALIAS, null) as SecretKey?

    /** The Keystore key, generated on first use. Only write() may create it. */
    private fun keyForWrite(): SecretKey {
        existingKey()?.let { return it }
        val spec = KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(KEY_BITS)
            .build()
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE_PROVIDER)
        generator.init(spec)
        Log.i(TAG, "credential store key generated")
        return generator.generateKey()
    }

    private fun encrypt(name: String, secret: String): String {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        // The Keystore requires randomized encryption: it generates the IV itself.
        cipher.init(Cipher.ENCRYPT_MODE, keyForWrite())
        cipher.updateAAD(name.toByteArray(Charsets.UTF_8))
        val sealed = cipher.doFinal(secret.toByteArray(Charsets.UTF_8))
        return VALUE_VERSION + ":" + b64(cipher.iv) + ":" + b64(sealed)
    }

    private fun decrypt(name: String, stored: String): String {
        val parts = stored.split(':')
        if (parts.size != 3 || parts[0] != VALUE_VERSION) {
            throw UnreadableEntry("unknown stored value format")
        }
        // A lost key with entries still present is the classic "app data restored / Keystore
        // cleared" case: report it, never treat it as "no secret stored".
        val key = existingKey() ?: throw UnreadableEntry("the credential store key no longer exists")
        val iv = Base64.decode(parts[1], Base64.NO_WRAP)
        val sealed = Base64.decode(parts[2], Base64.NO_WRAP)
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, iv))
        cipher.updateAAD(name.toByteArray(Charsets.UTF_8))
        return String(cipher.doFinal(sealed), Charsets.UTF_8)
    }

    private fun b64(bytes: ByteArray): String = Base64.encodeToString(bytes, Base64.NO_WRAP)

    /** Failures that mean "the stored value can never be decrypted", as opposed to a transient one. */
    private fun isUnreadable(t: Throwable): Boolean =
        t is UnreadableEntry ||
            t is AEADBadTagException ||
            t is KeyPermanentlyInvalidatedException ||
            t is UnrecoverableKeyException ||
            // Base64.decode rejects a malformed value with IllegalArgumentException.
            t is IllegalArgumentException

    private fun describe(t: Throwable): String = t.javaClass.simpleName + (t.message?.let { ": $it" } ?: "")

    /** Runs `body`; null = success, otherwise the failure detail (logged without any secret). */
    private inline fun guarded(operation: String, body: () -> Unit): String? {
        return try {
            body()
            null
        } catch (t: Throwable) {
            val detail = describe(t)
            Log.w(TAG, "credential $operation failed: $detail")
            detail
        }
    }
}
