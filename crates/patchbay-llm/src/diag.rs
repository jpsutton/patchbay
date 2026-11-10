//! Diagnostics for a reply stream that breaks: what the error wraps, how far the stream got,
//! and (opt-in) the raw bytes it carried.
//!
//! Set `PATCHBAY_LLM_DEBUG=1` to keep the raw bytes of every reply that fails, in
//! `<temp dir>/patchbay-llm-debug/`. Set it to a folder path to use that folder instead. A
//! reply that succeeds or is cancelled leaves nothing behind. The files hold the model's output,
//! so they stay readable by the owner only. Delete them when done.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The environment variable that turns the capture on.
pub const ENV: &str = "PATCHBAY_LLM_DEBUG";

/// How much of an unfinished event an error message quotes.
const TAIL_CHARS: usize = 200;

/// An error and every error it wraps, joined by `: `. `reqwest` reports "error decoding response
/// body" and keeps the real cause (a reset connection, a bad chunk, an HTTP/2 error) one level down.
pub fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut next = e.source();
    while let Some(s) = next {
        let text = s.to_string();
        // Some errors print their source as well; don't say it twice.
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        next = s.source();
    }
    out
}

/// The last `max` characters of `s`, with a marker when something was cut.
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let kept: String = s.chars().skip(n - max).collect();
    format!("…{kept}")
}

/// What `PATCHBAY_LLM_DEBUG` asks for: no folder when it is unset, empty or off.
fn dir_from(value: Option<&str>) -> Option<PathBuf> {
    match value?.trim() {
        "" | "0" | "false" | "off" => None,
        "1" | "true" | "on" => Some(std::env::temp_dir().join("patchbay-llm-debug")),
        path => Some(PathBuf::from(path)),
    }
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> &'a str {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("none")
}

/// The facts of a response that decide how its body is framed and decoded.
fn summary(resp: &reqwest::Response) -> String {
    format!(
        "{:?}, content-encoding {}, transfer-encoding {}, content-type {}",
        resp.version(),
        header(resp, "content-encoding"),
        header(resp, "transfer-encoding"),
        header(resp, "content-type"),
    )
}

/// The top of a capture file: where the reply came from, then every response header.
fn head(url: &str, resp: &reqwest::Response) -> String {
    let mut out = String::new();
    // A query string could carry a key.
    let _ = writeln!(out, "url: {}", url.split('?').next().unwrap_or(url));
    let _ = writeln!(out, "status: {}", resp.status());
    let _ = writeln!(out, "version: {:?}", resp.version());
    for (name, value) in resp.headers() {
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        let _ = writeln!(out, "{name}: {}", value.to_str().unwrap_or("<not text>"));
    }
    out.push_str("\n--- body ---\n");
    out
}

/// The raw bytes of one reply, written to a file as they arrive. Dropping it deletes the file,
/// unless [`Capture::keep`] ran: only a reply that went wrong is worth keeping.
struct Capture {
    /// `None` once a write failed; the capture then stops.
    file: Option<File>,
    path: PathBuf,
    chunks: u64,
    bytes: u64,
    kept: bool,
}

impl Capture {
    fn open(dir: &Path, head: &str) -> Option<Capture> {
        if let Err(e) = fs::create_dir_all(dir) {
            tracing::warn!("cannot create {}: {e}", dir.display());
            return None;
        }
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let path = dir.join(format!("{ms}-{}-{}.sse", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("cannot create {}: {e}", path.display());
                return None;
            }
        };
        if let Err(e) = file.write_all(head.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", path.display());
            drop(file);
            let _ = fs::remove_file(&path);
            return None;
        }
        Some(Capture { file: Some(file), path, chunks: 0, bytes: 0, kept: false })
    }

    fn chunk(&mut self, bytes: &[u8]) {
        self.chunks += 1;
        self.bytes += bytes.len() as u64;
        let failed = match &mut self.file {
            Some(f) => f.write_all(bytes).err(),
            None => None,
        };
        if let Some(e) = failed {
            tracing::warn!("cannot write {}: {e}", self.path.display());
            self.file = None;
        }
    }

    /// Mark the capture as worth keeping and end it with a line saying why.
    fn keep(&mut self, why: &str) -> &Path {
        if !self.kept {
            self.kept = true;
            let (chunks, bytes) = (self.chunks, self.bytes);
            if let Some(f) = &mut self.file {
                let _ = write!(f, "\n--- patchbay: {why}; {chunks} chunks, {bytes} bytes before this ---\n");
            }
        }
        &self.path
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.kept {
            // Close the file first: Windows won't delete an open one.
            self.file = None;
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// What one reply stream has done so far, for the message when it fails.
pub struct Stream {
    info: String,
    started: Instant,
    chunks: u64,
    bytes: u64,
    capture: Option<Capture>,
}

impl Stream {
    pub fn new(url: &str, resp: &reqwest::Response) -> Stream {
        let value = std::env::var(ENV).ok();
        let capture = dir_from(value.as_deref()).and_then(|dir| Capture::open(&dir, &head(url, resp)));
        Stream { info: summary(resp), started: Instant::now(), chunks: 0, bytes: 0, capture }
    }

    pub fn chunk(&mut self, bytes: &[u8]) {
        self.chunks += 1;
        self.bytes += bytes.len() as u64;
        if let Some(c) = &mut self.capture {
            c.chunk(bytes);
        }
    }

    /// Keep the capture, if there is one, for a reply that failed in a way the caller reports itself.
    pub fn keep(&mut self, why: &str) {
        if let Some(c) = &mut self.capture {
            let path = c.keep(why).to_path_buf();
            tracing::warn!("{why}; raw reply kept in {}", path.display());
        }
    }

    /// The error message for a stream that broke: `why`, then how far the stream got, then the
    /// start of the event it was in the middle of (`pending`). Keeps the capture, if there is one.
    pub fn failure(&mut self, why: &str, pending: &str) -> String {
        let mut detail = format!(
            "{}; {} chunks, {} bytes in {} ms",
            self.info,
            self.chunks,
            self.bytes,
            self.started.elapsed().as_millis()
        );
        let pending = tail(pending, TAIL_CHARS);
        if !pending.is_empty() {
            let _ = write!(detail, "; unfinished event {pending:?}");
        }
        if let Some(c) = &mut self.capture {
            let _ = write!(detail, "; raw reply kept in {}", c.keep(why).display());
        }
        let message = format!("{why} ({detail})");
        tracing::warn!("{message}");
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("patchbay-llm-diag-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_chain_names_every_cause() {
        let e = Layer("error decoding response body", Some(Box::new(Layer("connection reset", None))));
        assert_eq!(chain(&e), "error decoding response body: connection reset");
        assert_eq!(chain(&Layer("alone", None)), "alone");
    }

    #[test]
    fn a_cause_the_error_already_prints_is_not_repeated() {
        let e = Layer("read failed: connection reset", Some(Box::new(Layer("connection reset", None))));
        assert_eq!(chain(&e), "read failed: connection reset");
    }

    #[test]
    fn the_tail_keeps_the_end_and_counts_characters() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("abcdef", 3), "…def");
        assert_eq!(tail("ééééé", 2), "…éé", "a cut never lands inside a character");
    }

    #[test]
    fn the_variable_picks_the_folder() {
        assert_eq!(dir_from(None), None);
        for off in ["", "  ", "0", "false", "off"] {
            assert_eq!(dir_from(Some(off)), None, "{off:?}");
        }
        for on in ["1", "true", "on"] {
            assert_eq!(dir_from(Some(on)), Some(std::env::temp_dir().join("patchbay-llm-debug")), "{on:?}");
        }
        assert_eq!(dir_from(Some(" /var/tmp/llm ")), Some(PathBuf::from("/var/tmp/llm")));
    }

    #[test]
    fn a_reply_that_went_wrong_keeps_its_bytes() {
        let dir = scratch("kept");
        let mut c = Capture::open(&dir, "status: 200\n\n--- body ---\n").expect("capture");
        c.chunk(b"data: {\"a\":1}\n\n");
        c.chunk(b"data: {\"b\"");
        let path = c.keep("stream error: reset").to_path_buf();
        drop(c);
        let text = fs::read_to_string(&path).expect("kept file");
        assert!(text.starts_with("status: 200\n"), "{text}");
        assert!(text.contains("data: {\"a\":1}\n\ndata: {\"b\""), "{text}");
        assert!(text.contains("--- patchbay: stream error: reset; 2 chunks, 25 bytes before this ---"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reply_that_finished_leaves_nothing() {
        let dir = scratch("gone");
        let c = Capture::open(&dir, "status: 200\n\n--- body ---\n").expect("capture");
        let path = c.path.clone();
        assert!(path.exists());
        drop(c);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_failure_message_says_how_far_the_stream_got() {
        let mut s = Stream { info: "HTTP/2.0, content-encoding none".into(), started: Instant::now(), chunks: 0, bytes: 0, capture: None };
        s.chunk(b"data: 1\n\n");
        s.chunk(b"data: 2");
        let m = s.failure("stream error: error decoding response body: reset", "{\"x\":");
        assert!(m.starts_with("stream error: error decoding response body: reset (HTTP/2.0, content-encoding none; 2 chunks, 16 bytes in "), "{m}");
        assert!(m.ends_with("; unfinished event \"{\\\"x\\\":\")"), "{m}");
        let m = s.failure("no data for 180s", "");
        assert!(!m.contains("unfinished"), "{m}");
    }

    #[test]
    fn the_failure_message_names_the_kept_file() {
        let dir = scratch("named");
        let capture = Capture::open(&dir, "\n--- body ---\n");
        let path = capture.as_ref().map(|c| c.path.clone()).expect("capture");
        let mut s = Stream { info: "i".into(), started: Instant::now(), chunks: 0, bytes: 0, capture };
        let m = s.failure("stream error: x", "");
        assert!(m.contains(&format!("raw reply kept in {}", path.display())), "{m}");
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
