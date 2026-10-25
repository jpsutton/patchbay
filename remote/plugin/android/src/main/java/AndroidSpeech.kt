package dev.patchbay.remote.platform

import android.content.Context
import android.media.AudioAttributes
import android.os.Handler
import android.os.Looper
import android.speech.tts.TextToSpeech
import android.speech.tts.UtteranceProgressListener
import android.speech.tts.Voice
import java.util.Locale
import java.util.UUID

internal data class ReadingVoice(val id: String, val name: String)

/** Owns one engine connection. All state and replies are serialized on the main looper. */
internal class AndroidSpeech(private val context: Context) {
    private val main = Handler(Looper.getMainLooper())
    private var engine: TextToSpeech? = null
    private var ready = false
    private var destroyed = false
    private var defaultVoice: Voice? = null
    private val waiting = mutableListOf<Pair<(TextToSpeech) -> Unit, (String) -> Unit>>()
    private val utterances = mutableMapOf<String, (Result<Unit>) -> Unit>()
    private val deadlines = mutableMapOf<String, Runnable>()
    private var initGeneration = 0
    private var initDeadline: Runnable? = null

    fun voices(reply: (Result<List<ReadingVoice>>) -> Unit) = main.post {
        withEngine({ tts ->
            val voices = availableVoices(tts).sortedWith(compareBy({ it.locale.toLanguageTag() }, { it.name }))
                .map { ReadingVoice(it.name, "${it.locale.displayName} · ${it.name}${if (it.isNetworkConnectionRequired) " (network)" else ""}") }
            if (voices.isEmpty()) reply(Result.failure(IllegalStateException("No Android voices are installed. Download voice data in Android text-to-speech settings.")))
            else reply(Result.success(voices))
        }, { reply(Result.failure(IllegalStateException(it))) })
    }

    fun speak(text: String, voice: String, rate: String, reply: (Result<Unit>) -> Unit) = main.post {
        if (text.isBlank() || text.length > TextToSpeech.getMaxSpeechInputLength()) {
            reply(Result.failure(IllegalArgumentException("The passage must contain text within Android’s speech length limit.")))
            return@post
        }
        val id = UUID.randomUUID().toString()
        utterances[id] = reply
        withEngine({ tts ->
            // Stop may have arrived while the engine was initializing.
            if (!utterances.containsKey(id)) return@withEngine
            val selected = if (voice.isEmpty()) defaultVoice else availableVoices(tts).find { it.name == voice }
            if (selected == null || tts.setVoice(selected) == TextToSpeech.ERROR) {
                finish(id, "The Android voice is unavailable. Select an installed voice in Settings.")
                return@withEngine
            }
            val speed = when (rate) { "slower" -> 0.85f; "faster" -> 1.15f; else -> 1f }
            if (tts.setSpeechRate(speed) == TextToSpeech.ERROR) {
                finish(id, "Android could not set the reading speed.")
                return@withEngine
            }
            val deadline = Runnable {
                finish(id, "Android speech timed out. Check the selected engine and voice data.")
                tts.stop()
            }
            deadlines[id] = deadline
            main.postDelayed(deadline, 120_000)
            if (tts.speak(text, TextToSpeech.QUEUE_ADD, null, id) == TextToSpeech.ERROR) {
                finish(id, "Android could not start speech. Check the selected engine and voice data.")
            }
        }, { finish(id, it) })
    }

    fun stop(done: () -> Unit = {}) = main.post {
        engine?.stop()
        // onStop is not guaranteed for utterances still waiting for initialization.
        utterances.keys.toList().forEach { finish(it) }
        done()
    }

    fun destroy() = main.post {
        destroyed = true
        failInitialization("Android speech closed.")
        utterances.keys.toList().forEach { finish(it) }
    }

    private fun availableVoices(tts: TextToSpeech): List<Voice> =
        tts.voices.orEmpty().filter { !it.features.orEmpty().contains(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED) }

    private fun withEngine(use: (TextToSpeech) -> Unit, fail: (String) -> Unit) {
        if (destroyed) { fail("Android speech closed."); return }
        val current = engine
        if (ready && current != null) { use(current); return }
        waiting.add(use to fail)
        if (current != null) return
        try {
            // Always post: Android may call onInit before the constructor returns on failure.
            val generation = ++initGeneration
            val created = TextToSpeech(context) { status -> main.post {
                if (generation == initGeneration) initialized(status)
            } }
            engine = created
            val deadline = Runnable { failInitialization("Android speech initialization timed out. Check Android text-to-speech settings.") }
            initDeadline = deadline
            main.postDelayed(deadline, 10_000)
        } catch (error: Exception) {
            failInitialization("Could not initialize Android speech: ${error.message}")
        }
    }

    private fun initialized(status: Int) {
        val tts = engine ?: return
        if (destroyed || ready) return
        initDeadline?.let { main.removeCallbacks(it) }
        initDeadline = null
        if (status != TextToSpeech.SUCCESS) {
            failInitialization("Android speech engine failed to initialize. Check Android text-to-speech settings.")
            return
        }
        if (tts.voice == null) tts.setLanguage(Locale.getDefault())
        defaultVoice = tts.voice
        tts.setAudioAttributes(AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH).build())
        tts.setOnUtteranceProgressListener(object : UtteranceProgressListener() {
            override fun onStart(utteranceId: String?) {}
            override fun onDone(utteranceId: String?) { main.post { finish(utteranceId) } }
            override fun onError(utteranceId: String?) { main.post { finish(utteranceId, "Android speech failed. Check the voice data.") } }
            override fun onError(utteranceId: String?, errorCode: Int) { main.post { finish(utteranceId, "Android speech failed (code $errorCode). Check the voice data or connection.") } }
            override fun onStop(utteranceId: String?, interrupted: Boolean) { main.post { finish(utteranceId) } }
        })
        ready = true
        val jobs = waiting.toList()
        waiting.clear()
        jobs.forEach { it.first(tts) }
    }

    private fun failInitialization(message: String) {
        initGeneration += 1
        initDeadline?.let { main.removeCallbacks(it) }
        initDeadline = null
        ready = false
        engine?.shutdown()
        engine = null
        defaultVoice = null
        val jobs = waiting.toList()
        waiting.clear()
        jobs.forEach { it.second(message) }
    }

    private fun finish(id: String?, error: String? = null) {
        deadlines.remove(id)?.let { main.removeCallbacks(it) }
        val reply = utterances.remove(id) ?: return
        reply(if (error == null) Result.success(Unit) else Result.failure(IllegalStateException(error)))
    }
}
