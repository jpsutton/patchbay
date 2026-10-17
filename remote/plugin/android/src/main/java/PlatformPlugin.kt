package dev.patchbay.remote.platform

import android.app.Activity
import android.content.Intent
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
class DataArgs {
    lateinit var data: String
}

@InvokeArg
class KeepAliveArgs {
    var on: Boolean = false
    var text: String = ""
}

private const val KEY_ALIAS = "patchbay-remote"
private const val IV_LEN = 12

/** Keystore sealing and the keep-alive service, called from the app's Rust code. */
@TauriPlugin
class PlatformPlugin(private val activity: Activity) : Plugin(activity) {

    /** An AES key that never leaves the Keystore, created on first use. */
    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    @Command
    fun seal(invoke: Invoke) {
        val args = invoke.parseArgs(DataArgs::class.java)
        try {
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.ENCRYPT_MODE, key())
            val out = c.iv + c.doFinal(args.data.toByteArray(Charsets.UTF_8))
            invoke.resolve(JSObject().put("data", Base64.encodeToString(out, Base64.NO_WRAP)))
        } catch (e: Exception) {
            invoke.reject("sealing failed: ${e.message}")
        }
    }

    @Command
    fun open(invoke: Invoke) {
        val args = invoke.parseArgs(DataArgs::class.java)
        try {
            val raw = Base64.decode(args.data, Base64.NO_WRAP)
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, raw, 0, IV_LEN))
            val plain = c.doFinal(raw, IV_LEN, raw.size - IV_LEN)
            invoke.resolve(JSObject().put("data", String(plain, Charsets.UTF_8)))
        } catch (e: Exception) {
            invoke.reject("opening failed: ${e.message}")
        }
    }

    @Command
    fun keepAlive(invoke: Invoke) {
        val args = invoke.parseArgs(KeepAliveArgs::class.java)
        val intent = Intent(activity, KeepAliveService::class.java).putExtra(KeepAliveService.EXTRA_TEXT, args.text)
        try {
            if (args.on) {
                ContextCompat.startForegroundService(activity, intent)
            } else {
                activity.stopService(intent)
            }
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("keep-alive failed: ${e.message}")
        }
    }
}
