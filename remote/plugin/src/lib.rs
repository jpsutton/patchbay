//! Platform services for Patchbay Remote that need the phone's own APIs: sealing secrets with a
//! key the Android Keystore holds, a foreground service that keeps the gateway connection open
//! while the app is in the background, native text-to-speech, and Android 17's local network permission. Elsewhere (desktop builds, used for testing) sealing
//! is unavailable and keep-alive does nothing; an iOS build would implement the same calls.

use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "dev.patchbay.remote.platform";

pub struct Platform<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(target_os = "android"))]
    _r: std::marker::PhantomData<fn() -> R>,
}

#[cfg(target_os = "android")]
#[derive(serde::Deserialize)]
struct Data {
    data: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct NativeVoice {
    pub id: String,
    pub name: String,
}

impl<R: Runtime> Platform<R> {
    pub async fn speech_voices(&self) -> Result<Vec<NativeVoice>, String> {
        #[cfg(target_os = "android")]
        {
            #[derive(serde::Deserialize)]
            struct Voices {
                voices: Vec<NativeVoice>,
            }
            self.handle
                .run_mobile_plugin_async::<Voices>("listSpeechVoices", serde_json::json!({}))
                .await
                .map(|v| v.voices)
                .map_err(|e| e.to_string())
        }
        #[cfg(not(target_os = "android"))]
        {
            Ok(Vec::new())
        }
    }

    /// Completes when Android finishes this utterance, so the frontend queue stays ordered.
    pub async fn speak(&self, text: &str, voice: &str, rate: &str) -> Result<(), String> {
        #[cfg(target_os = "android")]
        {
            self.handle
                .run_mobile_plugin_async::<serde_json::Value>(
                    "speak",
                    serde_json::json!({ "text": text, "voice": voice, "rate": rate }),
                )
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (text, voice, rate);
            Err("Native phone speech is only available on Android".into())
        }
    }

    pub async fn stop_speech(&self) -> Result<(), String> {
        #[cfg(target_os = "android")]
        {
            self.handle
                .run_mobile_plugin_async::<serde_json::Value>("stopSpeech", serde_json::json!({}))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        #[cfg(not(target_os = "android"))]
        {
            Ok(())
        }
    }

    /// Encrypt `plain` under the Keystore key. `None` where no keystore is available.
    pub fn seal(&self, plain: &str) -> Result<Option<String>, String> {
        #[cfg(target_os = "android")]
        {
            let r: Data = self
                .handle
                .run_mobile_plugin("seal", serde_json::json!({ "data": plain }))
                .map_err(|e| e.to_string())?;
            Ok(Some(r.data))
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = plain;
            Ok(None)
        }
    }

    /// Decrypt what [`seal`](Self::seal) returned.
    pub fn open(&self, sealed: &str) -> Result<String, String> {
        #[cfg(target_os = "android")]
        {
            let r: Data = self
                .handle
                .run_mobile_plugin("open", serde_json::json!({ "data": sealed }))
                .map_err(|e| e.to_string())?;
            Ok(r.data)
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = sealed;
            Err("no keystore on this platform".into())
        }
    }

    /// Start or stop the foreground service; `text` is its notification's line.
    pub fn keep_alive(&self, on: bool, text: &str) -> Result<(), String> {
        #[cfg(target_os = "android")]
        {
            self.handle
                .run_mobile_plugin::<serde_json::Value>(
                    "keepAlive",
                    serde_json::json!({ "on": on, "text": text }),
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (on, text);
            Ok(())
        }
    }

    /// Ask for local network access where the phone requires it (Android 17), waiting for the
    /// answer. True when granted or not needed.
    pub async fn local_network(&self) -> Result<bool, String> {
        #[cfg(target_os = "android")]
        {
            #[derive(serde::Deserialize)]
            struct Answer {
                granted: bool,
            }
            let r: Answer = self
                .handle
                .run_mobile_plugin_async("localNetwork", serde_json::json!({}))
                .await
                .map_err(|e| e.to_string())?;
            Ok(r.granted)
        }
        #[cfg(not(target_os = "android"))]
        {
            Ok(true)
        }
    }
}

pub trait PlatformExt<R: Runtime> {
    fn platform(&self) -> &Platform<R>;
}

impl<R: Runtime, T: Manager<R>> PlatformExt<R> for T {
    fn platform(&self) -> &Platform<R> {
        self.state::<Platform<R>>().inner()
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("remote-platform")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            let p = Platform {
                handle: api.register_android_plugin(PLUGIN_IDENTIFIER, "PlatformPlugin")?,
            };
            #[cfg(not(target_os = "android"))]
            let p = {
                let _ = api;
                Platform::<R> {
                    _r: std::marker::PhantomData,
                }
            };
            app.manage(p);
            Ok(())
        })
        .build()
}
