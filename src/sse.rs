//! Bounded, incremental Server-Sent Events framing.
//!
//! Both translated transports read an upstream SSE body that arrives in
//! arbitrary chunks: a line, an event, or a multi-byte UTF-8 sequence may be
//! split across reads. [`SseFramer`] buffers only the current event, rejects
//! oversized events instead of growing without bound, and reports an upstream
//! that ends mid-event so a truncated stream can never look complete.

/// Default ceiling for one framed event (its `data` payload plus field names).
pub const DEFAULT_MAX_EVENT_BYTES: usize = 1024 * 1024;

/// One dispatched SSE event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if the upstream sent one.
    pub event: Option<String>,
    /// `data:` lines joined with `\n`, without the trailing newline.
    pub data: String,
}

#[derive(Debug)]
pub struct SseFramer {
    max_event_bytes: usize,
    partial: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
    /// Set once the framer reported a framing error; the stream is unusable.
    poisoned: bool,
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_EVENT_BYTES)
    }
}

impl SseFramer {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            partial: Vec::new(),
            event: None,
            data: None,
            poisoned: false,
        }
    }

    /// Feed one upstream chunk, returning every event completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        if self.poisoned {
            return Err("the upstream SSE stream is no longer readable".to_owned());
        }
        let mut events = Vec::new();
        // Complete the line buffered from the previous read, if any.
        let mut buffer = std::mem::take(&mut self.partial);
        buffer.extend_from_slice(chunk);
        let mut offset = 0;
        while let Some(index) = buffer[offset..].iter().position(|byte| *byte == b'\n') {
            let end = offset + index;
            let line = &buffer[offset..end];
            offset = end + 1;
            if self.push_line(line, &mut events).is_err() {
                self.poisoned = true;
                return Err(format!(
                    "the upstream sent an SSE event larger than {} bytes",
                    self.max_event_bytes
                ));
            }
        }
        let rest = &buffer[offset..];
        if rest.len() > self.max_event_bytes {
            self.poisoned = true;
            return Err(format!(
                "the upstream sent an SSE event larger than {} bytes",
                self.max_event_bytes
            ));
        }
        self.partial.extend_from_slice(rest);
        Ok(events)
    }

    /// Finish the stream. Any buffered partial line or undispatched event means
    /// the upstream was truncated mid-event, which is a stream error.
    pub fn finish(&mut self) -> Result<(), String> {
        if !self.partial.is_empty() || self.data.is_some() || self.event.is_some() {
            self.poisoned = true;
            return Err("the upstream SSE stream ended in the middle of an event".to_owned());
        }
        Ok(())
    }

    fn push_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) -> Result<(), ()> {
        let mut line = line;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            return self.dispatch(events);
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(index) => {
                let (field, tail) = line.split_at(index);
                let mut value = &tail[1..];
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                (field, value)
            }
            None => (line, &b""[..]),
        };
        let value = std::str::from_utf8(value).map_err(|_| ())?;
        match field {
            b"event" => {
                if value.len() > self.max_event_bytes {
                    return Err(());
                }
                self.event = Some(value.to_owned());
            }
            b"data" => {
                let prefix = self.data.as_ref().map_or(0, |data| data.len() + 1);
                if prefix.saturating_add(value.len()) > self.max_event_bytes {
                    return Err(());
                }
                match self.data.as_mut() {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => self.data = Some(value.to_owned()),
                }
            }
            // `id`, `retry`, comments and unknown fields carry no meaning for
            // either translated protocol.
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) -> Result<(), ()> {
        let event = self.event.take();
        let Some(data) = self.data.take() else {
            // A bare blank line is a keepalive, not an event.
            return Ok(());
        };
        events.push(SseEvent { event, data });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn frames_events_split_across_every_byte() {
        let input = "event: message_start\ndata: {\"a\":1}\n\nevent: ping\ndata: {}\r\n\r\n";
        let mut framer = SseFramer::default();
        let mut events = Vec::new();
        for byte in input.as_bytes() {
            events.extend(framer.feed(&[*byte]).unwrap());
        }
        framer.finish().unwrap();
        assert_eq!(
            events,
            [
                frame(Some("message_start"), "{\"a\":1}"),
                frame(Some("ping"), "{}")
            ]
        );
    }

    #[test]
    fn joins_multiline_data_and_ignores_comments_and_unknown_fields() {
        let mut framer = SseFramer::default();
        let events = framer
            .feed(b": keepalive\nid: 4\nretry: 100\ndata: first\ndata:second\n\n")
            .unwrap();
        assert_eq!(events, [frame(None, "first\nsecond")]);
        framer.finish().unwrap();
    }

    #[test]
    fn data_only_frames_are_supported() {
        let mut framer = SseFramer::default();
        let events = framer.feed(b"data: {\"type\":\"x\"}\n\n").unwrap();
        assert_eq!(events, [frame(None, "{\"type\":\"x\"}")]);
    }

    #[test]
    fn oversized_events_are_rejected_without_unbounded_growth() {
        let mut framer = SseFramer::new(16);
        assert!(framer.feed(b"data: 0123456789abcdefg\n").is_err());
        assert!(framer.feed(b"data: ok\n\n").is_err());
    }

    #[test]
    fn joined_empty_data_lines_count_towards_the_event_size() {
        let mut framer = SseFramer::new(2);
        assert!(framer.feed(b"data:\ndata:\ndata:\ndata:\n").is_err());
    }

    #[test]
    fn end_of_stream_mid_event_is_an_error() {
        for tail in ["data: {", "data: {}\n"] {
            let mut framer = SseFramer::default();
            framer.feed(tail.as_bytes()).unwrap();
            assert!(framer.finish().is_err(), "accepted {tail:?}");
        }
        let mut framer = SseFramer::default();
        framer.feed(b"data: {}\n\n").unwrap();
        framer.finish().unwrap();
    }
}
