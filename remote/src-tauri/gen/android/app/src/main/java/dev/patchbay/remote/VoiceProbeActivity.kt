// THROWAWAY PROBE — remediation plan pb-20261106-08, Phase 0. Do not merge: this activity exists
// only to measure two numbers on David's phone and to prove the RECORD_AUDIO manifest gap.
// Probe 1: is RECORD_AUDIO in the merged manifest, and does the runtime request grant or deny?
// Probe 2: SpeechRecognizer availability, partial results, and speech-end -> final-text latency.

package dev.patchbay.remote

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

class VoiceProbeActivity : Activity() {

    private lateinit var manifestLine: TextView
    private lateinit var permissionLine: TextView
    private lateinit var recognizerLine: TextView
    private lateinit var logView: TextView

    private val log = StringBuilder()
    private var runCount = 0
    private var requestedOnce = false

    private var recognizer: SpeechRecognizer? = null
    private var tStart = 0L
    private var tReady = 0L
    private var tBegin = 0L
    private var tEnd = 0L
    private val partials = mutableListOf<Pair<Long, String>>()

    private val micGranted: Boolean
        get() = checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED

    private fun now() = SystemClock.elapsedRealtime()

    private fun clock() = java.text.SimpleDateFormat("HH:mm:ss", java.util.Locale.US)
        .format(java.util.Date())

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val pad = (16 * resources.displayMetrics.density).toInt()
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
        }

        root.addView(TextView(this).apply {
            text = "PB Phase 0 voice probe (throwaway)"
            textSize = 18f
            gravity = Gravity.CENTER
        })
        root.addView(TextView(this).apply {
            text = "${Build.MANUFACTURER} ${Build.MODEL} · Android ${Build.VERSION.RELEASE} · SDK ${Build.VERSION.SDK_INT}"
            gravity = Gravity.CENTER
        })

        manifestLine = line(root, "manifest: …")
        permissionLine = line(root, "permission: …")
        recognizerLine = line(root, "recognizer: …")

        root.addView(Button(this).apply {
            text = "1. Request mic permission"
            setOnClickListener { requestMic() }
        })
        root.addView(Button(this).apply {
            text = "2. Run SpeechRecognizer"
            setOnClickListener { runRecognizer() }
        })
        root.addView(Button(this).apply {
            text = "Copy log"
            setOnClickListener { copyLog() }
        })

        val scroll = ScrollView(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f
            )
        }
        logView = TextView(this).apply {
            typeface = android.graphics.Typeface.MONOSPACE
            textSize = 12f
            setTextIsSelectable(true)
        }
        scroll.addView(logView)
        root.addView(scroll)

        setContentView(root)
        refreshStatus()
        appendLog("device ${Build.MANUFACTURER} ${Build.MODEL}, Android ${Build.VERSION.RELEASE} (SDK ${Build.VERSION.SDK_INT})")
        appendLog("RECORD_AUDIO declared in merged manifest: ${declaredInManifest()}")
        appendLog("RECORD_AUDIO granted right now: $micGranted")
        appendLog("SpeechRecognizer.isRecognitionAvailable: ${SpeechRecognizer.isRecognitionAvailable(this)}")
    }

    private fun line(parent: LinearLayout, label: String): TextView {
        val t = TextView(this).apply { text = label; textSize = 14f }
        parent.addView(t)
        return t
    }

    private fun declaredInManifest(): Boolean {
        val info = packageManager.getPackageInfo(packageName, PackageManager.GET_PERMISSIONS)
        return info.requestedPermissions?.contains(android.Manifest.permission.RECORD_AUDIO) == true
    }

    private fun refreshStatus() {
        manifestLine.text = "manifest: RECORD_AUDIO ${if (declaredInManifest()) "PRESENT" else "ABSENT"} (MODIFY_AUDIO_SETTINGS ${if (packageManager.getPackageInfo(packageName, PackageManager.GET_PERMISSIONS).requestedPermissions?.contains(android.Manifest.permission.MODIFY_AUDIO_SETTINGS) == true) "PRESENT" else "ABSENT"})"
        permissionLine.text = "permission: " + when {
            micGranted -> "GRANTED"
            requestedOnce -> "DENIED"
            else -> "not yet requested"
        }
        val avail = SpeechRecognizer.isRecognitionAvailable(this)
        recognizerLine.text = "recognizer: isRecognitionAvailable=$avail"
    }

    // ---- probe 1: the runtime permission ----

    private val REQ_MIC = 4201

    private fun requestMic() {
        appendLog("--- permission request at ${clock()} ---")
        appendLog("RECORD_AUDIO in manifest: ${declaredInManifest()}, granted before request: $micGranted")
        appendLog("shouldShowRequestPermissionRationale: ${shouldShowRequestPermissionRationale(android.Manifest.permission.RECORD_AUDIO)}")
        if (micGranted) {
            appendLog("already granted; re-request anyway to show the system's answer")
        }
        requestedOnce = true
        requestPermissions(arrayOf(android.Manifest.permission.RECORD_AUDIO), REQ_MIC)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int, permissions: Array<out String>, grantResults: IntArray
    ) {
        if (requestCode != REQ_MIC) return
        val r = grantResults.firstOrNull() ?: PackageManager.PERMISSION_DENIED
        val state = if (r == PackageManager.PERMISSION_GRANTED) "GRANTED" else "DENIED"
        val declared = declaredInManifest()
        val note = when {
            r == PackageManager.PERMISSION_GRANTED && declared -> "granted after the system dialog"
            r == PackageManager.PERMISSION_GRANTED -> "granted although RECORD_AUDIO is not declared (unexpected)"
            !declared -> "denied immediately, no dialog shown — RECORD_AUDIO absent from the merged manifest"
            else -> "denied after the dialog"
        }
        appendLog("result at ${clock()}: $state (grantResult=$r; $note)")
        appendLog("granted now: $micGranted")
        refreshStatus()
    }

    // ---- probe 2: SpeechRecognizer timings ----

    private fun runRecognizer() {
        runCount++
        val n = runCount
        partials.clear()
        appendLog("--- run $n at ${clock()} ---")
        appendLog("granted=$micGranted isRecognitionAvailable=${SpeechRecognizer.isRecognitionAvailable(this)}")
        if (!micGranted) {
            appendLog("cannot listen: RECORD_AUDIO not granted (run probe 1 first)")
            refreshStatus()
            return
        }
        if (!SpeechRecognizer.isRecognitionAvailable(this)) {
            appendLog("no speech recognition service on this device — route A is unavailable here")
            refreshStatus()
            return
        }

        recognizer?.destroy()
        tStart = now(); tReady = 0; tBegin = 0; tEnd = 0
        val r = SpeechRecognizer.createSpeechRecognizer(this)
        recognizer = r
        r.setRecognitionListener(object : RecognitionListener {
            override fun onReadyForSpeech(params: Bundle?) { tReady = now() }
            override fun onBeginningOfSpeech() { tBegin = now() }
            override fun onRmsChanged(rmsdB: Float) {}
            override fun onBufferReceived(buffer: ByteArray?) {}
            override fun onEndOfSpeech() { tEnd = now(); refreshStatus() }

            override fun onPartialResults(partialResults: Bundle?) {
                val text = partialResults
                    ?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                    ?.firstOrNull() ?: return
                val t = now()
                partials.add(t to text)
                val vsEnd = if (tEnd > 0) " (end${ms(t - tEnd)} ms)" else " (before speech end)"
                appendLog("  partial @ start+${ms(t - tStart)} ms$vsEnd: \"$text\"")
            }

            override fun onResults(results: Bundle?) {
                val tFinal = now()
                val text = results?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                    ?.firstOrNull() ?: "(no text)"
                appendLog("  start -> ready   ${span(tReady)} ms")
                appendLog("  ready -> begin   ${span2(tReady, tBegin)} ms   (speech start)")
                appendLog("  begin -> end     ${span2(tBegin, tEnd)} ms   (speech end)")
                appendLog("  END -> FINAL     ${if (tEnd > 0) ms(tFinal - tEnd) else "n/a"} ms   <-- final-result latency")
                appendLog("  start -> final   ${ms(tFinal - tStart)} ms")
                appendLog("  partials: ${partials.size}")
                appendLog("  final: \"$text\"")
                appendLog("run $n done")
                refreshStatus()
            }

            override fun onError(error: Int) {
                val tErr = now()
                appendLog(
                    "  ERROR ${errorName(error)} ($error) at start+${ms(tErr - tStart)} ms; " +
                        "end->error ${if (tEnd > 0) ms(tErr - tEnd) else "n/a"} ms"
                )
                appendLog("run $n failed")
                refreshStatus()
            }

            override fun onEvent(eventType: Int, params: Bundle?) {}
        })

        val intent = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
            putExtra(
                RecognizerIntent.EXTRA_LANGUAGE_MODEL,
                RecognizerIntent.LANGUAGE_MODEL_FREE_FORM
            )
            putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
            putExtra(RecognizerIntent.EXTRA_MAX_RESULTS, 1)
        }
        appendLog("startListening at ${clock()}")
        r.startListening(intent)
    }

    private fun span(t: Long): String =
        if (t == 0L || tStart == 0L) "n/a" else ms(t - tStart)

    private fun span2(a: Long, b: Long): String =
        if (a == 0L || b == 0L) "n/a" else ms(b - a)

    private fun ms(d: Long) = d.toString()

    private fun errorName(code: Int) = when (code) {
        SpeechRecognizer.ERROR_AUDIO -> "ERROR_AUDIO"
        SpeechRecognizer.ERROR_CLIENT -> "ERROR_CLIENT"
        SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS -> "ERROR_INSUFFICIENT_PERMISSIONS"
        SpeechRecognizer.ERROR_NETWORK -> "ERROR_NETWORK"
        SpeechRecognizer.ERROR_NETWORK_TIMEOUT -> "ERROR_NETWORK_TIMEOUT"
        SpeechRecognizer.ERROR_NO_MATCH -> "ERROR_NO_MATCH (heard nothing matchable)"
        SpeechRecognizer.ERROR_RECOGNIZER_BUSY -> "ERROR_RECOGNIZER_BUSY"
        SpeechRecognizer.ERROR_SERVER -> "ERROR_SERVER"
        SpeechRecognizer.ERROR_SPEECH_TIMEOUT -> "ERROR_SPEECH_TIMEOUT (no speech detected)"
        else -> "ERROR_$code"
    }

    private fun copyLog() {
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        cm.setPrimaryClip(ClipData.newPlainText("pb-phase0-probe", logView.text))
        Toast.makeText(this, "log copied", Toast.LENGTH_SHORT).show()
    }

    private fun appendLog(s: String) {
        log.append(s).append('\n')
        logView.text = log.toString()
    }

    override fun onDestroy() {
        recognizer?.destroy()
        recognizer = null
        super.onDestroy()
    }
}
