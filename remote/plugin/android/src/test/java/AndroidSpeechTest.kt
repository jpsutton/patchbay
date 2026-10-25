package dev.patchbay.remote.platform

import android.os.Bundle
import android.os.Looper
import android.speech.tts.TextToSpeech
import android.speech.tts.Voice
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.Implementation
import org.robolectric.annotation.Implements
import org.robolectric.annotation.LooperMode
import org.robolectric.shadow.api.Shadow
import org.robolectric.shadows.ShadowTextToSpeech
import java.time.Duration
import java.util.Locale

/** Real AndroidSpeech with a controllable Android engine, including async completion. */
@Implements(TextToSpeech::class)
class ControlledSpeech : ShadowTextToSpeech() {
    var speed = 0f
    var spoken: String? = null
    var utterance: String? = null
    var result = TextToSpeech.SUCCESS
    @Implementation
    fun setSpeechRate(rate: Float): Int { speed = rate; return TextToSpeech.SUCCESS }
    @Implementation
    public override fun speak(text: CharSequence, mode: Int, params: Bundle?, id: String): Int {
        spoken = text.toString(); utterance = id
        return result
    }
}

@RunWith(RobolectricTestRunner::class)
// Every integer API level from the app minimum through Robolectric’s supported maximum.
@Config(sdk = [26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37], shadows = [ControlledSpeech::class], manifest = Config.NONE)
@LooperMode(LooperMode.Mode.PAUSED)
class AndroidSpeechTest {
    private lateinit var speech: AndroidSpeech
    private val english = Voice("english", Locale.US, Voice.QUALITY_HIGH, Voice.LATENCY_NORMAL, false, emptySet())
    private val alternate = Voice("alternate", Locale.UK, Voice.QUALITY_HIGH, Voice.LATENCY_NORMAL, false, emptySet())
    private fun idle() { shadowOf(Looper.getMainLooper()).idle() }
    private fun engine(): TextToSpeech = ShadowTextToSpeech.getLastTextToSpeechInstance()
    private fun shadow(): ControlledSpeech = Shadow.extract(engine())
    private fun initialize(status: Int = TextToSpeech.SUCCESS) {
        engine().voice = english
        shadow().onInitListener.onInit(status)
        idle()
    }
    @Before fun setup() {
        ShadowTextToSpeech.addVoice(english)
        ShadowTextToSpeech.addVoice(alternate)
        speech = AndroidSpeech(RuntimeEnvironment.getApplication())
    }
    @Test fun initializationWaitsAndListsInstalledVoices() {
        val missing = Voice("missing", Locale.US, Voice.QUALITY_NORMAL, Voice.LATENCY_NORMAL, false,
            setOf(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED))
        ShadowTextToSpeech.addVoice(missing)
        var answer: Result<List<ReadingVoice>>? = null
        speech.voices { answer = it }; idle()
        assertNull(answer)
        initialize()
        assertEquals(setOf("english", "alternate"), answer!!.getOrThrow().map { it.id }.toSet())
    }
    @Test fun speaksUntilCompletionAndResetsDefaultVoice() {
        var finished = false
        speech.speak("Hello Pixel", alternate.name, "faster") { finished = it.isSuccess }; idle()
        initialize()
        assertFalse(finished)
        assertEquals("Hello Pixel", shadow().spoken)
        assertEquals(alternate, engine().voice)
        assertEquals(1.15f, shadow().speed)
        shadow().utteranceProgressListener.onDone(shadow().utterance); idle()
        assertTrue(finished)
        speech.speak("Default again", "", "slower") {}; idle()
        assertEquals(english, engine().voice)
        assertEquals(0.85f, shadow().speed)
    }
    @Test fun stopDuringInitializationNeverSpeaksLater() {
        var replies = 0
        speech.speak("Must not play", "", "normal") { replies++ }; idle()
        speech.stop(); idle()
        assertEquals(1, replies)
        initialize()
        assertNull(shadow().spoken)
        assertEquals(1, replies)
    }
    @Test fun stopDuringPlaybackSettlesOnceAndIgnoresLateCompletion() {
        var replies = 0
        speech.speak("Stop me", "", "normal") { replies++ }; idle(); initialize()
        speech.stop(); idle()
        assertTrue(shadow().isStopped)
        assertEquals(1, replies)
        shadow().utteranceProgressListener.onDone(shadow().utterance); idle()
        assertEquals(1, replies)
    }
    @Test fun initializationFailureAndTimeoutAreVisibleAndCanRetry() {
        var answer: Result<List<ReadingVoice>>? = null
        speech.voices { answer = it }; idle(); initialize(TextToSpeech.ERROR)
        assertTrue(answer!!.isFailure)
        assertTrue(shadow().isShutdown)
        answer = null
        speech.voices { answer = it }; idle()
        shadowOf(Looper.getMainLooper()).idleFor(Duration.ofSeconds(11))
        assertTrue(answer!!.exceptionOrNull()!!.message!!.contains("timed out"))
        speech.voices { answer = it }; idle(); initialize()
        assertTrue(answer!!.isSuccess)
    }
    @Test fun rejectsMissingVoiceAndReportsPlaybackError() {
        var answer: Result<Unit>? = null
        speech.speak("Hello", "removed-voice", "normal") { answer = it }; idle(); initialize()
        assertTrue(answer!!.isFailure)
        assertNull(shadow().spoken)
        speech.speak("Hello", "", "normal") { answer = it }; idle()
        shadow().utteranceProgressListener.onError(shadow().utterance, TextToSpeech.ERROR_SYNTHESIS); idle()
        assertTrue(answer!!.isFailure)
    }
    @Test fun engineRejectionAndMissingCompletionAreBounded() {
        speech.voices {}; idle(); initialize()
        shadow().result = TextToSpeech.ERROR
        var answer: Result<Unit>? = null
        speech.speak("Rejected", "", "normal") { answer = it }; idle()
        assertTrue(answer!!.isFailure)
        shadow().result = TextToSpeech.SUCCESS
        answer = null
        speech.speak("Never completes", "", "normal") { answer = it }; idle()
        assertNull(answer)
        shadowOf(Looper.getMainLooper()).idleFor(Duration.ofSeconds(121))
        assertTrue(answer!!.exceptionOrNull()!!.message!!.contains("timed out"))
    }
    @Test fun destroyShutsDownAndRejectsNewWork() {
        speech.voices {}; idle(); initialize()
        speech.destroy(); idle()
        assertTrue(shadow().isShutdown)
        var answer: Result<List<ReadingVoice>>? = null
        speech.voices { answer = it }; idle()
        assertTrue(answer!!.isFailure)
    }
    @Test fun noInstalledVoicesFailsClearly() {
        ShadowTextToSpeech.reset()
        var answer: Result<List<ReadingVoice>>? = null
        speech.voices { answer = it }; idle(); initialize()
        assertTrue(answer!!.exceptionOrNull()!!.message!!.contains("No Android voices"))
    }
    @Test fun networkVoicesAreLabelled() {
        ShadowTextToSpeech.addVoice(Voice("online", Locale.US, Voice.QUALITY_HIGH,
            Voice.LATENCY_NORMAL, true, emptySet()))
        var answer: Result<List<ReadingVoice>>? = null
        speech.voices { answer = it }; idle(); initialize()
        assertTrue(answer!!.getOrThrow().first { it.id == "online" }.name.endsWith("(network)"))
    }
    @Test fun nativeStopCallbackSettlesTheUtterance() {
        var replies = 0
        speech.speak("Interrupted by engine", "", "normal") { replies++ }; idle(); initialize()
        shadow().utteranceProgressListener.onStop(shadow().utterance, true); idle()
        assertEquals(1, replies)
    }
    @Test fun lateInitializationFromAnOldEngineCannotInitializeItsReplacement() {
        speech.voices {}; idle()
        val old = shadow()
        shadowOf(Looper.getMainLooper()).idleFor(Duration.ofSeconds(11))
        var answer: Result<List<ReadingVoice>>? = null
        speech.voices { answer = it }; idle()
        old.onInitListener.onInit(TextToSpeech.SUCCESS); idle()
        assertNull(answer)
        initialize()
        assertTrue(answer!!.isSuccess)
    }

}
