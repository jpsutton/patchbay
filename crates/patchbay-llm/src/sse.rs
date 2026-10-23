//! A minimal server-sent-events decoder.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub name: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct Sse {
    buf: Vec<u8>,
    name: Option<String>,
    data: Vec<String>,
}

impl Sse {
    /// Feed bytes; returns every event completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(SseEvent {
                        name: self.name.take(),
                        data: self.data.join("\n"),
                    });
                    self.data.clear();
                }
                self.name = None;
            } else if line.starts_with(':') {
                // comment / keep-alive
            } else {
                let (field, value) = line.split_once(':').unwrap_or((&line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => self.name = Some(value.to_owned()),
                    "data" => self.data.push(value.to_owned()),
                    _ => {}
                }
            }
        }
        out
    }

    /// A stream that ends without a trailing blank line still delivers its last event.
    pub fn flush(&mut self) -> Option<SseEvent> {
        if !self.buf.is_empty() {
            self.buf.push(b'\n');
            let mut evs = self.push(&[]);
            if !evs.is_empty() {
                return Some(evs.remove(0));
            }
        }
        if self.data.is_empty() {
            return None;
        }
        let ev = SseEvent {
            name: self.name.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        Some(ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut s = Sse::default();
        assert!(s.push(b"event: a\ndata: {\"x\"").is_empty());
        let evs = s.push(b":1}\n\ndata: two\r\ndata: lines\r\n\r\n: ping\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].name.as_deref(), Some("a"));
        assert_eq!(evs[0].data, "{\"x\":1}");
        assert_eq!(evs[1].data, "two\nlines");
    }

    #[test]
    fn flush_delivers_unterminated_event() {
        let mut s = Sse::default();
        assert!(s.push(b"data: tail").is_empty());
        assert_eq!(s.flush().unwrap().data, "tail");
    }
}
